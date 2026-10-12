//! `GET /v1/capabilities`: what the server's Proxmox token allows.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A HomeStack operation whose Proxmox privileges the server checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    /// Cluster, workspace and storage reads (`status`, `discover`, ...).
    Read,
    /// Listing VM volumes to find detached HomeStack volumes. Proxmox shows
    /// a VM volume only with `VM.Config.Disk` on its VM (or
    /// `Datastore.Allocate` on the storage).
    OrphanScan,
    /// Guest agent reads at audit level (`ping`, `get-fsinfo`, ...).
    GuestRead,
    /// Proxmox-side Gold checks.
    GoldReadiness,
    Create,
    Refresh,
    Resize,
    Migrate,
    Destroy,
    Addkey,
}

impl Operation {
    /// Every operation, in the order the API lists them.
    pub const ALL: [Self; 10] = [
        Self::Read,
        Self::OrphanScan,
        Self::GuestRead,
        Self::GoldReadiness,
        Self::Create,
        Self::Refresh,
        Self::Resize,
        Self::Migrate,
        Self::Destroy,
        Self::Addkey,
    ];
}

/// A Proxmox privilege on an ACL path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Privilege {
    /// ACL path, e.g. `/vms` or `/storage/local`.
    pub path: String,
    /// Privilege name, e.g. `VM.Allocate`.
    pub privilege: String,
}

/// Whether the token can perform one operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationCapability {
    pub operation: Operation,
    /// `missing` is empty.
    pub available: bool,
    /// Every privilege the operation needs, sorted by path and name.
    pub required: Vec<Privilege>,
    /// The required privileges the token does not have.
    pub missing: Vec<Privilege>,
}

/// Body of `GET /v1/capabilities`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// The server's Proxmox token, `USER@REALM!TOKENID` (never the secret).
    pub token_id: String,
    /// Configuration the ACL paths were derived from.
    pub config_version: String,
    pub operations: Vec<OperationCapability>,
}
