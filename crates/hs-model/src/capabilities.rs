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
    /// Required privileges are granted and Gold is configured when the operation needs it.
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
    /// What `deploy/pve/homestack-pve.sh install` makes, as far as the token
    /// can see it.
    pub installation: Installation,
}

/// The Proxmox objects of a HomeStack installation the server can check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Installation {
    /// The pool this instance changes workspaces in (`HOMESTACK_PVE_POOL`).
    pub pool: String,
    /// Gold's pool (`HOMESTACK_PVE_GOLD_POOL`, decision 9).
    pub gold_pool: String,
    /// No check is `missing` (`unknown` ones are left to the install script).
    pub complete: bool,
    pub checks: Vec<InstallationCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InstallationCheck {
    pub item: InstallationItem,
    pub status: InstallationStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstallationItem {
    /// The instance's pool exists.
    Pool,
    /// Gold's pool exists.
    GoldPool,
    /// The configured Gold is in Gold's pool.
    GoldInPool,
    /// The vendor snippets are on the nodes.
    Snippets,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstallationStatus {
    Ok,
    Missing,
    /// The token cannot tell.
    Unknown,
}
