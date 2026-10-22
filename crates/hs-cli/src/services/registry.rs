//! The profile's service deployment registry: `services.toml` next to
//! `client.toml`. It holds no secrets: identities, placement, image digests
//! and the journal of an unfinished operation. A record is what HomeStack
//! did, not proof of what exists; [`instance_status`] compares it with what
//! Proxmox reports before anything relies on it.
//!
//! Conventions: readers take the file as it is (writes replace it
//! atomically); a writer holds an exclusive `flock` on `services.toml.lock`
//! for its whole read-modify-write ([`update`]); an operation records its
//! journal before each risky step, and a record with a journal needs review
//! before any retry.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use hs_model::status::StatusEntry;
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub version: u32,
    #[serde(default)]
    pub deployments: Vec<Deployment>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            version: VERSION,
            deployments: Vec::new(),
        }
    }
}

/// One installation of a service: identity `(repository, service)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    /// `owner/repo` on GitHub.
    pub repository: String,
    /// The manifest `id`.
    pub service: String,
    /// The branch the manifest was read from, its commit and blob.
    pub git_ref: String,
    pub commit: String,
    pub manifest_path: String,
    pub manifest_blob: String,
    pub image_repository: String,
    pub image_version: String,
    /// `sha256:…`, resolved when the version was chosen.
    pub image_digest: String,
    /// The names of the environment variables set on the containers; the
    /// values stay in Proxmox.
    #[serde(default)]
    pub environment: Vec<String>,
    #[serde(default)]
    pub instances: Vec<Instance>,
    /// Set while an operation is under way; left behind by an interruption.
    #[serde(default)]
    pub operation: Option<Operation>,
}

/// One container of a deployment on one node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Instance {
    pub node: String,
    pub ctid: u32,
    pub hostname: String,
    pub address: String,
    /// Empty until Proxmox assigned it.
    pub mac: String,
    pub pool: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub cores: u32,
    pub memory_mib: u32,
    pub root_storage: String,
    pub root_gib: u32,
    pub home_storage: String,
    pub home_gib: u32,
    /// Proxmox volume IDs, e.g. `local-lvm:vm-120-disk-0`; empty until the
    /// container exists.
    pub root_volume: String,
    pub home_volume: String,
    pub stage: Stage,
    /// When its health check last passed (RFC 3339).
    #[serde(default)]
    pub verified: Option<String>,
    pub desired: Desired,
    /// Per-instance release, allowing selected-node updates.
    #[serde(default)]
    pub release: Option<Release>,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub nameserver: Option<String>,
}

/// The immutable release last installed on one instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub version: String,
    pub digest: String,
    pub commit: String,
    pub manifest_blob: String,
}

/// How far the deployment of an instance got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Recorded before the container is created.
    Planned,
    /// The container and its volumes exist.
    Created,
    /// Started and checked.
    Ready,
    /// Container removed; the exact home volume remains reserved.
    Retained,
    /// Container and explicitly selected data removed.
    Removed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Desired {
    Running,
    Stopped,
}

/// The journal of an operation: what it is, the step it reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub kind: String,
    pub step: String,
    /// RFC 3339.
    pub started: String,
    #[serde(default)]
    pub lifecycle: Option<super::lifecycle::Journal>,
    #[serde(default)]
    pub replacement: Option<super::replacement::Journal>,
}

impl Deployment {
    /// Whether anything of it is still deployed: an unfinished operation or
    /// an instance that is not removed.
    #[must_use]
    pub fn active(&self) -> bool {
        self.operation.is_some() || self.instances.iter().any(|i| i.stage != Stage::Removed)
    }
}

impl Registry {
    #[must_use]
    pub fn deployment(&self, repository: &str, service: &str) -> Option<&Deployment> {
        self.deployments
            .iter()
            .find(|deployment| deployment.repository == repository && deployment.service == service)
    }

    #[must_use]
    pub fn has_repository(&self, repository: &str) -> bool {
        self.deployments
            .iter()
            .any(|deployment| deployment.repository == repository && deployment.active())
    }

    /// The deployment and instance that own container `ctid`.
    #[must_use]
    pub fn instance(&self, ctid: u32) -> Option<(&Deployment, &Instance)> {
        self.deployments.iter().find_map(|deployment| {
            deployment
                .instances
                .iter()
                .find(|instance| instance.ctid == ctid)
                .map(|instance| (deployment, instance))
        })
    }
}

/// `services.toml` beside `client.toml`: one registry per profile.
#[must_use]
pub fn path_for(client_toml: &Path) -> PathBuf {
    client_toml.with_file_name("services.toml")
}

/// The registry at `path`; empty when there is none yet.
pub fn load(path: &Path) -> Result<Registry, String> {
    let shown = path.display();
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Registry::default()),
        Err(error) => return Err(format!("{shown}: {error}")),
    };
    if !metadata.is_file() {
        return Err(format!("{shown}: not a regular file"));
    }
    if metadata.len() > MAX_BYTES {
        return Err(format!("{shown}: larger than {MAX_BYTES} bytes"));
    }
    let text = std::fs::read_to_string(path).map_err(|error| format!("{shown}: {error}"))?;
    let registry: Registry = toml::from_str(&text).map_err(|error| format!("{shown}: {error}"))?;
    if registry.version != VERSION {
        return Err(format!("{shown}: version {} is not {VERSION}", registry.version));
    }
    Ok(registry)
}

/// Held for an entire infrastructure operation; file writes use their own
/// short lock so journal checkpoints remain atomic without deadlocking.
pub fn operation_lock(path: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path.with_file_name("services.operation.lock"))
        .map_err(|e| e.to_string())?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| "Another service operation holds this profile's lock".to_owned())?;
    Ok(file)
}

/// Change the registry under its lock and replace the file atomically
/// (mode 0600).
pub fn update<T>(path: &Path, change: impl FnOnce(&mut Registry) -> Result<T, String>) -> Result<T, String> {
    let failed = |error: std::io::Error| format!("{}: {error}", path.display());
    let directory = path.parent().unwrap_or(Path::new("."));
    let lock_path = path.with_file_name("services.toml.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(failed)?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)
        .map_err(|error| failed(std::io::Error::from(error)))?;
    let mut registry = load(path)?;
    let value = change(&mut registry)?;
    let text = toml::to_string(&registry).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".services.toml.")
        .tempfile_in(directory)
        .map_err(failed)?;
    temporary
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(failed)?;
    temporary.write_all(text.as_bytes()).map_err(failed)?;
    temporary.as_file().sync_all().map_err(failed)?;
    temporary.persist(path).map_err(|error| failed(error.error))?;
    std::fs::File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(failed)?;
    Ok(value)
}

/// A service's or a container's state as the registry and Proxmox see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    NotDeployed,
    /// Recorded without any instance yet.
    Registered,
    Running,
    Stopped,
    /// Recorded, but Proxmox does not list the container.
    Missing,
    NeedsReview(String),
    /// A container no registry record owns.
    Unmanaged,
    /// The registry or the cluster could not be read.
    Unavailable(String),
}

impl Status {
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::NotDeployed => "not deployed".to_owned(),
            Self::Registered => "registered".to_owned(),
            Self::Running => "running".to_owned(),
            Self::Stopped => "stopped".to_owned(),
            Self::Missing => "missing".to_owned(),
            Self::NeedsReview(why) => format!("needs review: {why}"),
            Self::Unmanaged => "unmanaged".to_owned(),
            Self::Unavailable(why) => format!("status unavailable: {why}"),
        }
    }
}

/// Compare `instance` of `deployment` with the containers Proxmox lists
/// (`None` when they could not be read).
#[must_use]
pub fn instance_status(deployment: &Deployment, instance: &Instance, observed: Option<&[StatusEntry]>) -> Status {
    let Some(observed) = observed else {
        return Status::Unavailable("cluster status not read".to_owned());
    };
    if instance.stage == Stage::Retained {
        return Status::NeedsReview(format!("home retained: {}", instance.home_volume));
    }
    if instance.stage == Stage::Removed {
        return Status::NotDeployed;
    }
    if let Some(operation) = &deployment.operation {
        return Status::NeedsReview(format!("unfinished {} at {}", operation.kind, operation.step));
    }
    let Some(entry) = observed.iter().find(|entry| entry.vmid == instance.ctid) else {
        return Status::Missing;
    };
    if entry.role != "CT" {
        return Status::NeedsReview(format!("{} is not a container", instance.ctid));
    }
    if entry.node != instance.node {
        return Status::NeedsReview(format!("on {}, recorded on {}", entry.node, instance.node));
    }
    match entry.status.as_str() {
        "running" => Status::Running,
        "stopped" => Status::Stopped,
        other => Status::NeedsReview(other.to_owned()),
    }
}

/// Each instance's status, or the service's when it has none.
#[must_use]
pub fn service_status(
    registry: &Result<Registry, String>,
    repository: &str,
    service: &str,
    observed: Option<&[StatusEntry]>,
) -> Vec<(Option<u32>, Status)> {
    let registry = match registry {
        Ok(registry) => registry,
        Err(error) => return vec![(None, Status::Unavailable(error.clone()))],
    };
    let Some(deployment) = registry.deployment(repository, service) else {
        return vec![(None, Status::NotDeployed)];
    };
    if deployment.instances.is_empty() {
        return vec![(
            None,
            match &deployment.operation {
                Some(operation) => Status::NeedsReview(format!("unfinished {} at {}", operation.kind, operation.step)),
                None => Status::Registered,
            },
        )];
    }
    deployment
        .instances
        .iter()
        .map(|instance| (Some(instance.ctid), instance_status(deployment, instance, observed)))
        .collect()
}
