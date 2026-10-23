//! Read results: global and per-VM status, storage capacity, detached
//! volumes, discovery, network profiles and Gold readiness.
//!
//! Field names and value types are those of the Python CLI's `--json`
//! output, so the parity tests compare them directly; only what describes
//! the CLI run itself (`ok`, `command`, `config`, `execution`) is left out.
//! Integers stay integers and percentages are floats, as in Python.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// `GET /v1/status`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct GlobalStatus {
    /// The configured Gold VM; `null` when it does not exist (see `warnings`).
    pub gold: Option<StatusEntry>,
    /// Workspaces (`homestack-ws`) and alien VMs (`homestack-alien`), by VMID.
    pub workspaces: Vec<StatusEntry>,
    /// The other VMs and containers of the HomeStack pools (more Golds,
    /// the servers), by VMID.
    #[serde(default)]
    pub others: Vec<StatusEntry>,
    /// The HomeStack pools the server's token sees, by ID.
    #[serde(default)]
    pub pools: Vec<PoolInfo>,
    pub summary: StatusSummary,
    pub warnings: Vec<String>,
}

/// One VM in the global status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct StatusEntry {
    pub vmid: u32,
    /// `GOLD`, `WS` or `ALIEN`.
    pub role: String,
    pub name: Option<String>,
    /// Proxmox status (`running`, `stopped`, `unknown` on an offline node).
    pub status: String,
    pub node: String,
    pub node_status: String,
    /// The pool it is in; `null` for none.
    #[serde(default)]
    pub pool: Option<String>,
    /// Its Proxmox tags, in their order.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Workspace address, or the actual static `net0` address for a container.
    pub ip: Option<String>,
    pub root_storage: Option<String>,
    /// Root disk capacity/usage; usage only when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<HomeUsage>,
    /// Home capacity/usage; `null` when no home configuration is available.
    pub home: Option<HomeUsage>,
    /// Optional in-container measurements; observations, never ownership proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_status: Option<hs_service_status::Report>,
    /// Rates derived from this and the previous `service_status` of the same
    /// process; `null` until a valid pair exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_rates: Option<ServiceRates>,
    pub role_tag_ok: bool,
    pub warning: Option<String>,
    /// The OCI image recorded by HomeStack in the container description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_update: Option<ImageUpdate>,
    /// The service HomeStack deployed in this container, from its description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<DeployedService>,
}

/// Counter deltas of two service reports divided by the time between their
/// measurements. A metric is `null` when either report lacks its counter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ServiceRates {
    /// Seconds between the two measurements.
    pub interval_seconds: f64,
    /// CPU time per wall time of the cgroup; 100 is one logical CPU busy.
    pub cpu_percent: Option<f64>,
    pub read_bytes_per_second: Option<f64>,
    pub write_bytes_per_second: Option<f64>,
    pub read_ops_per_second: Option<f64>,
    pub write_ops_per_second: Option<f64>,
}

/// A manifest-defined service in a container, as HomeStack's description
/// records it; an observation, never ownership proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DeployedService {
    /// `owner/repo` on GitHub.
    pub repository: String,
    /// The manifest `id`.
    pub id: String,
    /// `REGISTRY/PATH:VERSION`.
    pub image: String,
    pub digest: String,
}

/// Whether the server is currently discovering or checking container images.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ImageCheckProgress {
    pub checking: bool,
}

/// Cached registry check for a managed container's image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ImageUpdate {
    pub state: ImageUpdateState,
    pub available: Option<String>,
    /// Last successful check, in RFC 3339 UTC.
    pub checked_at: Option<String>,
    /// A failed check preserves the last successful result.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImageUpdateState {
    Checking,
    UpToDate,
    Available,
    Unknown,
}

/// A pool and its comment (the name shown for it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PoolInfo {
    pub id: String,
    pub comment: Option<String>,
}

/// Persistent home capacity/usage. Without filesystem measurements only
/// `size_bytes` from the VM disk or LXC mount configuration is known.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct HomeUsage {
    pub size_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub free_percent: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct StatusSummary {
    pub workspace_count: u64,
    pub running: u64,
    pub stopped: u64,
    /// Capacity of the configured node's default storage.
    pub root_storage: Capacity,
    pub storage_layouts: Vec<LayoutCapacity>,
    pub nodes: Vec<NodeState>,
    pub homes: HomesSummary,
    pub orphaned_volumes: Vec<OrphanVolume>,
    /// `false` when a node or storage that holds HomeStack volumes could not
    /// be scanned.
    pub orphan_scan_complete: bool,
    /// The cluster's name and quorum (`GET /cluster/status`); absent on a
    /// standalone node or when Proxmox denies the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quorum: Option<ClusterQuorum>,
}

/// The `cluster` entry of `GET /cluster/status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ClusterQuorum {
    pub name: String,
    pub quorate: bool,
    /// Nodes the cluster has.
    pub nodes: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Capacity {
    pub name: String,
    pub total_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    /// Used share of the total.
    pub percent: Option<f64>,
}

/// Extended inventory read independently of the main status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct StorageOverview {
    pub storages: Vec<StorageStatus>,
    pub latest_backups: Vec<GuestBackup>,
    pub backup_scan_complete: bool,
    pub warnings: Vec<String>,
}

/// A local storage on one node, or a shared storage listed once for all nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct StorageStatus {
    pub storage: String,
    pub storage_type: String,
    pub content: String,
    pub shared: bool,
    pub nodes: Vec<String>,
    pub capacity: Capacity,
    pub available: bool,
}

/// Newest backup actually present in the readable backup storage inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GuestBackup {
    pub vmid: u32,
    pub created_at: u64,
    pub storage: String,
}

/// One storage of one `homestack-storage-<N>` layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct LayoutCapacity {
    pub layout: String,
    pub node: String,
    pub storage: String,
    pub node_status: String,
    /// Whether the capacity could be read.
    pub ok: bool,
    pub name: String,
    pub total_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    pub percent: Option<f64>,
}

/// A cluster node and whether Proxmox reports it online, with the load
/// `GET /nodes` reports for an online node.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct NodeState {
    pub node: String,
    pub status: String,
    pub online: bool,
    /// CPU use, `0.0`–`1.0` of `maxcpu`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maxcpu: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maxmem_bytes: Option<u64>,
    /// The node's root filesystem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maxdisk_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct HomesSummary {
    pub used_bytes: Option<u64>,
    pub quota_bytes: u64,
    pub free_bytes: Option<u64>,
    pub free_percent: Option<f64>,
    /// Always `null`; kept for the CLI's output shape.
    pub physical_available_bytes: Option<u64>,
    pub missing_count: u64,
}

/// A HomeStack volume (`vm-<vmid>-hs-(root|home)-<name>`) no visible VM references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrphanVolume {
    pub volid: String,
    pub vmid: u32,
    /// `root` or `home`.
    pub role: String,
    pub name: String,
    pub size_bytes: Option<u64>,
    pub storage: String,
    pub node: String,
}

/// `GET /v1/orphans`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Orphans {
    pub volumes: Vec<OrphanVolume>,
    pub complete: bool,
    pub warnings: Vec<String>,
}

/// `GET /v1/workspaces/{target}`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct WorkspaceStatus {
    pub vmid: u32,
    pub name: Option<String>,
    /// `GOLD`, `WS`, `ALIEN`, or `null` without a role tag.
    pub role: Option<String>,
    pub tags: Vec<String>,
    pub role_tag_ok: bool,
    pub role_warning: Option<String>,
    pub status: String,
    pub node: String,
    pub configured_ip: Option<String>,
    /// From the guest agent, when the VM runs and the agent answers.
    pub actual_hostname: Option<String>,
    /// `address/prefix` of the first IPv4 address on `eth0`.
    pub actual_ip: Option<String>,
    /// Whether the guest agent answered.
    pub qga: bool,
    pub root_storage: Option<String>,
    pub volumes: Vec<VolumeEntry>,
    pub home_disk: Option<String>,
    pub home_storage: Option<String>,
    pub home_size_gb: Option<f64>,
    /// `HS_HOME_<vmid>` for workspaces.
    pub home_label: Option<String>,
    /// `serial=` of the home disk in the VM config.
    pub home_serial: Option<String>,
    pub home_identity_ok: Option<bool>,
    /// `<device> <type> <mountpoint>` of the filesystem at `/home/<user>`.
    pub home_mount: Option<String>,
    /// The home filesystem is ext4 on the VM's home disk.
    pub home_mount_ok: Option<bool>,
    pub ssh: SshCommands,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct VolumeEntry {
    pub slot: String,
    /// `root`, `home`, `unused`, `efi`, `tpm`, `cloud-init` or `disk`.
    pub role: String,
    pub volume: String,
    pub storage: Option<String>,
    pub size: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SshCommands {
    pub root: Option<String>,
    pub user: Option<String>,
}

/// `GET /v1/discovery`: the Proxmox part of `homestack discover`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Discovery {
    /// `GET /version` as Proxmox returns it.
    #[schema(value_type = Object)]
    pub proxmox_version: serde_json::Value,
    pub nodes: Vec<NodeState>,
    /// VMs tagged `homestack-gold`: `vmid`, `name`, `node`, `status`, `tags`
    /// where Proxmox reports them.
    #[schema(value_type = Vec<Object>)]
    pub gold_candidates: Vec<serde_json::Map<String, serde_json::Value>>,
    /// Storage definitions: `storage`, `type`, `content`, `nodes`, `shared`
    /// where defined.
    #[schema(value_type = Vec<Object>)]
    pub storages: Vec<serde_json::Map<String, serde_json::Value>>,
}

/// One network profile a workspace could use (`GET /v1/networks`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NetworkCandidate {
    pub cidr: String,
    pub bridge: Option<String>,
    pub gateway: Option<String>,
    pub dns_servers: Vec<String>,
    /// Where it was found, e.g. `HomeStack workspace VM 200`.
    pub source: String,
}

/// `GET /v1/gold/readiness`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GoldReadiness {
    pub vmid: u32,
    pub node: String,
    pub power_state: String,
    /// No required check failed.
    pub ok: bool,
    /// Some guest check ran.
    pub guest_checked: bool,
    pub checks: Vec<GoldCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GoldCheck {
    /// `PVE`, `guest` or `security`.
    pub scope: String,
    pub name: String,
    /// `pass`, `fail` or `skip`.
    pub status: String,
    pub detail: String,
    /// `required` or `optional`.
    pub requirement: String,
}
