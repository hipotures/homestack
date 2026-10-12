//! Proxmox data the server passes through to its clients.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Proxmox VE version, as returned by `GET /version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PveVersion {
    /// Major and minor release, e.g. `9.2`.
    pub release: String,
    /// Full `pve-manager` version, e.g. `9.2.21`.
    pub version: String,
    /// Source revision of `pve-manager`.
    pub repoid: String,
}

/// The part of `GET /nodes/{node}/status` HomeStack reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NodeStatus {
    /// Node the status belongs to.
    pub node: String,
    /// `pve-manager` package version string, e.g. `pve-manager/9.2.21/4f6e0ac8`.
    pub pveversion: String,
    /// Running kernel description.
    pub kversion: String,
    /// Node uptime in seconds.
    pub uptime: u64,
}
