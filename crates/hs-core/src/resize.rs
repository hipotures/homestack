//! `resize` (mapping B5; phase 7): grow a workspace's root or home disk in
//! Proxmox and its layers in the guest.
//!
//! Ports `resize.build_resize_plan`, `resize_workspace`, `_execute_resize`
//! and `resize_auth.crypt_resize_access`. The guest is inspected by
//! [`crate::resize_guest`]. The disk grows with `PUT /resize` and the
//! config's `digest`; the guest steps run through `agent/exec` as root, one
//! command per call, in the order and with the stops of Python's `set -eu`
//! script. A LUKS passphrase comes with the request that applies the plan
//! and reaches `cryptsetup --key-file -` on its standard input
//! (`input-data`), never in the arguments (decision 6). A repeated resize
//! with the same size skips Proxmox and finishes the guest steps.
//! `docs/migration/phase-7.md` lists what differs from Python.

use std::collections::BTreeSet;
use std::time::Duration;

use hs_model::config::Config;
use serde_json::{Value, json};

use crate::error::CoreError;
use crate::guest::{GUEST_EXEC_TIMEOUT, guest_exec_with_input, guest_out, shell_join};
use crate::inventory::{GOLD_TAG, disk_volume, has_tag, parse_disk_size_gb, vm_config};
use crate::lifecycle::{LifecycleTiming, parse_home_size, require_pool, step_finished, step_started, text, vmid_of};
use crate::pve::Pve;
use crate::resize_guest::{CryptAuth, CryptLayer, GuestLayout, InspectError, QgaGuest, Role, inspect};
use crate::status::resolve_existing_workspace;
use crate::tasks::{Observer, wait_task};

/// The name of the LUKS passphrase among the secrets of an apply request.
pub const LUKS_PASSPHRASE: &str = "luks_passphrase";

/// Bound of `PUT /resize` (Python: `node_run(..., timeout=300)`).
const RESIZE_TIMEOUT: Duration = Duration::from_secs(300);

/// Reads of the guest's disk size before giving up (Python: 15).
const DISK_SIZE_READS: u32 = 15;

/// What `resize` is asked for: exactly one of the sizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizeRequest {
    pub vmid: u32,
    pub root_size: Option<String>,
    pub home_size: Option<String>,
}

/// Python's `f"{value:g}"` for disk sizes.
fn format_g(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        #[allow(clippy::cast_possible_truncation, reason = "integral and small")]
        return format!("{}", value as i64);
    }
    format!("{value}")
}

/// The guest's mount of `role`.
fn mount_of(cfg: &Config, role: Role) -> String {
    match role {
        Role::Root => "/".to_owned(),
        Role::Home => format!("/home/{}", cfg.user.name),
    }
}

/// `_inspect_guest`: the inspection, with a guest failure as "Cannot
/// inspect ROLE disk in VM VMID: REASON".
async fn inspect_guest(
    pve: &impl Pve,
    cfg: &Config,
    node: &str,
    vmid: u32,
    role: Role,
    label: &str,
) -> Result<GuestLayout, CoreError> {
    let mut guest = QgaGuest::new(pve, node, vmid);
    match inspect(&mut guest, &mount_of(cfg, role), role, label).await {
        Ok(layout) => Ok(layout),
        Err(InspectError::Guest(reason)) => Err(CoreError::Failed(format!(
            "Cannot inspect {} disk in VM {vmid}: {}",
            role.as_str(),
            reason.trim()
        ))),
        Err(InspectError::Core(error)) => Err(error),
    }
}

/// Whether a config key is a data disk (`scsiN`, `virtioN`, `sataN`, `ideN`).
fn data_disk_key(key: &str) -> bool {
    ["scsi", "virtio", "sata", "ide"].iter().any(|bus| {
        key.strip_prefix(bus)
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// `build_resize_plan` for a VM of `pool`. The plan holds no config
/// `digest`: the execution reads it again (see `phase-7.md`).
pub async fn plan_resize(
    pve: &impl Pve,
    cfg: &Config,
    pool: &str,
    request: &ResizeRequest,
) -> Result<Value, CoreError> {
    let (role, requested) = match (&request.root_size, &request.home_size) {
        (Some(size), None) => (Role::Root, size),
        (None, Some(size)) => (Role::Home, size),
        _ => {
            return Err(CoreError::Invalid(
                "Specify exactly one of root_size or home_size".to_owned(),
            ));
        }
    };
    let vmid = request.vmid;
    let (size, target_mib) = parse_home_size(requested)?;
    #[allow(clippy::cast_precision_loss, reason = "sizes in MiB are small")]
    let target_gib = target_mib as f64 / 1024.0;
    require_pool(pve, vmid, pool).await?;
    let info = resolve_existing_workspace(pve, cfg, vmid).await?;
    let vm_cfg = &info.vm_config;
    if info.status != "running" {
        return Err(CoreError::Refused(
            "Resize requires a running VM with a working QEMU Guest Agent".to_owned(),
        ));
    }
    let locked = vm_cfg.get("lock").is_some_and(|lock| !lock.is_empty());
    if locked
        || vm_cfg.get("template").map(String::as_str) == Some("1")
        || has_tag(vm_cfg.get("tags").map(String::as_str), GOLD_TAG)
    {
        return Err(CoreError::Refused(
            "Cannot resize a locked VM or Gold/template VM".to_owned(),
        ));
    }
    let disk = match role {
        Role::Root => cfg.root_disk.clone(),
        Role::Home => cfg.home_disk.clone(),
    };
    let data_disks: BTreeSet<&str> = vm_cfg
        .iter()
        .filter(|(key, value)| data_disk_key(key) && !value.contains("media=cdrom") && !value.contains("cloudinit"))
        .map(|(key, _)| key.as_str())
        .collect();
    if data_disks != BTreeSet::from([cfg.root_disk.as_str(), cfg.home_disk.as_str()]) {
        return Err(CoreError::Refused(
            "Resize requires exactly the configured root and home data disks".to_owned(),
        ));
    }
    let disk_cfg = vm_cfg.get(&disk).cloned().unwrap_or_default();
    let current = parse_disk_size_gb(&disk_cfg)
        .ok_or_else(|| CoreError::Unusable(format!("Cannot determine current disk size for {disk}")))?;
    if target_gib < current {
        return Err(CoreError::Refused(format!(
            "Shrinking is not supported: {disk} is {}G, requested {size}",
            format_g(current)
        )));
    }
    // The growth must fit in the storage's free space (owner, 2026-10-15).
    let capacity = require_growth_space(pve, &info.node, disk_volume(&disk_cfg), current, target_gib).await?;
    let guest = inspect_guest(pve, cfg, &info.node, vmid, role, &info.home_label).await?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "Python's int()")]
    let current_bytes = (current * 1024.0 * 1024.0 * 1024.0) as u64;
    if guest.size_bytes != current_bytes {
        return Err(CoreError::Refused(
            "Guest disk size does not match Proxmox; wait for the guest to detect the disk size".to_owned(),
        ));
    }
    let secrets: Vec<&str> = match &guest.crypt {
        Some(CryptLayer {
            auth: CryptAuth::Passphrase,
            ..
        }) => vec![LUKS_PASSPHRASE],
        _ => Vec::new(),
    };
    let guest = serde_json::to_value(&guest).map_err(|error| CoreError::Failed(error.to_string()))?;
    Ok(json!({
        "command": "resize",
        "vmid": vmid,
        "name": info.name,
        "node": info.node,
        "pool": pool,
        "role": role.as_str(),
        "disk": disk,
        "volume": disk_volume(&disk_cfg),
        "current_size_gib": current,
        "size": size,
        "target_size_gib": target_gib,
        "guest": guest,
        "secrets_required": secrets,
        "capacity": capacity,
    }))
}

/// Admission of growing `volume` on `node` from `current` to `target` GiB:
/// the whole growth must fit in the free space of the volume's storage. A
/// resize to the current size (finishing the guest steps) needs none.
async fn require_growth_space(
    pve: &impl Pve,
    node: &str,
    volume: &str,
    current: f64,
    target: f64,
) -> Result<Value, CoreError> {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "sizes in GiB are small"
    )]
    let bytes = |gib: f64| (gib * 1024.0 * 1024.0 * 1024.0) as u64;
    let growth = bytes(target).saturating_sub(bytes(current));
    if growth == 0 {
        return Ok(Value::Null);
    }
    let storage = volume
        .split_once(':')
        .map(|(storage, _)| storage)
        .filter(|storage| !storage.is_empty())
        .ok_or_else(|| CoreError::Refused(format!("Cannot determine the storage of volume {volume}")))?;
    crate::capacity::require_space(pve, node, storage, growth).await
}

fn float(plan: &Value, key: &str) -> Result<f64, CoreError> {
    plan.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| CoreError::Invalid(format!("The plan has no {key}")))
}

/// What unlocks the LUKS layer for the test and the resize.
enum CryptKey<'a> {
    File(&'a str),
    Passphrase(&'a str),
}

struct Crypt<'a> {
    layer: &'a CryptLayer,
    key: CryptKey<'a>,
}

impl Crypt<'_> {
    fn command(&self, action: &[&str]) -> String {
        let key_file = match self.key {
            CryptKey::File(path) => path,
            CryptKey::Passphrase(_) => "-",
        };
        let mut words = vec!["cryptsetup", "--batch-mode", "--key-file", key_file];
        words.extend_from_slice(action);
        shell_join(&words)
    }

    const fn input(&self) -> Option<&str> {
        match self.key {
            CryptKey::File(_) => None,
            CryptKey::Passphrase(passphrase) => Some(passphrase),
        }
    }

    /// `cryptsetup open --test-passphrase`: the key opens the device.
    async fn test(&self, pve: &impl Pve, node: &str, vmid: u32) -> Result<(), CoreError> {
        let command = self.command(&["open", "--type", "luks", "--test-passphrase", &self.layer.device]);
        let result = guest_exec_with_input(pve, node, vmid, &command, self.input(), false, GUEST_EXEC_TIMEOUT).await?;
        if result.exited && result.exitcode == 0 {
            return Ok(());
        }
        Err(CoreError::Failed(
            match self.key {
                CryptKey::File(_) => "LUKS authentication or resize failed; verify the configured key file in the VM",
                CryptKey::Passphrase(_) => "LUKS passphrase verification failed; no disk changes made",
            }
            .to_owned(),
        ))
    }

    /// `cryptsetup resize NAME`
    async fn grow(&self, pve: &impl Pve, node: &str, vmid: u32) -> Result<(), CoreError> {
        let command = self.command(&["resize", &self.layer.name]);
        let result = guest_exec_with_input(pve, node, vmid, &command, self.input(), false, GUEST_EXEC_TIMEOUT).await?;
        if result.exited && result.exitcode == 0 {
            return Ok(());
        }
        Err(CoreError::Failed(
            match self.key {
                CryptKey::File(_) => "LUKS authentication or resize failed; verify the configured key file in the VM",
                CryptKey::Passphrase(_) => "LUKS resize failed; guest command output withheld",
            }
            .to_owned(),
        ))
    }
}

/// `crypt_resize_access`: what unlocks the layer, refused before any change
/// when it cannot be had.
fn crypt_access<'a>(layer: &'a CryptLayer, passphrase: Option<&'a str>) -> Result<Crypt<'a>, CoreError> {
    let key = match layer.auth {
        CryptAuth::Keyfile => CryptKey::File(
            layer
                .key_file
                .as_deref()
                .ok_or_else(|| CoreError::Invalid("The plan's LUKS layer has no key file".to_owned()))?,
        ),
        CryptAuth::Passphrase => match passphrase {
            None => {
                return Err(CoreError::Refused(format!(
                    "LUKS requires a passphrase. Apply the plan with secrets.{LUKS_PASSPHRASE}; no disk changes made"
                )));
            }
            Some("") => {
                return Err(CoreError::Refused(
                    "Empty LUKS passphrase; no disk changes made".to_owned(),
                ));
            }
            Some(passphrase) => CryptKey::Passphrase(passphrase),
        },
    };
    Ok(Crypt { layer, key })
}

/// `resize_workspace` for a plan of [`plan_resize`]; `passphrase` is the
/// LUKS passphrase of the apply request.
pub async fn resize_workspace(
    pve: &impl Pve,
    plan: &Value,
    passphrase: Option<&str>,
    timing: &LifecycleTiming,
    observer: &impl Observer,
) -> Result<Value, CoreError> {
    let vmid = vmid_of(plan)?;
    let node = text(plan, "node")?;
    let disk = text(plan, "disk")?;
    let volume = text(plan, "volume")?;
    let size = text(plan, "size")?;
    let role = text(plan, "role")?;
    let current = float(plan, "current_size_gib")?;
    let target = float(plan, "target_size_gib")?;
    let guest: GuestLayout = serde_json::from_value(plan.get("guest").cloned().unwrap_or_default())
        .map_err(|_| CoreError::Invalid("The plan has no guest inspection".to_owned()))?;

    let vm_cfg = vm_config(pve, node, vmid).await?;
    let disk_cfg = vm_cfg.get(disk).map(String::as_str).unwrap_or_default();
    if disk_volume(disk_cfg) != volume {
        return Err(CoreError::Refused(
            "Workspace volume changed after resize confirmation; generate a new plan".to_owned(),
        ));
    }
    if parse_disk_size_gb(disk_cfg) != Some(current) {
        return Err(CoreError::Refused(
            "Workspace current_size_gib changed after resize confirmation; generate a new plan".to_owned(),
        ));
    }
    let digest = vm_cfg.get("digest").cloned().unwrap_or_default();
    if digest.is_empty() {
        return Err(CoreError::Failed(
            "Missing Proxmox configuration digest; cannot safely resize".to_owned(),
        ));
    }
    let crypt = guest
        .crypt
        .as_ref()
        .map(|layer| crypt_access(layer, passphrase))
        .transpose()?;
    if let Some(crypt) = &crypt {
        let step = "Check the LUKS key in the guest";
        step_started(observer, step);
        let tested = crypt.test(pve, node, vmid).await;
        step_finished(observer, step, tested.is_ok());
        tested?;
    }
    if target > current {
        let step = "Grow the disk in Proxmox";
        step_started(observer, step);
        let grown = async {
            require_growth_space(pve, node, volume, current, target).await?;
            grow_disk(pve, node, vmid, disk, size, &digest, timing, observer).await
        }
        .await;
        step_finished(observer, step, grown.is_ok());
        grown?;
    }
    let step = "Grow the partition and filesystem in the guest";
    step_started(observer, step);
    let finished = grow_guest(pve, node, vmid, disk, volume, target, &guest, crypt.as_ref(), timing).await;
    step_finished(observer, step, finished.is_ok());
    if let Err(error) = finished {
        return Err(CoreError::Failed(format!(
            "Disk growth may already be applied; filesystem growth is incomplete. \
             Re-run resize {vmid} --{role}-size {size} to finish. {error}"
        )));
    }
    let mut result = plan.as_object().cloned().unwrap_or_default();
    result.insert("ok".to_owned(), json!(true));
    result.insert("status".to_owned(), json!("resized"));
    result.insert("event".to_owned(), json!("workspace.resized"));
    Ok(Value::Object(result))
}

/// `qm disk resize VMID DISK SIZE --digest D` as `PUT /resize`, and its
/// task when Proxmox runs it as one.
#[allow(clippy::too_many_arguments, reason = "one Proxmox call")]
async fn grow_disk(
    pve: &impl Pve,
    node: &str,
    vmid: u32,
    disk: &str,
    size: &str,
    digest: &str,
    timing: &LifecycleTiming,
    observer: &impl Observer,
) -> Result<(), CoreError> {
    let path = format!("/nodes/{node}/qemu/{vmid}/resize");
    crate::tasks::retry_while_locked(timing.poll, || async {
        let answer = pve
            .put(&path, &[("disk", disk), ("size", size), ("digest", digest)])
            .await?;
        if let Some(upid) = answer.as_str() {
            let wait = crate::tasks::TaskWait {
                poll: timing.poll,
                timeout: RESIZE_TIMEOUT,
            };
            let result = wait_task(pve, upid, &wait, observer).await?;
            if !result.ok() {
                return Err(CoreError::Failed(format!("Disk resize failed: {}", result.exitstatus)));
            }
        }
        Ok(())
    })
    .await
}

async fn guest_command(pve: &impl Pve, node: &str, vmid: u32, words: &[&str]) -> Result<String, CoreError> {
    guest_out(pve, node, vmid, &shell_join(words), true, GUEST_EXEC_TIMEOUT).await
}

/// The guest part of `_execute_resize`: the config check, then the disk
/// size, the partition, LUKS, LVM and the filesystem, stopping at the first
/// failure.
#[allow(clippy::too_many_arguments, reason = "the plan's parts")]
async fn grow_guest(
    pve: &impl Pve,
    node: &str,
    vmid: u32,
    disk: &str,
    volume: &str,
    target: f64,
    guest: &GuestLayout,
    crypt: Option<&Crypt<'_>>,
    timing: &LifecycleTiming,
) -> Result<(), CoreError> {
    let vm_cfg = vm_config(pve, node, vmid).await?;
    let disk_cfg = vm_cfg.get(disk).map(String::as_str).unwrap_or_default();
    if disk_volume(disk_cfg) != volume || parse_disk_size_gb(disk_cfg) != Some(target) {
        return Err(CoreError::Failed(
            "Proxmox disk size or identity verification failed".to_owned(),
        ));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "Python's int()")]
    let target_bytes = (target * 1024.0 * 1024.0 * 1024.0) as u64;
    let mut reads = 0;
    loop {
        let out = guest_command(pve, node, vmid, &["blockdev", "--getsize64", &guest.parent]).await?;
        let seen: u64 = out
            .trim()
            .parse()
            .map_err(|_| CoreError::Failed(format!("blockdev printed {out:?} for {}", guest.parent)))?;
        if seen >= target_bytes {
            break;
        }
        reads += 1;
        if reads >= DISK_SIZE_READS {
            return Err(CoreError::Failed("Guest has not detected the enlarged disk".to_owned()));
        }
        tokio::time::sleep(timing.poll).await;
    }
    if let Some(partition) = guest.partition {
        let number = partition.to_string();
        let command = shell_join(&["growpart", &guest.parent, &number]);
        let result = crate::guest::guest_exec(pve, node, vmid, &command, false, GUEST_EXEC_TIMEOUT).await?;
        let output = format!("{}{}", result.out, result.err);
        let unchanged = result.exitcode == 1 && output.starts_with("NOCHANGE:");
        if result.exitcode != 0 && !unchanged {
            return Err(CoreError::Failed(format!(
                "Guest command failed in VM {vmid} on {node} (exit={}): {command}\n{}",
                result.exitcode,
                output.trim()
            )));
        }
    }
    if let Some(crypt) = crypt {
        crypt.grow(pve, node, vmid).await?;
    }
    if let Some(lvm) = &guest.lvm {
        guest_command(pve, node, vmid, &["pvresize", &lvm.pv_path]).await?;
        let free = guest_command(
            pve,
            node,
            vmid,
            &["vgs", "--noheadings", "-o", "vg_free_count", &lvm.vg_name],
        )
        .await?;
        if free.trim().parse::<i64>().is_ok_and(|free| free > 0) {
            guest_command(pve, node, vmid, &["lvextend", "--extents", "+100%FREE", &lvm.lv_path]).await?;
        }
    }
    if guest.filesystem == "btrfs" {
        let devid = guest
            .btrfs_devid
            .ok_or_else(|| CoreError::Invalid("The plan's btrfs layer has no device id".to_owned()))?;
        guest_command(
            pve,
            node,
            vmid,
            &["btrfs", "filesystem", "resize", &format!("{devid}:max"), &guest.mount],
        )
        .await?;
    } else {
        guest_command(pve, node, vmid, &["resize2fs", &guest.device]).await?;
    }
    Ok(())
}
