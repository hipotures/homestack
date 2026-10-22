//! Read-only, bounded HTTP telemetry. The destination comes from PVE's actual
//! static net0 and HomeStack-written port metadata, never from a remote URL.

use std::net::Ipv4Addr;
use std::sync::OnceLock;
use std::time::Duration;

use hs_service_status::{MAX_AGE_SECONDS, MAX_BYTES, MAX_FILESYSTEMS, PATH, Report};
use serde_json::Value;

/// Absence, stale samples, unsupported producers and failed reads all mean
/// unavailable telemetry. They never change lifecycle ownership or health.
pub async fn read(config: &Value, ip: &str) -> Option<Report> {
    let description = config["description"].as_str()?;
    if !description.lines().any(|line| line == "made by homestack")
        || !description.lines().any(|line| line.starts_with("HomeStack service "))
        || !config["tags"].as_str()?.split(';').any(|tag| tag == "homestack-app")
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
    if report.schema != 1
        || report.error.is_some()
        || report.sample_age_seconds? > MAX_AGE_SECONDS
        || report.filesystems.len() > MAX_FILESYSTEMS
    {
        return None;
    }
    let mut seen = std::collections::BTreeSet::new();
    for fs in &report.filesystems {
        if !fs.mount.starts_with('/') || fs.mount.len() > 4096 || !seen.insert(&fs.mount) {
            return None;
        }
        if fs.error.is_none() && fs.mounted {
            let (total, used, free, available) = (fs.total_bytes?, fs.used_bytes?, fs.free_bytes?, fs.available_bytes?);
            if total == 0 || used.checked_add(free)? != total || available > free {
                return None;
            }
        } else if fs.total_bytes.is_some()
            || fs.used_bytes.is_some()
            || fs.free_bytes.is_some()
            || fs.available_bytes.is_some()
        {
            return None;
        }
    }
    if report.load_average.as_ref().is_some_and(|load| {
        load.scope != "system"
            || [load.one, load.five, load.fifteen]
                .iter()
                .any(|v| !v.is_finite() || *v < 0.0)
    }) {
        return None;
    }
    Some(report)
}
