//! Container telemetry from Prometheus, where `homestack-pve-agent` on each
//! PVE node sends cgroup and filesystem measurements of its LXCs. The values
//! keep the shape of the service status contract, so clients read them
//! unchanged; one query per status request covers every container.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use hs_model::status::ServiceRates;
use hs_service_status::{
    Container, Cpu, Filesystem, Io, MAX_AGE_SECONDS, Memory, MemoryEvents, Pressure, Psi, PsiLine, Report, SCHEMA,
    Status, Throttling,
};
use reqwest::Url;
use serde::Deserialize;

/// The window of the CPU and I/O rates.
const RATE_WINDOW_SECONDS: f64 = 60.0;

/// Raw series of every LXC, plus derived ones told apart by a `derived`
/// label: `or` matches label sets without metric names, so a derived series
/// must not share its labels with a raw one.
const QUERY: &str = concat!(
    r#"{__name__=~"hs_pve_(cpu_capacity|cpu_(usage|user|system|throttled)_seconds_total|cpu_(periods|throttled_periods)_total"#,
    r#"|memory_usage_bytes|memory_effective_limit_bytes|pressure_stall_ratio|pressure_stall_seconds_total"#,
    r#"|filesystem_(total|used|free|available)_bytes|filesystem_(mounted|read_success)_ratio)",type="lxc"}"#,
    r#" or hs_pve_memory_stat_bytes{type="lxc",kind=~"anon|file"}"#,
    r#" or hs_pve_memory_events_total{type="lxc",event=~"high|max|oom|oom_kill"}"#,
    r#" or proxmox_vm_uptime_seconds{type="lxc"}"#,
    r#" or label_replace(sum by (node, vmid, operation) (hs_pve_io_bytes_total{type="lxc"}), "derived", "io_bytes", "", "")"#,
    r#" or label_replace(sum by (node, vmid, operation) (hs_pve_io_operations_total{type="lxc"}), "derived", "io_ops", "", "")"#,
    r#" or label_replace(time() - timestamp(hs_pve_memory_usage_bytes{type="lxc"}), "derived", "age", "", "")"#,
    r#" or label_replace(sum by (node, vmid) (rate(hs_pve_cpu_usage_seconds_total{type="lxc"}[1m])), "derived", "cpu_rate", "", "")"#,
    r#" or label_replace(sum by (node, vmid, operation) (rate(hs_pve_io_bytes_total{type="lxc"}[1m])), "derived", "io_bytes_rate", "", "")"#,
    r#" or label_replace(sum by (node, vmid, operation) (rate(hs_pve_io_operations_total{type="lxc"}[1m])), "derived", "io_ops_rate", "", "")"#,
);

#[derive(Deserialize)]
struct Response {
    data: Data,
}

#[derive(Deserialize)]
struct Data {
    result: Vec<Series>,
}

#[derive(Deserialize)]
struct Series {
    metric: BTreeMap<String, String>,
    value: (f64, String),
}

/// One container's series: its metric name, labels and value.
#[derive(Default)]
struct Samples(Vec<(String, BTreeMap<String, String>, f64)>);

impl Samples {
    fn all<'a>(&'a self, name: &'a str, labels: &'a [(&str, &str)]) -> impl Iterator<Item = f64> + 'a {
        self.0
            .iter()
            .filter(move |(series, have, _)| {
                series == name
                    && labels
                        .iter()
                        .all(|(key, value)| have.get(*key).map(String::as_str) == Some(*value))
            })
            .map(|(_, _, value)| *value)
    }

    fn get(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        self.all(name, labels).next()
    }

    fn count(&self, name: &str, labels: &[(&str, &str)]) -> Option<u64> {
        self.get(name, labels).and_then(count)
    }

    /// Microseconds of a counter in seconds.
    fn usec(&self, name: &str) -> Option<u64> {
        self.get(name, &[]).and_then(|seconds| count(seconds * 1_000_000.0))
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "checked finite and non-negative"
)]
fn count(value: f64) -> Option<u64> {
    (value.is_finite() && value >= 0.0).then_some(value as u64)
}

/// Measurements of running LXCs by node and VMID. An unreachable or failing
/// Prometheus, and containers without a fresh sample, have none: telemetry
/// never changes lifecycle ownership or health.
pub async fn read(prometheus: &Url) -> BTreeMap<(String, u32), (Report, Option<ServiceRates>)> {
    let Some(response) = query(prometheus).await else {
        return BTreeMap::new();
    };
    let mut containers: BTreeMap<(String, u32), Samples> = BTreeMap::new();
    for series in response.data.result {
        let (Some(node), Some(vmid)) = (
            series.metric.get("node"),
            series.metric.get("vmid").and_then(|vmid| vmid.parse().ok()),
        ) else {
            continue;
        };
        let Some(name) = series.metric.get("derived").or_else(|| series.metric.get("__name__")) else {
            continue;
        };
        let Ok(value) = series.value.1.parse::<f64>() else {
            continue;
        };
        containers
            .entry((node.clone(), vmid))
            .or_default()
            .0
            .push((name.clone(), series.metric, value));
    }
    containers
        .into_iter()
        .filter_map(|(key, samples)| report(&samples).map(|report| (key, (report, rates(&samples)))))
        .collect()
}

async fn query(prometheus: &Url) -> Option<Response> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    let client = CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_millis(500))
                .timeout(Duration::from_secs(3))
                .build()
                .ok()
        })
        .as_ref()?;
    let mut url = prometheus.clone();
    url.set_path("/api/v1/query");
    url.query_pairs_mut().clear().append_pair("query", QUERY);
    let response = client.get(url).send().await.ok()?;
    if response.status() != reqwest::StatusCode::OK {
        return None;
    }
    response.json().await.ok()
}

/// The container's measurement, when the agent sent one recently.
fn report(samples: &Samples) -> Option<Report> {
    let age = samples.get("age", &[]).and_then(count)?;
    if age > MAX_AGE_SECONDS {
        return None;
    }
    let cpu = match (
        samples.usec("hs_pve_cpu_usage_seconds_total"),
        samples.usec("hs_pve_cpu_user_seconds_total"),
        samples.usec("hs_pve_cpu_system_seconds_total"),
    ) {
        (Some(usage_usec), Some(user_usec), Some(system_usec)) => Some(Cpu {
            capacity_cpus: samples.get("hs_pve_cpu_capacity", &[]),
            usage_usec,
            user_usec,
            system_usec,
            throttling: match (
                samples.count("hs_pve_cpu_periods_total", &[]),
                samples.count("hs_pve_cpu_throttled_periods_total", &[]),
                samples.usec("hs_pve_cpu_throttled_seconds_total"),
            ) {
                (Some(periods), Some(throttled_periods), Some(throttled_usec)) => Some(Throttling {
                    periods,
                    throttled_periods,
                    throttled_usec,
                }),
                _ => None,
            },
        }),
        _ => None,
    };
    let event = |event| samples.count("hs_pve_memory_events_total", &[("event", event)]);
    let memory = samples
        .count("hs_pve_memory_usage_bytes", &[])
        .map(|usage_bytes| Memory {
            usage_bytes,
            // Omitted while the whole hierarchy is unlimited.
            limit_bytes: samples.count("hs_pve_memory_effective_limit_bytes", &[]),
            anon_bytes: samples.count("hs_pve_memory_stat_bytes", &[("kind", "anon")]),
            file_bytes: samples.count("hs_pve_memory_stat_bytes", &[("kind", "file")]),
            // The agent measures the container, not one process in it.
            process_rss_bytes: None,
            events: match (event("high"), event("max"), event("oom"), event("oom_kill")) {
                (Some(high), Some(max), Some(oom), Some(oom_kill)) => Some(MemoryEvents {
                    high,
                    max,
                    oom,
                    oom_kill,
                }),
                _ => None,
            },
        });
    let line = |resource, stall| {
        // The agent sends PSI as ratios; the contract has percent.
        let window = |window| {
            samples
                .get(
                    "hs_pve_pressure_stall_ratio",
                    &[("resource", resource), ("stall", stall), ("window", window)],
                )
                .map(|ratio| ratio * 100.0)
        };
        Some(PsiLine {
            avg10: window("10s")?,
            avg60: window("60s")?,
            avg300: window("300s")?,
            total_usec: samples
                .get(
                    "hs_pve_pressure_stall_seconds_total",
                    &[("resource", resource), ("stall", stall)],
                )
                .and_then(|seconds| count(seconds * 1_000_000.0))?,
        })
    };
    let psi = |resource| {
        Some(Psi {
            some: line(resource, "some")?,
            full: line(resource, "full"),
        })
    };
    let pressure = Pressure {
        cpu: psi("cpu"),
        memory: psi("memory"),
        io: psi("io"),
    };
    // Summed over devices by the query.
    let io = |name, operation| samples.count(name, &[("operation", operation)]);
    let io = match (
        io("io_bytes", "read"),
        io("io_bytes", "write"),
        io("io_ops", "read"),
        io("io_ops", "write"),
    ) {
        (Some(read_bytes), Some(write_bytes), Some(read_ops), Some(write_ops)) => Some(Io {
            read_bytes,
            write_bytes,
            read_ops,
            write_ops,
        }),
        _ => None,
    };
    Some(Report {
        schema: SCHEMA,
        // A running container with a fresh sample; the application's own
        // liveness is its health check's business.
        status: Status::Pass,
        // The container's, not its application process's.
        uptime_seconds: samples.count("proxmox_vm_uptime_seconds", &[]).unwrap_or(0),
        sample_uptime_usec: None,
        sample_age_seconds: Some(age),
        filesystems: filesystems(samples),
        container: Container {
            cpu,
            memory,
            pressure: (pressure.cpu.is_some() || pressure.memory.is_some() || pressure.io.is_some())
                .then_some(pressure),
            io,
            error: None,
        },
        error: None,
    })
}

fn filesystems(samples: &Samples) -> Vec<Filesystem> {
    let mut mounts: BTreeMap<&str, Option<&str>> = BTreeMap::new();
    for (name, labels, _) in &samples.0 {
        if name == "hs_pve_filesystem_mounted_ratio"
            && let Some(mount) = labels.get("mount")
        {
            mounts.insert(mount, labels.get("filesystem").map(String::as_str));
        }
    }
    mounts
        .into_iter()
        .map(|(mount, filesystem)| {
            let at = [("mount", mount)];
            let mounted = samples.get("hs_pve_filesystem_mounted_ratio", &at) == Some(1.0);
            let bytes = |name| samples.count(name, &at);
            let measured = mounted && samples.get("hs_pve_filesystem_read_success_ratio", &at) == Some(1.0);
            let counts = [
                bytes("hs_pve_filesystem_total_bytes"),
                bytes("hs_pve_filesystem_used_bytes"),
                bytes("hs_pve_filesystem_free_bytes"),
                bytes("hs_pve_filesystem_available_bytes"),
            ];
            let [total_bytes, used_bytes, free_bytes, available_bytes] = if measured { counts } else { [None; 4] };
            Filesystem {
                mount: mount.to_owned(),
                filesystem: mounted.then(|| filesystem.map(str::to_owned)).flatten(),
                mounted,
                total_bytes,
                used_bytes,
                free_bytes,
                available_bytes,
                error: if !mounted {
                    Some("not_mounted".to_owned())
                } else if measured && counts.iter().all(Option::is_some) {
                    None
                } else {
                    Some("measurement_unavailable".to_owned())
                },
            }
        })
        .collect()
}

/// CPU and I/O rates over the last minute, when Prometheus has enough samples.
fn rates(samples: &Samples) -> Option<ServiceRates> {
    let rate = |name, labels: &[(&str, &str)]| samples.get(name, labels).filter(|value| value.is_finite());
    let rates = ServiceRates {
        interval_seconds: RATE_WINDOW_SECONDS,
        // CPU seconds per second; 100 is one logical CPU busy.
        cpu_percent: rate("cpu_rate", &[]).map(|cpus| cpus * 100.0),
        read_bytes_per_second: rate("io_bytes_rate", &[("operation", "read")]),
        write_bytes_per_second: rate("io_bytes_rate", &[("operation", "write")]),
        read_ops_per_second: rate("io_ops_rate", &[("operation", "read")]),
        write_ops_per_second: rate("io_ops_rate", &[("operation", "write")]),
    };
    [
        rates.cpu_percent,
        rates.read_bytes_per_second,
        rates.write_bytes_per_second,
        rates.read_ops_per_second,
        rates.write_ops_per_second,
    ]
    .iter()
    .any(Option::is_some)
    .then_some(rates)
}
