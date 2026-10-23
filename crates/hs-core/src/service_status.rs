//! Read-only, bounded HTTP telemetry. The destination comes from PVE's actual
//! static net0 and HomeStack-written port metadata, never from a remote URL.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use hs_model::status::ServiceRates;
use hs_service_status::{Io, MAX_AGE_SECONDS, MAX_BYTES, MAX_FILESYSTEMS, PATH, Report, SCHEMA};
use serde_json::Value;

/// Absence, stale samples, unsupported producers and failed reads all mean
/// unavailable telemetry. They never change lifecycle ownership or health.
pub async fn read(config: &Value, ip: &str) -> Option<Report> {
    let description = config["description"].as_str()?;
    if !description.lines().any(|line| line == "made by homestack")
        || !description.lines().any(|line| line.starts_with("HomeStack service "))
    {
        return None;
    }
    let port: u16 = description
        .lines()
        .find_map(|line| line.strip_prefix("status-port "))?
        .parse()
        .ok()?;
    let address: Ipv4Addr = ip.parse().ok()?;
    if port == 0
        || address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || address.is_broadcast()
        || address.is_link_local()
    {
        return None;
    }
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    let client = CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_millis(300))
                .timeout(Duration::from_millis(800))
                .build()
                .ok()
        })
        .as_ref()?;
    let mut response = client.get(format!("http://{address}:{port}{PATH}")).send().await.ok()?;
    if response.status() != reqwest::StatusCode::OK
        || response.content_length().is_some_and(|len| len > MAX_BYTES as u64)
        || response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)?
            .to_str()
            .ok()?
            .split(';')
            .next()?
            .trim()
            != "application/json"
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len().checked_add(chunk.len())? > MAX_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    let report: Report = serde_json::from_slice(&bytes).ok()?;
    valid(&report).then_some(report)
}

/// A stable error code: short lowercase words, never a raw OS message.
fn code(error: Option<&String>) -> bool {
    error.is_none_or(|code| {
        (1..=64).contains(&code.len()) && code.bytes().all(|byte| byte.is_ascii_lowercase() || byte == b'_')
    })
}

/// Schema 2 with a fresh measurement whose numbers are consistent. Unknown
/// fields were already ignored; a `null` measurement stays unknown.
fn valid(report: &Report) -> bool {
    let (Some(sample_usec), Some(age)) = (report.sample_uptime_usec, report.sample_age_seconds) else {
        return false;
    };
    if report.schema != SCHEMA
        || age > MAX_AGE_SECONDS
        || sample_usec / 1_000_000 > report.uptime_seconds
        || report.filesystems.len() > MAX_FILESYSTEMS
        || !code(report.error.as_ref())
    {
        return false;
    }
    let mut seen = std::collections::BTreeSet::new();
    for fs in &report.filesystems {
        if !fs.mount.starts_with('/')
            || fs.mount.len() > 4096
            || !seen.insert(&fs.mount)
            || fs.filesystem.as_ref().is_some_and(|kind| kind.len() > 64)
            || !code(fs.error.as_ref())
        {
            return false;
        }
        let counts = [fs.total_bytes, fs.used_bytes, fs.free_bytes, fs.available_bytes];
        if fs.mounted && fs.error.is_none() {
            let [Some(total), Some(used), Some(free), Some(available)] = counts else {
                return false;
            };
            if used.checked_add(free) != Some(total) || available > free {
                return false;
            }
        } else if fs.error.is_none() || counts.iter().any(Option::is_some) {
            return false;
        }
    }
    let container = &report.container;
    if !code(container.error.as_ref()) {
        return false;
    }
    if container.error.is_some() {
        return container.cpu.is_none()
            && container.memory.is_none()
            && container.pressure.is_none()
            && container.io.is_none();
    }
    if let Some(cpu) = &container.cpu
        && (cpu
            .capacity_cpus
            .is_some_and(|cpus| !cpus.is_finite() || cpus <= 0.0 || cpus > 65_536.0)
            || cpu.user_usec.checked_add(cpu.system_usec).is_none()
            || cpu
                .throttling
                .as_ref()
                .is_some_and(|throttling| throttling.throttled_periods > throttling.periods))
    {
        return false;
    }
    if let Some(memory) = &container.memory
        && memory.limit_bytes == Some(0)
    {
        return false;
    }
    let line = |line: &hs_service_status::PsiLine| {
        [line.avg10, line.avg60, line.avg300]
            .iter()
            .all(|value| value.is_finite() && (0.0..=100.0).contains(value))
    };
    container.pressure.as_ref().is_none_or(|pressure| {
        [&pressure.cpu, &pressure.memory, &pressure.io]
            .into_iter()
            .flatten()
            .all(|psi| line(&psi.some) && psi.full.as_ref().is_none_or(line))
    })
}

/// The previous measurement of one container's service process.
struct Previous {
    uptime_seconds: u64,
    sample_usec: u64,
    cpu_usec: Option<u64>,
    io: Option<Io>,
    rates: Option<ServiceRates>,
    seen: Instant,
}

/// Measurements closer than this are not a meaningful interval.
const MIN_INTERVAL_USEC: u64 = 1_000_000;
/// Rates over a longer gap are averages, not current activity.
const MAX_INTERVAL_USEC: u64 = 600_000_000;

/// CPU and I/O rates between `report` and the previous valid report of
/// `vmid`'s service. Only status requests call it, so history ages out
/// while no client asks. A restart, a counter that went back or an invalid
/// interval starts a new series without rates.
pub fn rates(vmid: u32, report: &Report) -> Option<ServiceRates> {
    static HISTORY: Mutex<BTreeMap<u32, Previous>> = Mutex::new(BTreeMap::new());
    let sample_usec = report.sample_uptime_usec?;
    let now = Instant::now();
    let mut history = HISTORY.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    history.retain(|_, previous| now.duration_since(previous.seen) < Duration::from_micros(MAX_INTERVAL_USEC));
    let cpu_usec = report.container.cpu.as_ref().map(|cpu| cpu.usage_usec);
    let io = report.container.io.clone();
    if let Some(previous) = history.get_mut(&vmid)
        && previous.sample_usec == sample_usec
        && previous.uptime_seconds <= report.uptime_seconds
    {
        // The service answered from the measurement it already reported.
        previous.seen = now;
        return previous.rates.clone();
    }
    let rates = history.get(&vmid).and_then(|previous| {
        if report.uptime_seconds < previous.uptime_seconds {
            return None;
        }
        let interval = sample_usec.checked_sub(previous.sample_usec)?;
        if !(MIN_INTERVAL_USEC..=MAX_INTERVAL_USEC).contains(&interval) {
            return None;
        }
        let delta = |before: Option<u64>, after: Option<u64>| match (before, after) {
            (Some(before), Some(after)) => after.checked_sub(before).map(Some).ok_or(()),
            _ => Ok(None),
        };
        let field = |pick: fn(&Io) -> u64| delta(previous.io.as_ref().map(pick), io.as_ref().map(pick));
        let cpu = delta(previous.cpu_usec, cpu_usec).ok()?;
        let read_bytes = field(|io| io.read_bytes).ok()?;
        let write_bytes = field(|io| io.write_bytes).ok()?;
        let read_ops = field(|io| io.read_ops).ok()?;
        let write_ops = field(|io| io.write_ops).ok()?;
        let seconds = interval as f64 / 1_000_000.0;
        let per_second = |count: Option<u64>| count.map(|count| count as f64 / seconds);
        Some(ServiceRates {
            interval_seconds: seconds,
            cpu_percent: cpu.map(|usec| usec as f64 * 100.0 / interval as f64),
            read_bytes_per_second: per_second(read_bytes),
            write_bytes_per_second: per_second(write_bytes),
            read_ops_per_second: per_second(read_ops),
            write_ops_per_second: per_second(write_ops),
        })
    });
    history.insert(
        vmid,
        Previous {
            uptime_seconds: report.uptime_seconds,
            sample_usec,
            cpu_usec,
            io,
            rates: rates.clone(),
            seen: now,
        },
    );
    rates
}
