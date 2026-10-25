//! Read-only statistics for a virtiofs share identified by `HS_HOME_<vmid>`.

use std::time::Duration;

use hs_model::status::HomeUsage;
use serde::Deserialize;

use crate::CoreError;
use crate::guest::{guest_out, shell_join};
use crate::inventory::{StatusResource, VmConfig, home_label};
use crate::pve::Pve;

#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    pub vmid: u32,
    pub node: String,
    pub pool: Option<String>,
    pub config: VmConfig,
}

impl Target {
    #[must_use]
    pub fn of(resource: &StatusResource, config: &VmConfig) -> Option<Self> {
        let expected = home_label(resource.vmid).ok()?;
        let count = config
            .iter()
            .filter(|(key, value)| {
                key.strip_prefix("virtiofs")
                    .is_some_and(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
                    && value
                        .split(',')
                        .any(|field| field.strip_prefix("dirid=").unwrap_or(field) == expected)
            })
            .count();
        (count == 1).then(|| Self {
            vmid: resource.vmid,
            node: resource.node.clone(),
            pool: resource.pool.clone(),
            config: config.clone(),
        })
    }
}

#[derive(Clone)]
pub struct Report {
    pub mount: String,
    pub usage: HomeUsage,
}

#[derive(Deserialize)]
struct Reading {
    tag: String,
    mountpoint: String,
    size_bytes: u64,
    used_bytes: u64,
    free_bytes: u64,
}

pub async fn read(pve: &impl Pve, target: &Target) -> Result<Option<Report>, CoreError> {
    let expected = home_label(target.vmid)?;
    let command = shell_join(&[
        "python3",
        "-I",
        "-B",
        "-c",
        include_str!("virtiofs_usage.py"),
        &expected,
    ]);
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        guest_out(pve, &target.node, target.vmid, &command, true, Duration::from_secs(3)),
    )
    .await
    .map_err(|_| CoreError::Failed("Virtiofs statistics timed out".to_owned()))??;
    let reading: Option<Reading> =
        serde_json::from_str(&output).map_err(|_| CoreError::Unusable("Invalid virtiofs statistics".to_owned()))?;
    let Some(reading) = reading else { return Ok(None) };
    if reading.tag != expected
        || !reading.mountpoint.starts_with('/')
        || reading
            .used_bytes
            .checked_add(reading.free_bytes)
            .is_none_or(|bytes| bytes > reading.size_bytes)
    {
        return Err(CoreError::Unusable("Inconsistent virtiofs statistics".to_owned()));
    }
    #[allow(clippy::cast_precision_loss, reason = "df's percentage is a small integer")]
    let free_percent = (100.0 - crate::status::df_percent(reading.used_bytes, reading.free_bytes) as f64).max(0.0);
    Ok(Some(Report {
        mount: format!("{expected} virtiofs {}", reading.mountpoint),
        usage: HomeUsage {
            size_bytes: Some(reading.size_bytes),
            used_bytes: Some(reading.used_bytes),
            free_bytes: Some(reading.free_bytes),
            free_percent: Some(free_percent),
        },
    }))
}
