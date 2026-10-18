//! `migrate` (mapping B6; phase 8): move a workspace with its local disks to
//! another node, offline, and keep its power state.
//!
//! Ports `lifecycle.build_migrate_plan`, `migrate_workspace`,
//! `migration_volume_plan`, `migration_volume_key` and
//! `MigrationProgressTracker`. `qm migrate --with-local-disks
//! --targetstorage` is `POST /nodes/{source}/qemu/{vmid}/migrate`, a
//! `qmigrate` task whose log feeds the progress. With decision 1 = A every
//! node has the same two vendor snippets, so nothing is copied before the
//! shutdown (C3), and Proxmox writes the Cloud-Init drive at the start on the
//! target (no `qm cloudinit update`). A failure after this job shut the VM
//! down starts it again on the source node when it is still there.
//! `docs/migration/phase-8.md` lists what differs from Python.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use hs_model::NodeName;
use hs_model::config::Config;
use serde_json::{Map, Value, json};

use crate::error::CoreError;
use crate::inventory::{
    VmConfig, cluster_vm_resources, disk_option, parse_disk_size_gb, py_repr, resolve_homestack_storage, vm_config,
    vm_status, vm_volume_inventory,
};
use crate::lifecycle::{LifecycleTiming, require_pool, step_finished, step_started, text, vmid_of};
use crate::power::{SHUTDOWN_TIMEOUT_SECS, START_TIMEOUT_SECS, shutdown_vm, start_vm};
use crate::pve::Pve;
use crate::status::resolve_existing_workspace;
use crate::tasks::{LogTracker, Observer, TaskWait, TransferProgress, wait_task_tracking};
use crate::verify::verify_migrated_home;

/// Bound of `qm migrate` (Python: `timeout=7200`).
const MIGRATE_TIMEOUT: Duration = Duration::from_secs(7200);

/// The agent's time after the start on the target (Python: 180 s).
const AGENT_TIMEOUT: Duration = Duration::from_secs(180);

/// What `migrate` is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateRequest {
    pub vmid: u32,
    pub target_node: String,
    /// The first storage of the target node's layout when absent.
    pub target_storage: Option<String>,
}

/// `migration_volume_key`: the volume's base name without `.raw`, `.qcow2`
/// or `.vmdk`. Python's pattern never matches a dot, so it keeps them; the
/// owner decided (2026-10-13) that the server strips them.
#[must_use]
pub fn migration_volume_key(volume: &str) -> String {
    let tail = volume.split_once(':').map_or(volume, |(_, path)| path);
    let tail = tail.rsplit('/').next().unwrap_or(tail);
    [".raw", ".qcow2", ".vmdk"]
        .iter()
        .find_map(|extension| tail.strip_suffix(extension))
        .unwrap_or(tail)
        .to_owned()
}

/// `migration_volume_plan`: the volumes that move, without `unusedN`.
#[must_use]
pub fn migration_volume_plan(vm_cfg: &VmConfig, cfg: &Config) -> Vec<Value> {
    vm_volume_inventory(vm_cfg, cfg)
        .into_iter()
        .filter(|item| item.role != "unused")
        .map(|item| {
            let disk_cfg = vm_cfg.get(&item.slot).map(String::as_str).unwrap_or_default();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "Python's int()")]
            let size_bytes = parse_disk_size_gb(disk_cfg).map(|gib| (gib * 1024.0 * 1024.0 * 1024.0) as u64);
            json!({
                "slot": item.slot,
                "role": item.role,
                "volume": item.volume,
                "name": migration_volume_key(&item.volume),
                "size_bytes": size_bytes,
            })
        })
        .collect()
}

/// `ui.human_bytes`
fn human_bytes(value: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    #[allow(clippy::cast_precision_loss, reason = "a display")]
    let mut size = value as f64;
    let mut unit = UNITS[0];
    for candidate in UNITS {
        unit = candidate;
        if size < 1024.0 || candidate == UNITS[UNITS.len() - 1] {
            break;
        }
        size /= 1024.0;
    }
    if unit == "B" {
        return format!("{value} B");
    }
    format!("{size:.1} {unit}")
}

/// One reading of `MigrationProgressTracker.update`.
#[derive(Debug, Clone, PartialEq)]
pub struct MigrationProgress {
    /// Of the disk being copied, clamped to 0–100; `None` without its size.
    pub percent: Option<f64>,
    pub role: String,
    /// The logical volume being written.
    pub volume: String,
    pub copied_bytes: u64,
    pub total_bytes: Option<u64>,
    /// `  ↳ <role> · <copied> / <total> · <speed>`, as Python shows it.
    pub description: String,
}

/// `MigrationProgressTracker`: the disk being copied from the last
/// `Logical volume "<name>" created.`, its bytes from the last `dd` line
/// after it.
#[derive(Debug, Default)]
pub struct MigrationTracker {
    volumes: BTreeMap<String, (String, Option<u64>)>,
    current: Option<String>,
    current_bytes: u64,
    last: Option<(f64, String)>,
}

impl MigrationTracker {
    /// `volumes` as [`migration_volume_plan`] lists them.
    #[must_use]
    pub fn new(volumes: &[Value]) -> Self {
        let volumes = volumes
            .iter()
            .filter_map(|item| {
                let name = item.get("name").and_then(Value::as_str).filter(|n| !n.is_empty())?;
                let role = item.get("role").and_then(Value::as_str).unwrap_or("disk").to_owned();
                Some((name.to_owned(), (role, item.get("size_bytes").and_then(Value::as_u64))))
            })
            .collect();
        Self {
            volumes,
            ..Self::default()
        }
    }

    pub fn update(&mut self, output: &str) -> Option<MigrationProgress> {
        let mut tail = output;
        if let Some((name, end)) = last_created(output) {
            if self.current.as_deref() != Some(name) {
                self.current = Some(name.to_owned());
                self.current_bytes = 0;
            }
            tail = &output[end..];
        }
        let mut speed = None;
        if let Some((bytes, number, unit)) = last_copied(tail)
            && self.current.is_some()
        {
            self.current_bytes = bytes;
            speed = Some(format!("{number} {unit}"));
        }
        let current = self.current.clone()?;
        let (role, total) = self
            .volumes
            .get(&current)
            .map_or_else(|| ("disk".to_owned(), None), |(role, size)| (role.clone(), *size));
        let total = total.filter(|total| *total > 0);
        #[allow(clippy::cast_precision_loss, reason = "a percentage")]
        let percent =
            total.map(|total| (100.0 * self.current_bytes.min(total) as f64 / total as f64).clamp(0.0, 100.0));
        let mut detail = match total {
            Some(total) => format!("{} / {}", human_bytes(self.current_bytes), human_bytes(total)),
            None => human_bytes(self.current_bytes),
        };
        if let Some(speed) = speed {
            detail.push_str(" · ");
            detail.push_str(&speed);
        }
        Some(MigrationProgress {
            percent,
            description: format!("  ↳ {role} · {detail}"),
            role,
            volume: current,
            copied_bytes: self.current_bytes,
            total_bytes: total,
        })
    }
}

impl LogTracker for MigrationTracker {
    /// A job's progress needs a percentage: a disk without a size shows
    /// only in the task log. A reading equal to the last is not repeated.
    fn progress(&mut self, output: &str) -> Option<TransferProgress> {
        let progress = self.update(output)?;
        let percent = progress.percent?;
        let detail = progress.description.trim_start_matches("  ↳ ").to_owned();
        let reading = (percent, detail);
        if self.last.as_ref() == Some(&reading) {
            return None;
        }
        self.last = Some(reading.clone());
        Some(TransferProgress {
            percent: reading.0,
            detail: reading.1,
        })
    }
}

/// `Logical volume "([^"]+)" created\.`, last match: the name and where the
/// match ends.
fn last_created(text: &str) -> Option<(&str, usize)> {
    const OPEN: &str = "Logical volume \"";
    const CLOSE: &str = "\" created.";
    text.match_indices(OPEN)
        .filter_map(|(index, _)| {
            let start = index + OPEN.len();
            let quote = text[start..].find('"')?;
            let name = &text[start..start + quote];
            (!name.is_empty() && text[start + quote..].starts_with(CLOSE))
                .then_some((name, start + quote + CLOSE.len()))
        })
        .last()
}

/// `([0-9]+)\s+bytes\s+\([^)]+\)\s+copied,\s+[0-9.]+\s+s,\s+([0-9.]+)\s+([KMGT]?B/s)`,
/// last match: the bytes, the speed and its unit.
fn last_copied(text: &str) -> Option<(u64, &str, &str)> {
    let bytes = text.as_bytes();
    (0..bytes.len())
        .rev()
        .filter(|&at| bytes[at].is_ascii_digit() && (at == 0 || !bytes[at - 1].is_ascii_digit()))
        .find_map(|at| copied_at(&text[at..]))
}

fn copied_at(text: &str) -> Option<(u64, &str, &str)> {
    let mut rest = text;
    let count = take(&mut rest, |c| c.is_ascii_digit())?;
    spaces(&mut rest)?;
    rest = rest.strip_prefix("bytes")?;
    spaces(&mut rest)?;
    rest = rest.strip_prefix('(')?;
    take(&mut rest, |c| c != ')')?;
    rest = rest.strip_prefix(')')?;
    spaces(&mut rest)?;
    rest = rest.strip_prefix("copied,")?;
    spaces(&mut rest)?;
    take(&mut rest, |c| c.is_ascii_digit() || c == '.')?;
    spaces(&mut rest)?;
    rest = rest.strip_prefix("s,")?;
    spaces(&mut rest)?;
    let speed = take(&mut rest, |c| c.is_ascii_digit() || c == '.')?;
    spaces(&mut rest)?;
    let unit_start = rest;
    if let Some(after) = rest.strip_prefix(['K', 'M', 'G', 'T']) {
        rest = after;
    }
    rest = rest.strip_prefix("B/s")?;
    let unit = &unit_start[..unit_start.len() - rest.len()];
    Some((count.parse().ok()?, speed, unit))
}

/// `\s+`
fn spaces(rest: &mut &str) -> Option<()> {
    let trimmed = rest.trim_start();
    (trimmed.len() < rest.len()).then(|| *rest = trimmed)
}

/// One or more characters matching `accept`.
fn take<'a>(rest: &mut &'a str, accept: impl Fn(char) -> bool) -> Option<&'a str> {
    let end = rest.find(|c: char| !accept(c)).unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let (taken, after) = rest.split_at(end);
    *rest = after;
    Some(taken)
}

/// Check the disks Proxmox will move against the target's available bytes.
/// Source storage content reports virtual capacity even for sparse images;
/// missing sizes must never silently turn into a zero-byte requirement.
async fn require_migration_capacity(
    pve: &impl Pve,
    source: &str,
    target: &str,
    vmid: u32,
    storage: &str,
) -> Result<Value, CoreError> {
    let preflight = pve
        .get(&format!("/nodes/{source}/qemu/{vmid}/migrate"), &[("target", target)])
        .await?;
    let disks = preflight["local_disks"]
        .as_array()
        .ok_or_else(|| CoreError::Refused("Proxmox migration preflight did not return local disks".to_owned()))?;
    let mut contents = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut required = 0_u64;
    for disk in disks {
        let volume = disk["volid"].as_str().ok_or_else(|| {
            CoreError::Refused("Proxmox migration preflight returned a disk without a volume ID".to_owned())
        })?;
        if !seen.insert(volume) {
            continue;
        }
        let flag = |key: &str| disk[key] == true || disk[key] == 1;
        if flag("cdrom") && !flag("is_cloudinit") {
            return Err(CoreError::Refused(format!(
                "VM {vmid} has local CD-ROM {volume}; detach it before migration"
            )));
        }
        let (source_storage, _) = volume
            .split_once(':')
            .ok_or_else(|| CoreError::Refused(format!("Cannot determine storage for migration disk {volume}")))?;
        if !contents.contains_key(source_storage) {
            let content = pve
                .get(
                    &format!("/nodes/{source}/storage/{source_storage}/content"),
                    &[("content", "images")],
                )
                .await?;
            contents.insert(source_storage.to_owned(), content);
        }
        let size = contents[source_storage]
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["volid"].as_str() == Some(volume))
            .and_then(|item| item["size"].as_u64())
            .filter(|size| *size > 0)
            .ok_or_else(|| CoreError::Refused(format!("Cannot determine migration disk size for {volume}")))?;
        required = required
            .checked_add(size)
            .ok_or_else(|| CoreError::Refused("Migration disk sizes exceed the supported byte range".to_owned()))?;
    }
    crate::capacity::require_space(pve, target, storage, required).await
}

async fn power_state(pve: &impl Pve, node: &str, vmid: u32) -> Result<String, CoreError> {
    let status = vm_status(pve, node, vmid).await;
    if !matches!(status.as_str(), "running" | "stopped") {
        return Err(CoreError::Refused(format!(
            "Cannot determine current power state of VM {vmid}"
        )));
    }
    Ok(status)
}

/// `build_migrate_plan` for a workspace of `pool`.
pub async fn plan_migrate(
    pve: &impl Pve,
    cfg: &Config,
    pool: &str,
    request: &MigrateRequest,
) -> Result<Value, CoreError> {
    let vmid = request.vmid;
    let target = NodeName::parse(&request.target_node)
        .map_err(|_| CoreError::Invalid(format!("Invalid node name {}", py_repr(Some(&request.target_node)))))?
        .to_string();
    require_pool(pve, vmid, pool).await?;
    let info = resolve_existing_workspace(pve, cfg, vmid).await?;
    if target == info.node {
        return Err(CoreError::Refused(format!("VM {vmid} is already on node {target}")));
    }
    let unused: Vec<String> = info
        .vm_config
        .iter()
        .filter(|(key, _)| {
            key.strip_prefix("unused")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(|(key, value)| format!("{key}={}", value.split(',').next().unwrap_or_default()))
        .collect();
    if !unused.is_empty() {
        return Err(CoreError::Refused(format!(
            "VM {vmid} has detached/unused disk references ({}); \
             Proxmox migration scans them too. Clean or resolve them before migration.",
            unused.join(", ")
        )));
    }
    let target_storage = resolve_homestack_storage(cfg, &target, request.target_storage.as_deref())?;
    if pve.get(&format!("/nodes/{target}/status"), &[]).await.is_err() {
        return Err(CoreError::Refused(format!(
            "Target node {} is not available in the Proxmox cluster",
            py_repr(Some(&target))
        )));
    }
    let capacity = require_migration_capacity(pve, &info.node, &target, vmid, &target_storage).await?;
    // Cluster inventory can lag behind a completed stop or start operation.
    let status = power_state(pve, &info.node, vmid).await?;
    Ok(json!({
        "command": "migrate",
        "capacity": capacity,
        "vmid": vmid,
        "name": info.name,
        "pool": pool,
        "source_node": info.node,
        "target_node": target,
        "target_storage": target_storage,
        "status": status,
        "home_disk": cfg.home_disk,
        "home_label": info.home_label,
        "home_volume": info.home_volume,
        "volumes": migration_volume_plan(&info.vm_config, cfg),
        "migration_mode": "offline; all local disks including persistent home",
        "snippet_policy": "none: the static vendor snippets are on every node (decision 1 = A)",
    }))
}

/// What the execution needs from a plan.
struct Planned<'p> {
    vmid: u32,
    source: &'p str,
    target: &'p str,
    storage: &'p str,
    home_label: &'p str,
    was_running: bool,
    volumes: Vec<Value>,
}

/// `migrate_workspace` for a plan of [`plan_migrate`]: the workspace is
/// checked again (tags, pool, node, home) before the shutdown.
pub async fn migrate_workspace(
    pve: &impl Pve,
    cfg: &Config,
    pool: &str,
    plan: &Value,
    timing: &LifecycleTiming,
    observer: &impl Observer,
) -> Result<Value, CoreError> {
    let planned = Planned {
        vmid: vmid_of(plan)?,
        source: text(plan, "source_node")?,
        target: text(plan, "target_node")?,
        storage: text(plan, "target_storage")?,
        home_label: text(plan, "home_label")?,
        was_running: text(plan, "status")? == "running",
        volumes: plan
            .get("volumes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    };
    let vmid = planned.vmid;
    let info = resolve_existing_workspace(pve, cfg, vmid).await?;
    require_pool(pve, vmid, pool).await?;
    if info.node != planned.source {
        return Err(CoreError::Refused(format!(
            "VM {vmid} is on {}, not on {} as planned; generate a new plan",
            info.node, planned.source
        )));
    }
    if plan.get("home_volume").and_then(Value::as_str) != Some(info.home_volume.as_str()) {
        return Err(CoreError::Refused(
            "Workspace home volume changed after migrate confirmation; generate a new plan".to_owned(),
        ));
    }
    if power_state(pve, planned.source, vmid).await? != text(plan, "status")? {
        return Err(CoreError::Refused(
            "Workspace power state changed after migrate confirmation; generate a new plan".to_owned(),
        ));
    }

    require_migration_capacity(pve, planned.source, planned.target, vmid, planned.storage).await?;
    let mut source_stopped = false;
    let outcome = migrate_steps(pve, cfg, &planned, timing, observer, &mut source_stopped).await;
    let status = match outcome {
        Ok(status) => status,
        Err(error) if planned.was_running && source_stopped => {
            let note = recover(pve, &planned, timing, observer).await;
            return Err(if note.is_empty() {
                error
            } else {
                CoreError::Failed(format!("{error}{note}"))
            });
        }
        Err(error) => return Err(error),
    };
    let mut result: Map<String, Value> = plan.as_object().cloned().unwrap_or_default();
    result.insert("ok".to_owned(), json!(true));
    result.insert("status".to_owned(), json!(status));
    result.insert("snippets_copied".to_owned(), json!([]));
    result.insert("home_preserved".to_owned(), json!(true));
    result.insert("power_state_preserved".to_owned(), json!(true));
    result.insert("event".to_owned(), json!("workspace.migrated"));
    Ok(Value::Object(result))
}

/// The steps of `migrate_workspace` from the shutdown to the final power
/// state, which it returns. `source_stopped` tells whether this job shut
/// the VM down.
async fn migrate_steps(
    pve: &impl Pve,
    cfg: &Config,
    planned: &Planned<'_>,
    timing: &LifecycleTiming,
    observer: &impl Observer,
    source_stopped: &mut bool,
) -> Result<String, CoreError> {
    let Planned {
        vmid,
        source,
        target,
        storage,
        home_label,
        was_running,
        ..
    } = *planned;
    if was_running {
        let step = "Shutdown workspace";
        step_started(observer, step);
        let stopped = shutdown_vm(pve, source, vmid, SHUTDOWN_TIMEOUT_SECS, &timing.wait(600), observer).await;
        step_finished(observer, step, stopped.is_ok());
        stopped?;
        *source_stopped = true;
    }

    let step = "Migrate local disks";
    step_started(observer, step);
    let migrated = migrate_offline(pve, source, target, vmid, storage, &planned.volumes, timing, observer).await;
    step_finished(observer, step, migrated.is_ok());
    migrated?;

    let step = "Verify target configuration";
    step_started(observer, step);
    let verified = async {
        let actual = cluster_vm_resources(pve)
            .await?
            .into_iter()
            .find(|resource| resource.vmid == vmid)
            .map(|resource| resource.node)
            .unwrap_or_default();
        if actual != target {
            return Err(CoreError::Failed(format!(
                "Migration command completed but VM {vmid} is on {}, expected {}",
                py_repr(Some(&actual)),
                py_repr(Some(target))
            )));
        }
        let target_cfg = vm_config(pve, target, vmid).await?;
        let home = target_cfg.get(&cfg.home_disk).map(String::as_str).unwrap_or_default();
        if disk_option(home, "serial").as_deref() != Some(home_label) {
            return Err(CoreError::Failed(format!(
                "Persistent home identity check failed after migration to {target}"
            )));
        }
        Ok(())
    }
    .await;
    step_finished(observer, step, verified.is_ok());
    verified?;

    if was_running {
        let step = "Start workspace";
        step_started(observer, step);
        let started = start_vm(pve, target, vmid, &timing.wait(START_TIMEOUT_SECS), observer).await;
        step_finished(observer, step, started.is_ok());
        started?;

        let step = "Verify guest and persistent home";
        step_started(observer, step);
        let home = verify_migrated_home(pve, target, vmid, &cfg.user.name, home_label, AGENT_TIMEOUT).await;
        step_finished(observer, step, home.is_ok());
        home?;
    }

    let step = "Verify final power state";
    step_started(observer, step);
    let status = vm_status(pve, target, vmid).await;
    let desired = if was_running { "running" } else { "stopped" };
    let kept = status == desired;
    step_finished(observer, step, kept);
    if !kept {
        return Err(CoreError::Failed(format!(
            "Migration completed but VM {vmid} power state is {}; expected {} to preserve its pre-migration state",
            py_repr(Some(&status)),
            py_repr(Some(desired))
        )));
    }
    Ok(status)
}

/// `qm migrate VMID TARGET --with-local-disks --targetstorage STORAGE` of a
/// stopped VM: the `qmigrate` task, its log feeding the progress of
/// `volumes` ([`migration_volume_plan`]); tried again while the VM's lock
/// is taken.
#[allow(clippy::too_many_arguments, reason = "one Proxmox call and its tracking")]
pub(crate) async fn migrate_offline(
    pve: &impl Pve,
    source: &str,
    target: &str,
    vmid: u32,
    storage: &str,
    volumes: &[Value],
    timing: &LifecycleTiming,
    observer: &impl Observer,
) -> Result<(), CoreError> {
    crate::tasks::retry_while_locked(timing.poll, || async {
        let answer = pve
            .post(
                &format!("/nodes/{source}/qemu/{vmid}/migrate"),
                &[
                    ("target", target),
                    ("with-local-disks", "1"),
                    ("targetstorage", storage),
                ],
            )
            .await?;
        let upid = answer
            .as_str()
            .ok_or_else(|| CoreError::Unusable(format!("Proxmox did not return a task id: {answer}")))?;
        let wait = TaskWait {
            poll: timing.poll,
            timeout: MIGRATE_TIMEOUT,
        };
        let mut tracker = MigrationTracker::new(volumes);
        let result = wait_task_tracking(pve, upid, &wait, observer, &mut tracker).await?;
        if result.ok() {
            Ok(())
        } else {
            Err(CoreError::Failed(format!(
                "Proxmox task qmigrate for VM {vmid} on {source} failed: {}",
                result.exitstatus
            )))
        }
    })
    .await
}

/// Python's `except` branch: start the VM again on the source node when it
/// is still there and not running; the note for the error.
async fn recover(pve: &impl Pve, planned: &Planned<'_>, timing: &LifecycleTiming, observer: &impl Observer) -> String {
    let (vmid, source) = (planned.vmid, planned.source);
    let resources = match cluster_vm_resources(pve).await {
        Ok(resources) => resources,
        Err(error) => return format!("\nRecovery check failed for VM {vmid} on {source}: {error}"),
    };
    let resource = resources.into_iter().find(|resource| resource.vmid == vmid);
    let node = resource.as_ref().map(|r| r.node.as_str()).unwrap_or_default();
    let status = resource.as_ref().and_then(|r| r.status.as_deref()).unwrap_or_default();
    if node != source || status == "running" {
        return String::new();
    }
    let step = "Restart workspace on the source node";
    step_started(observer, step);
    let restarted = start_vm(pve, source, vmid, &timing.wait(START_TIMEOUT_SECS), observer).await;
    step_finished(observer, step, restarted.is_ok());
    match restarted {
        Ok(()) => format!("\nRecovery: VM {vmid} was restarted automatically on {source}."),
        Err(error) => format!("\nRecovery: VM {vmid} is still on {source}, but automatic restart failed: {error}"),
    }
}
