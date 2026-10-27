//! What is observed of running machines: `homestack-pve-agent` on each PVE
//! node sends cgroup and filesystem measurements of its LXCs and VMs, and
//! Proxmox their uptime, to Prometheus, read with one query per status
//! request; the application's own liveness and RSS come from its status
//! endpoint. A machine that is not running keeps its last measured space.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::OnceLock;
use std::time::Duration;

use hs_model::status::{ContainerMemory, ContainerTelemetry, HomeUsage, Stalls};
use hs_service_status::{MAX_BYTES, PATH, Report, SCHEMA};
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;

/// Samples older than this are not shown.
const MAX_AGE_SECONDS: u64 = 30;

/// Raw series of every LXC, plus derived ones told apart by a `derived`
/// label: `or` matches label sets without metric names, so a derived series
/// must not share its labels with a raw one.
const QUERY: &str = concat!(
    r#"{__name__=~"hs_pve_(cpu_capacity|memory_usage_bytes|memory_effective_limit_bytes)"}"#,
    r#" or hs_pve_memory_stat_bytes{kind=~"anon|file"}"#,
    r#" or hs_pve_memory_events_total{event="oom_kill"}"#,
    r#" or hs_pve_pressure_stall_ratio{stall="some",window="10s"}"#,
    r#" or {__name__=~"hs_pve_filesystem_(total|used|available)_bytes|hs_pve_filesystem_read_success_ratio","#,
    r#"type="lxc",mount=~"/|/home/app"}"#,
    r#" or proxmox_vm_uptime_seconds"#,
    r#" or label_replace(time() - timestamp(hs_pve_memory_usage_bytes), "derived", "age", "", "")"#,
    r#" or label_replace(sum by (node, vmid) (rate(hs_pve_cpu_usage_seconds_total[1m])), "derived", "cpu_rate", "", "")"#,
    r#" or label_replace(sum by (node, vmid, operation) (rate(hs_pve_io_bytes_total[1m])), "derived", "io_bytes_rate", "", "")"#,
    r#" or label_replace(sum by (node, vmid, operation) (rate(hs_pve_io_operations_total[1m])), "derived", "io_ops_rate", "", "")"#,
);

/// A machine's measurement and, for a container, the `/` and `/home/app`
/// space in it; a VM's space comes from its guest agent.
pub struct Measured {
    pub telemetry: ContainerTelemetry,
    pub root: Option<HomeUsage>,
    pub home: Option<HomeUsage>,
}

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
    fn get(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        self.0
            .iter()
            .find(|(series, have, value)| {
                series == name
                    && value.is_finite()
                    && labels
                        .iter()
                        .all(|(key, value)| have.get(*key).map(String::as_str) == Some(*value))
            })
            .map(|(_, _, value)| *value)
    }

    fn count(&self, name: &str, labels: &[(&str, &str)]) -> Option<u64> {
        self.get(name, labels).and_then(count)
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
pub async fn read(prometheus: &Url) -> BTreeMap<(String, u32), Measured> {
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
        .filter_map(|(key, samples)| measured(&samples).map(|measured| (key, measured)))
        .collect()
}

fn client() -> Option<&'static reqwest::Client> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_millis(500))
                .timeout(Duration::from_secs(3))
                .build()
                .ok()
        })
        .as_ref()
}

async fn query(prometheus: &Url) -> Option<Response> {
    run(prometheus, QUERY).await
}

/// The last space measured in each data filesystem of the machines `vmids`,
/// which are not running, within Prometheus' retention: by VMID and mount.
pub async fn last_space(prometheus: &Url, vmids: &[u32]) -> BTreeMap<u32, BTreeMap<String, HomeUsage>> {
    if vmids.is_empty() {
        return BTreeMap::new();
    }
    let list: Vec<String> = vmids.iter().map(u32::to_string).collect();
    let query = format!(
        r#"last_over_time({{__name__=~"hs_pve_filesystem_(total|used|available)_bytes|hs_pve_filesystem_read_success_ratio",vmid=~"{}",mount!~"/boot(/.*)?|/snap/.*"}}[30d])"#,
        list.join("|")
    );
    let Some(response) = run(prometheus, &query).await else {
        return BTreeMap::new();
    };
    let mut machines: BTreeMap<u32, Samples> = BTreeMap::new();
    for series in response.data.result {
        let (Some(vmid), Some(name), Ok(value)) = (
            series.metric.get("vmid").and_then(|vmid| vmid.parse().ok()),
            series.metric.get("__name__").cloned(),
            series.value.1.parse::<f64>(),
        ) else {
            continue;
        };
        machines.entry(vmid).or_default().0.push((name, series.metric, value));
    }
    machines
        .into_iter()
        .map(|(vmid, samples)| {
            let mounts: std::collections::BTreeSet<String> = samples
                .0
                .iter()
                .filter_map(|(_, labels, _)| labels.get("mount").cloned())
                .collect();
            let spaces = mounts
                .into_iter()
                .filter_map(|mount| space(&samples, &mount).map(|usage| (mount, usage)))
                .collect();
            (vmid, spaces)
        })
        .collect()
}

/// The last `/home/app` of a container, or of a VM the guest filesystem
/// within 15 % of its home disk's `size`.
#[must_use]
pub fn last_home(mounts: &BTreeMap<String, HomeUsage>, container: bool, size: Option<u64>) -> Option<HomeUsage> {
    if container {
        return mounts.get("/home/app").cloned();
    }
    let size = size?;
    #[allow(clippy::cast_precision_loss, reason = "a size comparison")]
    let gap = |total: u64| (total as f64 - size as f64).abs() / size as f64;
    mounts
        .iter()
        .filter(|(mount, usage)| *mount != "/" && usage.size_bytes.is_some_and(|total| gap(total) <= 0.15))
        .min_by(|(_, a), (_, b)| gap(a.size_bytes.unwrap_or(0)).total_cmp(&gap(b.size_bytes.unwrap_or(0))))
        .map(|(_, usage)| usage.clone())
}

async fn run(prometheus: &Url, query: &str) -> Option<Response> {
    let mut url = prometheus.clone();
    url.set_path("/api/v1/query");
    url.query_pairs_mut().clear().append_pair("query", query);
    let response = client()?.get(url).send().await.ok()?;
    if response.status() != reqwest::StatusCode::OK {
        return None;
    }
    response.json().await.ok()
}

/// The container's measurement, when the agent sent one recently.
fn measured(samples: &Samples) -> Option<Measured> {
    let age = samples.count("age", &[])?;
    if age > MAX_AGE_SECONDS {
        return None;
    }
    let memory = samples
        .count("hs_pve_memory_usage_bytes", &[])
        .map(|usage_bytes| ContainerMemory {
            usage_bytes,
            // Omitted while the whole hierarchy is unlimited.
            limit_bytes: samples.count("hs_pve_memory_effective_limit_bytes", &[]),
            anon_bytes: samples.count("hs_pve_memory_stat_bytes", &[("kind", "anon")]),
            file_bytes: samples.count("hs_pve_memory_stat_bytes", &[("kind", "file")]),
            oom_kills: samples.count("hs_pve_memory_events_total", &[("event", "oom_kill")]),
        });
    // The agent sends PSI as ratios.
    let stall = |resource| {
        samples
            .get("hs_pve_pressure_stall_ratio", &[("resource", resource)])
            .map(|ratio| ratio * 100.0)
    };
    // Summed over devices by the query.
    let rate = |name, operation| samples.get(name, &[("operation", operation)]);
    Some(Measured {
        telemetry: ContainerTelemetry {
            sample_age_seconds: age,
            uptime_seconds: samples.count("proxmox_vm_uptime_seconds", &[]),
            capacity_cpus: samples.get("hs_pve_cpu_capacity", &[]),
            cpu_percent: samples.get("cpu_rate", &[]).map(|cpus| cpus * 100.0),
            memory,
            read_bytes_per_second: rate("io_bytes_rate", "read"),
            write_bytes_per_second: rate("io_bytes_rate", "write"),
            read_ops_per_second: rate("io_ops_rate", "read"),
            write_ops_per_second: rate("io_ops_rate", "write"),
            stalls: Stalls {
                cpu: stall("cpu"),
                memory: stall("memory"),
                io: stall("io"),
            },
        },
        root: space(samples, "/"),
        home: space(samples, "/home/app"),
    })
}

/// A mount's space, when the agent read it.
fn space(samples: &Samples, mount: &str) -> Option<HomeUsage> {
    let at = [("mount", mount)];
    if samples.get("hs_pve_filesystem_read_success_ratio", &at) != Some(1.0) {
        return None;
    }
    let total = samples.count("hs_pve_filesystem_total_bytes", &at)?;
    let available = samples.count("hs_pve_filesystem_available_bytes", &at)?;
    #[allow(clippy::cast_precision_loss, reason = "a percentage")]
    let free_percent = (total > 0).then(|| available as f64 * 100.0 / total as f64);
    Some(HomeUsage {
        size_bytes: Some(total),
        used_bytes: samples.count("hs_pve_filesystem_used_bytes", &at),
        free_bytes: Some(available),
        free_percent,
    })
}

/// The application's own report. The destination comes from PVE's actual
/// static net0 and HomeStack-written port metadata, never from a remote URL.
/// Absence, unsupported producers and failed reads all mean unavailable.
pub async fn application(config: &Value, ip: &str) -> Option<Report> {
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
    let mut response = client()?
        .get(format!("http://{address}:{port}{PATH}"))
        .timeout(Duration::from_millis(800))
        .send()
        .await
        .ok()?;
    if response.status() != reqwest::StatusCode::OK
        || response.content_length().is_some_and(|len| len > MAX_BYTES as u64)
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
    (report.schema == SCHEMA).then_some(report)
}
