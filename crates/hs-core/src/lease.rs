//! The lock between server instances (mapping 8.3, `phase-11.md`): a lease
//! in the VM's `description`, next to the `refresh` journal of decision 4.
//!
//! ```text
//! <!-- homestack-lease: <base64 JSON> -->
//! ```
//!
//! A job writes its lease with the config's `digest` before it changes the
//! VM and removes it, again with `digest`, when it ends. A lease of another
//! instance that has not expired refuses the job; one of the same instance
//! for a job that instance does not run (it restarted) is stale. Nothing
//! renews a lease while its job runs: every step's write carries the
//! `digest` the step read, and a renewal in between would fail it, so a
//! lease lasts [`LEASE_TTL`].

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::CoreError;
use crate::inventory::{VmConfig, cluster_vm_resources, set_vm_config, vm_config};
use crate::pve::Pve;

/// How long a lease holds.
pub const LEASE_TTL: Duration = Duration::from_secs(2 * 60 * 60);

const LEASE_START: &str = "<!-- homestack-lease: ";
const LEASE_END: &str = " -->";
/// PVE's limit on `description` (phase 6 probe).
const DESCRIPTION_MAX: usize = 8192;
/// Writes lost to another writer before giving up.
const ATTEMPTS: usize = 5;

/// One instance's claim on a VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    /// `HOMESTACK_INSTANCE` of the holder.
    pub instance: String,
    pub job: String,
    pub operation: String,
    /// Seconds since the Unix epoch.
    pub acquired_at: u64,
    pub expires_at: u64,
}

/// The exact configuration transition made while acquiring a job lease.
pub struct Acquisition {
    pub vmid: u32,
    pub before: VmConfig,
    pub after: VmConfig,
    pub lease: Lease,
}

/// Who asks for a lease, and which jobs it runs now.
pub struct Holder<'a> {
    pub instance: &'a str,
    pub job: &'a str,
    pub operation: &'a str,
    pub now: SystemTime,
    pub ttl: Duration,
    /// Whether this instance runs a job with this id now.
    pub running: &'a (dyn Fn(&str) -> bool + Sync),
}

fn seconds(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

/// The description without the lease block, and the block's payload.
fn split(description: &str) -> Result<(String, Option<&str>), CoreError> {
    let Some(start) = description.find(LEASE_START) else {
        return Ok((description.to_owned(), None));
    };
    let after = &description[start + LEASE_START.len()..];
    let end = after
        .find(LEASE_END)
        .ok_or_else(|| CoreError::Unusable("The HomeStack lease in the VM description is cut off".to_owned()))?;
    let rest = format!("{}{}", description[..start].trim_end(), &after[end + LEASE_END.len()..]);
    Ok((rest, Some(&after[..end])))
}

/// The lease in a VM's config, if any.
pub fn read_lease(vm_cfg: &VmConfig) -> Result<Option<Lease>, CoreError> {
    let description = vm_cfg.get("description").map(String::as_str).unwrap_or_default();
    let (_, Some(payload)) = split(description)? else {
        return Ok(None);
    };
    let unusable = || CoreError::Unusable("The HomeStack lease in the VM description is not valid".to_owned());
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .map_err(|_| unusable())?;
    serde_json::from_slice(&bytes).map(Some).map_err(|_| unusable())
}

/// `description` with `lease` in place of any lease block.
pub fn description_with(current: &str, lease: Option<&Lease>) -> Result<String, CoreError> {
    let (rest, _) = split(current)?;
    let Some(lease) = lease else {
        return Ok(rest);
    };
    let json = serde_json::to_vec(lease).map_err(|error| CoreError::Failed(error.to_string()))?;
    let payload = base64::engine::general_purpose::STANDARD.encode(json);
    let separator = if rest.is_empty() { "" } else { "\n" };
    Ok(format!("{rest}{separator}{LEASE_START}{payload}{LEASE_END}"))
}

/// Whether `lease` lets `holder` work on the VM: none, its own, expired,
/// or the holder instance's for a job it no longer runs.
pub fn check(vmid: u32, lease: Option<&Lease>, holder: &Holder<'_>) -> Result<(), CoreError> {
    let Some(lease) = lease else {
        return Ok(());
    };
    let own = lease.instance == holder.instance;
    if (own && (lease.job == holder.job || !(holder.running)(&lease.job))) || lease.expires_at <= seconds(holder.now) {
        return Ok(());
    }
    let left = lease.expires_at.saturating_sub(seconds(holder.now));
    Err(CoreError::Busy(format!(
        "VM {vmid} is busy: instance {} runs {} (job {}); its lease ends in {} min unless that job ends first.",
        lease.instance,
        lease.operation,
        lease.job,
        left.div_ceil(60)
    )))
}

/// The node of a VM, `None` when it does not exist.
async fn node_of(pve: &impl Pve, vmid: u32) -> Result<Option<String>, CoreError> {
    Ok(cluster_vm_resources(pve)
        .await?
        .into_iter()
        .find(|resource| resource.vmid == vmid)
        .map(|resource| resource.node))
}

fn lost_race(error: &CoreError) -> bool {
    matches!(error, CoreError::Pve(error) if error.to_string().contains("checksum mismatch"))
}

/// Refuse when another instance holds the VM (`plan`; changes nothing).
/// A VM that does not exist is free.
pub async fn check_free(pve: &impl Pve, vmid: u32, holder: &Holder<'_>) -> Result<(), CoreError> {
    let Some(node) = node_of(pve, vmid).await? else {
        return Ok(());
    };
    let vm_cfg = vm_config(pve, &node, vmid).await?;
    check(vmid, read_lease(&vm_cfg)?.as_ref(), holder)
}

/// Write `holder`'s lease on the VM. A VM that does not exist yet is not
/// leased (`create`: a second clone of its VMID fails in Proxmox).
pub async fn acquire(pve: &impl Pve, vmid: u32, holder: &Holder<'_>) -> Result<Option<Acquisition>, CoreError> {
    let Some(node) = node_of(pve, vmid).await? else {
        return Ok(None);
    };
    let now = seconds(holder.now);
    let lease = Lease {
        instance: holder.instance.to_owned(),
        job: holder.job.to_owned(),
        operation: holder.operation.to_owned(),
        acquired_at: now,
        expires_at: now + holder.ttl.as_secs(),
    };
    for _ in 0..ATTEMPTS {
        let vm_cfg = vm_config(pve, &node, vmid).await?;
        check(vmid, read_lease(&vm_cfg)?.as_ref(), holder)?;
        let current = vm_cfg.get("description").map(String::as_str).unwrap_or_default();
        let description = description_with(current, Some(&lease))?;
        if description.chars().count() > DESCRIPTION_MAX {
            return Err(CoreError::Failed(format!(
                "The HomeStack lease does not fit in the description of VM {vmid} ({DESCRIPTION_MAX} characters at most)"
            )));
        }
        let digest = vm_cfg.get("digest").map(String::as_str);
        match set_vm_config(pve, &node, vmid, &[("description", &description)], digest).await {
            Ok(()) => {
                return Ok(Some(Acquisition {
                    vmid,
                    before: vm_cfg,
                    after: vm_config(pve, &node, vmid).await?,
                    lease,
                }));
            }
            Err(error) if lost_race(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Err(CoreError::Busy(format!(
        "VM {vmid} is busy: its configuration kept changing while its lease was written."
    )))
}

/// Remove the lease of `job`, wherever the VM is now. Nothing to do when
/// the VM is gone or holds another lease.
pub async fn release(pve: &impl Pve, vmid: u32, instance: &str, job: &str) -> Result<(), CoreError> {
    for _ in 0..ATTEMPTS {
        let Some(node) = node_of(pve, vmid).await? else {
            return Ok(());
        };
        let vm_cfg = vm_config(pve, &node, vmid).await?;
        match read_lease(&vm_cfg)? {
            Some(lease) if lease.instance == instance && lease.job == job => {}
            _ => return Ok(()),
        }
        let current = vm_cfg.get("description").map(String::as_str).unwrap_or_default();
        let description = description_with(current, None)?;
        let digest = vm_cfg.get("digest").map(String::as_str);
        let written = if description.is_empty() {
            set_vm_config(pve, &node, vmid, &[("delete", "description")], digest).await
        } else {
            set_vm_config(pve, &node, vmid, &[("description", &description)], digest).await
        };
        match written {
            Ok(()) => return Ok(()),
            Err(error) if lost_race(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Err(CoreError::Failed(format!(
        "The lease of VM {vmid} could not be removed: its configuration kept changing."
    )))
}
