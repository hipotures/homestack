//! The `[pve]` part of a scenario: the initial state of the simulated cluster.
//!
//! Unknown keys are errors, so a typo cannot silently change a scenario.

use std::collections::BTreeMap;

use hs_model::NodeName;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterSpec {
    pub version: VersionSpec,
    #[serde(default)]
    pub nodes: Vec<NodeSpec>,
    #[serde(default)]
    pub tokens: Vec<TokenSpec>,
    #[serde(default)]
    pub tasks: Vec<TaskSpec>,
    #[serde(default)]
    pub faults: Vec<FaultSpec>,
    /// Cluster-wide storage definitions (`GET /storage`).
    #[serde(default)]
    pub storages: Vec<StorageSpec>,
    /// Volumes on node storages (`GET /nodes/{node}/storage/{storage}/content`).
    #[serde(default)]
    pub volumes: Vec<VolumeSpec>,
    #[serde(default)]
    pub vms: Vec<VmSpec>,
    /// Resource pools (`GET /pools`); a VM's pool adds the roles on
    /// `/pool/<poolid>` to those on `/vms/<vmid>`.
    #[serde(default)]
    pub pools: Vec<PoolSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolSpec {
    pub poolid: String,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub vms: Vec<u32>,
}

/// Body of `GET /version`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionSpec {
    pub release: String,
    pub version: String,
    pub repoid: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSpec {
    pub name: NodeName,
    #[serde(default)]
    pub status: NodeState,
    pub pveversion: String,
    pub kversion: String,
    pub uptime: u64,
    #[serde(default)]
    pub cpu: f64,
    #[serde(default = "default_maxcpu")]
    pub maxcpu: u32,
    #[serde(default)]
    pub mem: u64,
    #[serde(default)]
    pub maxmem: u64,
    #[serde(default)]
    pub disk: u64,
    #[serde(default)]
    pub maxdisk: u64,
    /// Storages active on this node, with their capacity.
    #[serde(default)]
    pub storages: Vec<NodeStorageSpec>,
    /// `GET /nodes/{node}/network` entries, as Proxmox returns them.
    #[serde(default)]
    pub network: Vec<toml::Table>,
    /// `GET /nodes/{node}/dns`, as Proxmox returns it.
    #[serde(default)]
    pub dns: toml::Table,
}

const fn default_maxcpu() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeState {
    #[default]
    Online,
    Offline,
}

impl NodeState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Offline => "offline",
        }
    }
}

/// An API token and its effective ACL.
///
/// The ACL is what the token can actually do, i.e. already the intersection
/// of user and token privileges for a privilege-separated token. A privilege
/// granted on a path also applies below it (`propagate=1`). The privilege
/// `*` stands for all of them, which models `root@pam`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenSpec {
    /// `user@realm!tokenid`
    pub id: String,
    pub secret: String,
    #[serde(default)]
    pub acl: BTreeMap<String, Vec<String>>,
}

/// A task that exists when the scenario starts.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub node: NodeName,
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub user: String,
    #[serde(default)]
    pub log: Vec<String>,
    /// Number of status polls after which the task reports `stopped`.
    #[serde(default)]
    pub polls_until_done: u32,
    #[serde(default = "default_exitstatus")]
    pub exitstatus: String,
}

fn default_exitstatus() -> String {
    "OK".to_owned()
}

/// Make the `occurrence`-th call of `op` (1-based) fail with `status`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaultSpec {
    pub op: String,
    pub occurrence: u32,
    pub status: u16,
    pub message: String,
}

impl ClusterSpec {
    /// Read the `[pve]` table of a scenario file.
    pub fn from_scenario(text: &str) -> Result<Self, String> {
        let mut table: toml::Table = toml::from_str(text).map_err(|error| error.to_string())?;
        let pve = table.remove("pve").ok_or("the scenario has no [pve] table")?;
        pve.try_into().map_err(|error: toml::de::Error| error.to_string())
    }
}

/// A storage definition (`/etc/pve/storage.cfg`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageSpec {
    pub storage: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// Comma-separated content types, e.g. `images,rootdir`.
    pub content: String,
    #[serde(default)]
    pub shared: bool,
    /// Comma-separated node restriction, if any.
    pub nodes: Option<String>,
}

/// A storage as one node reports it (`GET /nodes/{node}/storage`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeStorageSpec {
    pub storage: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub used: u64,
    #[serde(default)]
    pub avail: u64,
}

const fn default_true() -> bool {
    true
}

/// A volume on a node's storage. Its owner VMID comes from the volume name
/// (`vm-<vmid>-...`), as in Proxmox.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeSpec {
    pub node: NodeName,
    pub storage: String,
    pub volid: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default = "default_format")]
    pub format: String,
    /// The ext4 label of the filesystem on it; `None` for a blank volume.
    #[serde(default)]
    pub label: Option<String>,
}

fn default_format() -> String {
    "raw".to_owned()
}

/// A QEMU VM.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmSpec {
    pub vmid: u32,
    pub node: NodeName,
    #[serde(default)]
    pub status: VmState,
    /// The VM configuration as `GET .../config` returns it (`name`, `tags`,
    /// disks, ...). Values are strings or numbers.
    #[serde(default)]
    pub config: toml::Table,
    /// The guest seen through the QEMU guest agent; absent means the agent
    /// does not answer.
    pub guest: Option<GuestSpec>,
    /// Whether the guest powers off on the ACPI button (`status/shutdown`).
    #[serde(default = "default_true")]
    pub acpi: bool,
    /// The VM this one was cloned from: its first start boots that guest
    /// through native Cloud-Init (`api::boot_clone`).
    #[serde(skip)]
    pub clone_of: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VmState {
    Running,
    #[default]
    Stopped,
}

impl VmState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
        }
    }
}

/// What the guest agent reports, and what the guest commands HomeStack runs
/// through `exec` print.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestSpec {
    pub hostname: String,
    #[serde(default)]
    pub interfaces: Vec<InterfaceSpec>,
    #[serde(default)]
    pub filesystems: Vec<FilesystemSpec>,
    /// Accounts in `/etc/passwd`, in file order: a name alone gets UID and
    /// GID 0 for `root`, else the next free one from 1000.
    #[serde(default)]
    pub users: Vec<UserEntry>,
    /// Programs on the `PATH`, found in `/usr/bin`, `/usr/sbin`, `/bin` and
    /// `/sbin` alike (merged `/usr`); `python3-yaml` means `python3` can
    /// import `yaml`.
    #[serde(default = "default_commands")]
    pub commands: Vec<String>,
    /// Further files by absolute path (e.g. `authorized_keys`).
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// What `cloud-id` prints; cloud-init has finished (`result.json`
    /// exists) when it is set.
    #[serde(default = "default_cloud_id")]
    pub cloud_id: Option<String>,
}

/// The programs of a guest that meets the Gold contract (no `sudo`), and
/// what the vendor snippet needs (`python3-yaml` stands for Python's `yaml`
/// module).
fn default_commands() -> Vec<String> {
    [
        "cloud-init",
        "cloud-id",
        "sshd",
        "nmcli",
        "lsblk",
        "blkid",
        "wipefs",
        "mkfs.ext4",
        "mount",
        "mountpoint",
        "systemctl",
        "systemd-escape",
        "install",
        "rsync",
        "git",
        "ssh-keygen",
        "netplan",
        "udevadm",
        "NetworkManager",
        "python3",
        "python3-yaml",
    ]
    .iter()
    .map(|name| (*name).to_owned())
    .collect()
}

#[allow(clippy::unnecessary_wraps, reason = "serde default for an Option field")]
fn default_cloud_id() -> Option<String> {
    Some("nocloud".to_owned())
}

/// An account: a name, or a name with explicit IDs.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum UserEntry {
    Name(String),
    Account { name: String, uid: u32, gid: u32 },
}

/// One line of `/etc/passwd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
}

impl Account {
    /// `name:x:uid:gid:gecos:home:shell`
    #[must_use]
    pub fn passwd_line(&self) -> String {
        let (gecos, shell) = match self.uid {
            0 => ("root", "/bin/bash"),
            65534 => ("nobody", "/usr/sbin/nologin"),
            _ => ("", "/bin/bash"),
        };
        format!(
            "{}:x:{}:{}:{gecos}:{}:{shell}",
            self.name, self.uid, self.gid, self.home
        )
    }
}

impl GuestSpec {
    /// The accounts of `/etc/passwd` in file order, `nobody` last.
    #[must_use]
    pub fn accounts(&self) -> Vec<Account> {
        let mut next = 1000;
        let mut accounts: Vec<Account> = self
            .users
            .iter()
            .map(|entry| {
                let (name, uid, gid) = match entry {
                    UserEntry::Name(name) if name == "root" => (name.clone(), 0, 0),
                    UserEntry::Name(name) => {
                        next += 1;
                        (name.clone(), next - 1, next - 1)
                    }
                    UserEntry::Account { name, uid, gid } => (name.clone(), *uid, *gid),
                };
                let home = if uid == 0 {
                    "/root".to_owned()
                } else {
                    format!("/home/{name}")
                };
                Account { name, uid, gid, home }
            })
            .collect();
        accounts.push(Account {
            name: "nobody".to_owned(),
            uid: 65534,
            gid: 65534,
            home: "/nonexistent".to_owned(),
        });
        accounts
    }

    /// Whether a program is on the `PATH`.
    #[must_use]
    pub fn has_command(&self, name: &str) -> bool {
        self.commands.iter().any(|command| command == name)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterfaceSpec {
    pub name: String,
    #[serde(default = "default_mac")]
    pub mac: String,
    /// `address/prefix`, in kernel order.
    #[serde(default)]
    pub ipv4: Vec<String>,
}

fn default_mac() -> String {
    "00:00:00:00:00:00".to_owned()
}

/// A mounted filesystem. Sizes follow `statvfs`: `size` is all blocks,
/// `avail` what unprivileged users may still use (`df`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemSpec {
    pub mountpoint: String,
    #[serde(rename = "type")]
    pub fstype: String,
    /// Kernel device name, e.g. `sdb`.
    pub device: String,
    /// VM disk slot the device is attached to, e.g. `scsi1`.
    pub slot: String,
    /// Filesystem label (`blkid`).
    pub label: Option<String>,
    pub size: u64,
    pub used: u64,
    pub avail: u64,
}
