//! Language-neutral service telemetry, schema 3: what only the application
//! process knows. Container resources, filesystems and uptime come from the
//! PVE agent and Proxmox through Prometheus, not from the service. See the
//! homestack-service skill's service-status.md contract.

use serde::{Deserialize, Serialize};

pub const PATH: &str = "/.well-known/homestack/status";
pub const SCHEMA: u32 = 3;
pub const MAX_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Report {
    pub schema: u32,
    /// Process liveness, independent of dependency readiness.
    pub status: Status,
    /// Resident set of the service process alone; `null` when unavailable.
    /// A custom deserializer makes Serde reject the field missing.
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub process_rss_bytes: Option<u64>,
}

fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum Status {
    Pass,
    Fail,
}

/// The report of this process. Reads one small `/proc` file per request and
/// does nothing in between.
pub fn report(status: Status) -> Report {
    Report {
        schema: SCHEMA,
        status,
        process_rss_bytes: process_rss(),
    }
}

fn process_rss() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kib: u64 = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse()
        .ok()?;
    kib.checked_mul(1024)
}
