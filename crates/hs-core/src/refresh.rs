//! `refresh` and the recovery of an interrupted one (mapping B3, B4, E3;
//! phase 6).
//!
//! Ports `lifecycle.build_refresh_plan`, `refresh_workspace`,
//! `_recover_refresh` and the journal functions onto the API, with
//! decision 4 = B: the journal lives in the VM's `description`, written with
//! the `digest` of the config each step was checked against, and the
//! owner's rules for it (written before each step, removed only after the
//! result is verified, a later `refresh` compares every recorded stage with
//! the VM before doing anything, every mismatch is reported and nothing
//! changes while there is one, and `force`). Gold's root is copied with
//! `import-from` into a free slot while the workspace runs and then
//! unlinked to `unusedN` (variant A of the phase 6 probe). `PUT /unlink`
//! takes no `digest` (owner's decision of 2026-10-13): the slot is checked
//! right before. `docs/migration/phase-6.md` lists what differs from
//! Python.

use std::collections::BTreeMap;

use base64::Engine;
use hs_model::config::Config;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::cloudinit::{HomeFormat, cloud_init_settings};
use crate::error::CoreError;
use crate::gold::{boot_order_contains_disk, extract_mac};
use crate::inventory::{
    ALIEN_WORKSPACE_TAG, REFRESH_LOCK_TAG, VmConfig, cluster_vm_resources, disk_option, disk_volume, has_tag,
    home_label, parse_disk_size_gb, py_repr, resolve_homestack_storage, set_vm_config, vm_config, vm_status,
};
use crate::lifecycle::{
    LifecycleTiming, number, post_task, require_gold_tag, require_pool, step_finished, step_started, text,
    verify_workspace_role_tags, vmid_of,
};
use crate::power::{SHUTDOWN_TIMEOUT_SECS, START_TIMEOUT_SECS, shutdown_vm, start_vm};
use crate::pve::Pve;
use crate::status::resolve_existing_workspace;
use crate::tasks::Observer;
use crate::verify::{GuestExpectation, verify_refreshed_guest};

/// `proxmox.require_refresh_unlocked`
pub fn require_refresh_unlocked(vmid: u32, vm_cfg: &VmConfig) -> Result<(), CoreError> {
    let tags = vm_cfg.get("tags").map(String::as_str);
    if has_tag(tags, ALIEN_WORKSPACE_TAG) {
        return Err(CoreError::Refused(format!(
            "Refresh of VM {vmid} is blocked for alien workspace role tag '{ALIEN_WORKSPACE_TAG}'."
        )));
    }
    if has_tag(tags, REFRESH_LOCK_TAG) {
        return Err(CoreError::Refused(format!(
            "Refresh of VM {vmid} is blocked by tag '{REFRESH_LOCK_TAG}'. \
             Remove this tag from the VM in Proxmox before refreshing."
        )));
    }
    Ok(())
}

/// `guest.parse_ipconfig0`: `(ip, cidr, gateway)` of a static `ipconfig0`.
pub fn parse_ipconfig0(value: &str) -> Result<(String, u8, String), CoreError> {
    let fields: BTreeMap<&str, &str> = value
        .split(',')
        .filter_map(|item| item.split_once('='))
        .map(|(key, raw)| (key.trim(), raw.trim()))
        .collect();
    let ip = fields.get("ip").copied().unwrap_or_default();
    let Some((address, cidr)) = ip.rsplit_once('/').filter(|_| ip != "dhcp") else {
        return Err(CoreError::Unusable(format!(
            "Workspace ipconfig0 does not contain a static IPv4 address: {}",
            py_repr(Some(value))
        )));
    };
    let cidr: u8 = cidr
        .parse()
        .map_err(|_| CoreError::Unusable(format!("Cannot parse CIDR from ipconfig0: {}", py_repr(Some(value)))))?;
    let gateway = fields.get("gw").copied().unwrap_or_default();
    if gateway.is_empty() {
        return Err(CoreError::Unusable(format!(
            "Workspace ipconfig0 does not contain a gateway: {}",
            py_repr(Some(value))
        )));
    }
    Ok((address.to_owned(), cidr, gateway.to_owned()))
}

// --- the journal -------------------------------------------------------------

/// The journal's phases, in order (`_validate_refresh_journal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Prepared,
    Imported,
    Switching,
    Switched,
    Verifying,
    Verified,
}

impl Phase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Imported => "imported",
            Self::Switching => "switching",
            Self::Switched => "switched",
            Self::Verifying => "verifying",
            Self::Verified => "verified",
        }
    }
}

/// The power state before the refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PowerState {
    Running,
    Stopped,
}

impl PowerState {
    fn parse(status: &str) -> Option<Self> {
        match status {
            "running" => Some(Self::Running),
            "stopped" => Some(Self::Stopped),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
        }
    }
}

/// The transaction journal of one refresh, kept in the VM's `description`.
/// Python's fields, with `staging_slot` (the slot `import-from` copies
/// into), `gold_root_disk_config` (for attaching the copy) and `restore`
/// (every option refresh sets, as it was; empty when it was not set) in
/// place of Python's `boot`, `ipconfig0` and `cicustom`, and
/// `rolling_back`, set before a recovery changes anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub version: u32,
    pub transaction_id: String,
    pub phase: Phase,
    pub vmid: u32,
    pub name: String,
    pub node: String,
    pub initial_status: PowerState,
    pub old_root_spec: String,
    pub old_root_volume: String,
    pub root_disk: String,
    pub staging_slot: String,
    pub gold_root_disk_config: String,
    pub new_root_volume: String,
    pub new_unused_key: String,
    pub old_unused_key: String,
    pub home_volume: String,
    pub home_label: String,
    pub home_disk: String,
    pub restore: BTreeMap<String, String>,
    #[serde(default)]
    pub rolling_back: bool,
}

const JOURNAL_VERSION: u32 = 1;
const JOURNAL_START: &str = "<!-- homestack-journal: ";
const JOURNAL_END: &str = " -->";
/// PVE's limit on `description` (phase 6 probe).
const DESCRIPTION_MAX: usize = 8192;
/// The options `refresh` sets, which a rollback restores.
const RESTORED_OPTIONS: [&str; 6] = ["boot", "ipconfig0", "nameserver", "ciuser", "ciupgrade", "cicustom"];

fn is_unused_key(key: &str) -> bool {
    key.strip_prefix("unused")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn is_scsi_key(key: &str) -> bool {
    key.strip_prefix("scsi")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn has_storage(volume: &str) -> bool {
    volume.contains(':')
}

/// The key a disk whose unlink Proxmox has pending gets in
/// [`effective_config`].
const PENDING_UNUSED: &str = "unused-pending-";

fn is_disk_slot(key: &str) -> bool {
    ["scsi", "virtio", "sata", "ide", "efidisk", "tpmstate"]
        .iter()
        .any(|prefix| {
            key.strip_prefix(prefix)
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// A VM's config as it is once Proxmox applies its pending changes.
/// `PUT /unlink` and config changes write a pending change first and apply
/// it after (`update_vm_api`); a call cut off in between leaves it pending.
/// `GET config` shows pending values but drops a disk whose deletion is
/// pending; here such a disk appears as `unused-pending-<slot>` (where an
/// unlink without `force` puts it), and one pending for physical deletion
/// is left out. Any config change of a stopped VM applies them.
async fn effective_config(pve: &impl Pve, node: &str, vmid: u32) -> Result<VmConfig, CoreError> {
    let mut vm_cfg = vm_config(pve, node, vmid).await?;
    let pending = pve.get(&format!("/nodes/{node}/qemu/{vmid}/pending"), &[]).await?;
    for entry in pending.as_array().into_iter().flatten() {
        let delete = entry.get("delete").and_then(Value::as_u64).unwrap_or(0);
        let Some(key) = entry.get("key").and_then(Value::as_str).filter(|_| delete != 0) else {
            continue;
        };
        vm_cfg.remove(key);
        let value = entry.get("value").and_then(Value::as_str).unwrap_or_default();
        if delete == 1 && is_disk_slot(key) && has_storage(disk_volume(value)) {
            vm_cfg.insert(format!("{PENDING_UNUSED}{key}"), value.to_owned());
        }
    }
    Ok(vm_cfg)
}

fn has_pending(vm_cfg: &VmConfig) -> bool {
    vm_cfg.keys().any(|key| key.starts_with(PENDING_UNUSED))
}

/// `_validate_refresh_journal` for the server's journal.
fn validate_journal(cfg: &Config, journal: &Journal) -> Result<(), CoreError> {
    let invalid = |what: &str| Err(CoreError::Unusable(format!("Refresh journal has an invalid {what}")));
    if journal.version != JOURNAL_VERSION {
        return Err(CoreError::Unusable(
            "Refresh journal has an unsupported version".to_owned(),
        ));
    }
    let id = &journal.transaction_id;
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return invalid("transaction identity");
    }
    let node_ok = journal.node.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && journal
            .node
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if !node_ok {
        return invalid("node");
    }
    if journal.root_disk != cfg.root_disk || journal.home_disk != cfg.home_disk {
        return Err(CoreError::Unusable(
            "Current disk-slot configuration does not match the refresh journal".to_owned(),
        ));
    }
    if cfg.root_disk == cfg.home_disk {
        return Err(CoreError::Unusable(
            "Root and persistent-home disk slots must be different".to_owned(),
        ));
    }
    if !is_scsi_key(&journal.staging_slot) || [&cfg.root_disk, &cfg.home_disk].contains(&&journal.staging_slot) {
        return invalid("staging disk slot");
    }
    let old = &journal.old_root_volume;
    if !has_storage(old) || disk_volume(&journal.old_root_spec) != old {
        return invalid("original root identity");
    }
    let home = &journal.home_volume;
    if !has_storage(home) || home == old {
        return invalid("persistent-home identity");
    }
    if journal.home_label != home_label(journal.vmid)? {
        return invalid("persistent-home label");
    }
    let new = &journal.new_root_volume;
    if !new.is_empty() && (!has_storage(new) || new == old || new == home) {
        return invalid("staged root identity");
    }
    if journal.phase != Phase::Prepared && new.is_empty() {
        return Err(CoreError::Unusable(
            "Refresh journal is missing the staged root identity".to_owned(),
        ));
    }
    for key in [&journal.new_unused_key, &journal.old_unused_key] {
        if !key.is_empty() && !is_unused_key(key) {
            return invalid("staging slot");
        }
    }
    if journal
        .restore
        .keys()
        .any(|key| !RESTORED_OPTIONS.contains(&key.as_str()))
    {
        return invalid("set of restored options");
    }
    Ok(())
}

/// The VM's `description` without the journal block, and the block's
/// payload.
fn split_description(description: &str) -> Result<(String, Option<&str>), CoreError> {
    let Some(start) = description.find(JOURNAL_START) else {
        return Ok((description.to_owned(), None));
    };
    let after = &description[start + JOURNAL_START.len()..];
    let end = after
        .find(JOURNAL_END)
        .ok_or_else(|| CoreError::Unusable("The refresh journal in the VM description is cut off".to_owned()))?;
    let rest = format!(
        "{}{}",
        description[..start].trim_end(),
        &after[end + JOURNAL_END.len()..]
    );
    Ok((rest, Some(&after[..end])))
}

/// `_read_refresh_journal`: the journal in a VM's config, validated.
pub fn read_journal(cfg: &Config, vmid: u32, vm_cfg: &VmConfig) -> Result<Option<Journal>, CoreError> {
    let description = vm_cfg.get("description").map(String::as_str).unwrap_or_default();
    let (_, Some(payload)) = split_description(description)? else {
        return Ok(None);
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .map_err(|_| CoreError::Unusable(format!("The refresh journal of VM {vmid} is not valid base64")))?;
    let journal: Journal = serde_json::from_slice(&bytes)
        .map_err(|_| CoreError::Unusable(format!("The refresh journal of VM {vmid} is not a valid journal")))?;
    if journal.vmid != vmid {
        return Err(CoreError::Unusable(format!(
            "The refresh journal of VM {vmid} has an invalid VM identity"
        )));
    }
    validate_journal(cfg, &journal)?;
    Ok(Some(journal))
}

/// `description` with `journal` in place of any journal block.
fn description_with(current: &str, journal: Option<&Journal>) -> Result<String, CoreError> {
    let (rest, _) = split_description(current)?;
    let Some(journal) = journal else {
        return Ok(rest);
    };
    let json = serde_json::to_vec(journal).map_err(|error| CoreError::Failed(error.to_string()))?;
    let payload = base64::engine::general_purpose::STANDARD.encode(json);
    let separator = if rest.is_empty() { "" } else { "\n" };
    Ok(format!("{rest}{separator}{JOURNAL_START}{payload}{JOURNAL_END}"))
}

/// `_write_refresh_journal` / `_remove_refresh_journal`: write (or remove)
/// the journal with the `digest` of `vm_cfg`, the config the next step was
/// checked against, and return the config after the write. A config changed
/// by anyone since it was read fails the write, and nothing else happens.
async fn write_journal(
    pve: &impl Pve,
    node: &str,
    vmid: u32,
    vm_cfg: &VmConfig,
    journal: Option<&Journal>,
) -> Result<VmConfig, CoreError> {
    let current = vm_cfg.get("description").map(String::as_str).unwrap_or_default();
    let description = description_with(current, journal)?;
    if description.chars().count() > DESCRIPTION_MAX {
        return Err(CoreError::Failed(format!(
            "The refresh journal does not fit in the description of VM {vmid} ({DESCRIPTION_MAX} characters at most)"
        )));
    }
    let digest = vm_cfg.get("digest").map(String::as_str);
    if description.is_empty() {
        set_vm_config(pve, node, vmid, &[("delete", "description")], digest).await?;
    } else {
        set_vm_config(pve, node, vmid, &[("description", &description)], digest).await?;
    }
    vm_config(pve, node, vmid).await
}

// --- where the journal's volumes are ----------------------------------------

/// Where a volume is in a VM's config.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Place {
    Root,
    Staging,
    Unused(String),
    /// Attached in a slot refresh does not use.
    Other(String),
    Absent,
}

fn place_of(journal: &Journal, vm_cfg: &VmConfig, volume: &str) -> Place {
    let slots: Vec<&String> = vm_cfg
        .iter()
        .filter(|(key, value)| key.as_str() != "description" && disk_volume(value) == volume)
        .map(|(key, _)| key)
        .collect();
    match slots.as_slice() {
        [] => Place::Absent,
        [slot] if **slot == journal.root_disk => Place::Root,
        [slot] if **slot == journal.staging_slot => Place::Staging,
        [slot] if is_unused_key(slot) || slot.starts_with(PENDING_UNUSED) => Place::Unused((*slot).clone()),
        [slot] => Place::Other((*slot).clone()),
        many => Place::Other(many.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")),
    }
}

fn describe(place: &Place) -> String {
    match place {
        Place::Root => "the root slot".to_owned(),
        Place::Staging => "the staging slot".to_owned(),
        Place::Unused(key) => key.clone(),
        Place::Other(slots) => format!("slot {slots}"),
        Place::Absent => "not in the VM's config".to_owned(),
    }
}

/// The staged root: the journal's, or, before it was recorded, a volume in
/// the staging slot other than the old root and the home.
fn staged_volume(journal: &Journal, vm_cfg: &VmConfig) -> Option<String> {
    if !journal.new_root_volume.is_empty() {
        return Some(journal.new_root_volume.clone());
    }
    vm_cfg
        .get(&journal.staging_slot)
        .map(|value| disk_volume(value).to_owned())
        .filter(|volume| !volume.is_empty() && *volume != journal.old_root_volume && *volume != journal.home_volume)
}

fn home_mismatch(journal: &Journal, vm_cfg: &VmConfig) -> Option<String> {
    let home = vm_cfg.get(&journal.home_disk).map(String::as_str).unwrap_or_default();
    let volume = disk_volume(home);
    let serial = disk_option(home, "serial");
    (volume != journal.home_volume || serial.as_deref() != Some(journal.home_label.as_str())).then(|| {
        format!(
            "{} holds {} with serial {}; the journal names {} with serial {}",
            journal.home_disk,
            py_repr(Some(volume)),
            py_repr(serial.as_deref()),
            py_repr(Some(&journal.home_volume)),
            py_repr(Some(&journal.home_label))
        )
    })
}

/// Rule 3 of decision 4: every way the VM's config disagrees with the
/// journal's phase (`phase-6.md`, section 3). Empty when they agree.
#[must_use]
pub fn journal_mismatches(journal: &Journal, node: &str, vm_cfg: &VmConfig) -> Vec<String> {
    let mut found = Vec::new();
    if node != journal.node {
        found.push(format!(
            "the VM is on {}, the journal on {}",
            py_repr(Some(node)),
            py_repr(Some(&journal.node))
        ));
    }
    if let Some(home) = home_mismatch(journal, vm_cfg) {
        found.push(home);
    }
    let old = place_of(journal, vm_cfg, &journal.old_root_volume);
    let staged = staged_volume(journal, vm_cfg);
    let new = staged
        .as_deref()
        .map_or(Place::Absent, |volume| place_of(journal, vm_cfg, volume));
    let root = vm_cfg
        .get(&journal.root_disk)
        .map(|value| disk_volume(value).to_owned())
        .unwrap_or_default();
    let unused = |place: &Place| matches!(place, Place::Unused(_));
    let (old_ok, new_ok) = if journal.rolling_back {
        (
            old == Place::Root || unused(&old),
            matches!(new, Place::Root | Place::Staging | Place::Absent) || unused(&new),
        )
    } else {
        match journal.phase {
            Phase::Prepared => (old == Place::Root, matches!(new, Place::Absent | Place::Staging)),
            Phase::Imported => (old == Place::Root, new == Place::Staging),
            Phase::Switching => match (&old, &new) {
                (Place::Root, new) => (true, *new == Place::Staging || unused(new)),
                (old, Place::Root) => (unused(old), true),
                (old, new) => (unused(old) && root.is_empty(), unused(new)),
            },
            Phase::Switched | Phase::Verifying => (unused(&old), new == Place::Root),
            Phase::Verified => (unused(&old) || old == Place::Absent, new == Place::Root),
        }
    };
    let what = if journal.rolling_back {
        "a rollback".to_owned()
    } else {
        format!("phase {}", journal.phase.as_str())
    };
    if !old_ok {
        found.push(format!(
            "the original root {} is {}, which does not agree with {what}",
            py_repr(Some(&journal.old_root_volume)),
            describe(&old)
        ));
    }
    if !new_ok {
        let name = staged.as_deref().unwrap_or("(not recorded)");
        found.push(format!(
            "the new root {} is {}, which does not agree with {what}",
            py_repr(Some(name)),
            describe(&new)
        ));
    }
    let known = [Some(journal.old_root_volume.as_str()), staged.as_deref()];
    if !root.is_empty() && !known.contains(&Some(root.as_str())) {
        found.push(format!(
            "the root slot {} holds {}, which the journal does not know",
            journal.root_disk,
            py_repr(Some(&root))
        ));
    }
    if let Some(lock) = vm_cfg.get("lock").filter(|lock| !lock.is_empty()) {
        found.push(format!("the VM is locked ({lock})"));
    }
    found
}

fn unused_key_of(vm_cfg: &VmConfig, volume: &str) -> Option<String> {
    vm_cfg
        .iter()
        .find(|(key, value)| is_unused_key(key) && disk_volume(value) == volume)
        .map(|(key, _)| key.clone())
}

/// The highest `scsiN` (N ≤ 30, PVE's limit) the config does not use.
fn free_staging_slot(vm_cfg: &VmConfig) -> Result<String, CoreError> {
    (1..=30)
        .rev()
        .map(|index| format!("scsi{index}"))
        .find(|key| !vm_cfg.contains_key(key))
        .ok_or_else(|| CoreError::Failed("VM has no free SCSI slot for a staged root".to_owned()))
}

/// `_root_attachment_spec`: the volume with Gold's root options.
#[must_use]
pub fn root_attachment_spec(volume: &str, gold_disk_config: &str) -> String {
    let mut parts = vec![volume.to_owned()];
    for key in [
        "iothread",
        "discard",
        "ssd",
        "cache",
        "aio",
        "backup",
        "replicate",
        "ro",
    ] {
        if let Some(value) = disk_option(gold_disk_config, key) {
            parts.push(format!("{key}={value}"));
        }
    }
    parts.join(",")
}

// --- Proxmox calls of the steps --------------------------------------------

struct Vm<'a, P: Pve, O: Observer> {
    pve: &'a P,
    node: &'a str,
    vmid: u32,
    timing: &'a LifecycleTiming,
    observer: &'a O,
}

impl<P: Pve, O: Observer> Vm<'_, P, O> {
    async fn config(&self) -> Result<VmConfig, CoreError> {
        effective_config(self.pve, self.node, self.vmid).await
    }

    async fn write(&self, vm_cfg: &VmConfig, journal: Option<&Journal>) -> Result<VmConfig, CoreError> {
        write_journal(self.pve, self.node, self.vmid, vm_cfg, journal).await
    }

    /// `POST config` with the `digest` of `vm_cfg`; returns the new config.
    async fn set(&self, vm_cfg: &VmConfig, values: &[(&str, &str)], what: &str) -> Result<VmConfig, CoreError> {
        let mut form = values.to_vec();
        if let Some(digest) = vm_cfg.get("digest").filter(|d| !d.is_empty()) {
            form.push(("digest", digest));
        }
        post_task(
            self.pve,
            &format!("/nodes/{}/qemu/{}/config", self.node, self.vmid),
            &form,
            what,
            &self.timing.wait(3600),
            self.observer,
        )
        .await?;
        self.config().await
    }

    /// `PUT /unlink` of `slot` after checking, on a config read right
    /// before, that it holds `volume` (owner's decision: no `digest`).
    async fn unlink(&self, slot: &str, volume: &str, force: bool) -> Result<VmConfig, CoreError> {
        let vm_cfg = self.config().await?;
        let found = vm_cfg.get(slot).map(|value| disk_volume(value)).unwrap_or_default();
        if found != volume {
            return Err(CoreError::Failed(format!(
                "Refusing to unlink {slot} of VM {}: it holds {}, expected {}",
                self.vmid,
                py_repr(Some(found)),
                py_repr(Some(volume))
            )));
        }
        let mut form = vec![("idlist", slot)];
        if force {
            form.push(("force", "1"));
        }
        let path = format!("/nodes/{}/qemu/{}/unlink", self.node, self.vmid);
        crate::tasks::retry_while_locked(self.timing.poll, || async { Ok(self.pve.put(&path, &form).await?) }).await?;
        self.config().await
    }

    async fn status(&self) -> String {
        vm_status(self.pve, self.node, self.vmid).await
    }

    async fn shutdown(&self) -> Result<(), CoreError> {
        shutdown_vm(
            self.pve,
            self.node,
            self.vmid,
            SHUTDOWN_TIMEOUT_SECS,
            &self.timing.wait(600),
            self.observer,
        )
        .await
    }

    async fn start(&self) -> Result<(), CoreError> {
        start_vm(
            self.pve,
            self.node,
            self.vmid,
            &self.timing.wait(START_TIMEOUT_SECS),
            self.observer,
        )
        .await
    }
}

fn require_home(journal: &Journal, vm_cfg: &VmConfig) -> Result<(), CoreError> {
    if home_mismatch(journal, vm_cfg).is_some() {
        return Err(CoreError::Failed(format!(
            "Persistent home identity changed during refresh of VM {}; refusing further disk changes",
            journal.vmid
        )));
    }
    Ok(())
}

fn root_of(cfg: &Config, vm_cfg: &VmConfig) -> String {
    vm_cfg
        .get(&cfg.root_disk)
        .map(|value| disk_volume(value).to_owned())
        .unwrap_or_default()
}

// --- plan --------------------------------------------------------------------

/// `build_refresh_plan` for a workspace in `pool`. With a journal in the
/// VM's description the plan is `mode="recover"` and lists every way the VM
/// disagrees with it; that is refused unless `force` (rule 5 of decision 4).
/// `force` without a journal is refused.
pub async fn plan_refresh(
    pve: &impl Pve,
    cfg: &Config,
    pool: &str,
    vmid: u32,
    force: bool,
) -> Result<Value, CoreError> {
    let gold = crate::gold::configured_gold(cfg)?;
    require_pool(pve, vmid, pool).await?;
    let resource = cluster_vm_resources(pve)
        .await?
        .into_iter()
        .find(|r| r.vmid == vmid)
        .ok_or_else(|| CoreError::NotFound(format!("VMID {vmid} does not exist")))?;
    if resource.node.is_empty() {
        return Err(CoreError::Unusable(format!(
            "VM {vmid} has no node in cluster inventory"
        )));
    }
    let node = resource.node.clone();
    let vm_cfg = effective_config(pve, &node, vmid).await?;
    require_refresh_unlocked(vmid, &vm_cfg)?;
    if let Some(journal) = read_journal(cfg, vmid, &vm_cfg)? {
        if journal.node != node {
            return Err(CoreError::Failed(format!(
                "Refresh journal belongs to node {}, but VM {vmid} is on {}",
                py_repr(Some(&journal.node)),
                py_repr(Some(&node))
            )));
        }
        let mismatches = journal_mismatches(&journal, &node, &vm_cfg);
        if !mismatches.is_empty() && !force {
            return Err(CoreError::Refused(format!(
                "VM {vmid} does not agree with its refresh journal ({}); nothing was changed. \
                 Fix it by hand, or plan again with force to attach the new root without deleting any volume",
                mismatches.join("; ")
            )));
        }
        let status = resource.status.clone().unwrap_or_else(|| "unknown".to_owned());
        let journal_value = serde_json::to_value(&journal).map_err(|error| CoreError::Failed(error.to_string()))?;
        return Ok(json!({
            "command": "refresh",
            "mode": "recover",
            "vmid": vmid,
            "name": journal.name,
            "node": node,
            "pool": pool,
            "status": status,
            "home_label": journal.home_label,
            "home_volume": journal.home_volume,
            "recovery_phase": journal.phase.as_str(),
            "rolling_back": journal.rolling_back,
            "mismatches": mismatches,
            "force": force,
            "journal": journal_value,
        }));
    }
    if force {
        return Err(CoreError::Invalid(
            "force applies only to the recovery of an interrupted refresh; VM has no refresh journal".to_owned(),
        ));
    }

    let info = resolve_existing_workspace(pve, cfg, vmid).await?;
    let (ip, cidr, gateway) = parse_ipconfig0(info.vm_config.get("ipconfig0").map(String::as_str).unwrap_or_default())?;
    if info.node != cfg.node.as_str() {
        return Err(CoreError::Refused(format!(
            "Refresh currently requires workspace VM {vmid} on Gold node {}; it is on {}. Migrate it back first.",
            cfg.node, info.node
        )));
    }
    let root_storage = info.root_storage.clone().unwrap_or_default();
    resolve_homestack_storage(cfg, &info.node, Some(&root_storage))?;
    let gold_resource = cluster_vm_resources(pve)
        .await?
        .into_iter()
        .find(|r| r.vmid == gold)
        .ok_or_else(|| CoreError::Failed(format!("Gold VM {gold} does not exist")))?;
    if gold_resource.node != cfg.node.as_str() {
        return Err(CoreError::Failed(format!(
            "Gold VM {gold} is on {}, expected configured node {}",
            py_repr(Some(&gold_resource.node)),
            py_repr(Some(cfg.node.as_str()))
        )));
    }
    let gold_cfg = vm_config(pve, &gold_resource.node, gold).await?;
    require_gold_tag(gold, &gold_cfg)?;
    let disk_cfg = gold_cfg
        .get(&cfg.root_disk)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| CoreError::Failed(format!("Gold VM {gold} has no {} disk", cfg.root_disk)))?;
    Ok(json!({
        "command": "refresh",
        "mode": "replace",
        "vmid": vmid,
        "name": info.name,
        "node": info.node,
        "pool": pool,
        "status": info.status,
        "ip": ip,
        "cidr": cidr,
        "gateway": gateway,
        "gold_vmid": gold,
        "root_storage": root_storage,
        "root_disk": cfg.root_disk,
        "root_disk_gb": parse_disk_size_gb(disk_cfg),
        "gold_root_disk_config": disk_cfg,
        "gold_root_volume": disk_volume(disk_cfg),
        "home_disk": cfg.home_disk,
        "home_storage": info.home_storage,
        "home_label": info.home_label,
        "home_volume": info.home_volume,
        "home_policy": "preserve persistent disk exactly; never format during refresh",
        "power_state_policy": "verify by boot and restore pre-refresh power state",
    }))
}

// --- execution ---------------------------------------------------------------

/// `refresh_workspace` for a plan of [`plan_refresh`].
pub async fn refresh_workspace(
    pve: &impl Pve,
    cfg: &Config,
    pool: &str,
    plan: &Value,
    timing: &LifecycleTiming,
    observer: &impl Observer,
) -> Result<Value, CoreError> {
    crate::gold::configured_gold(cfg)?;
    let vmid = vmid_of(plan)?;
    let node = text(plan, "node")?;
    require_pool(pve, vmid, pool).await?;
    let vm = Vm {
        pve,
        node,
        vmid,
        timing,
        observer,
    };
    if text(plan, "mode")? == "recover" {
        let vm_cfg = vm.config().await?;
        require_refresh_unlocked(vmid, &vm_cfg)?;
        let journal = read_journal(cfg, vmid, &vm_cfg)?
            .ok_or_else(|| CoreError::Failed(format!("VM {vmid} has no refresh journal any more")))?;
        let planned = plan
            .get("journal")
            .and_then(|j| j.get("transaction_id"))
            .and_then(Value::as_str);
        if planned != Some(journal.transaction_id.as_str()) {
            return Err(CoreError::Failed(format!(
                "The refresh journal of VM {vmid} changed after the plan; plan again"
            )));
        }
        let force = plan.get("force").and_then(Value::as_bool).unwrap_or(false);
        let step = "Recover refresh";
        step_started(observer, step);
        let recovered = recover(&vm, cfg, journal, force).await;
        step_finished(observer, step, recovered.is_ok());
        let recovery = recovered?;
        let mut result = plan.as_object().cloned().unwrap_or_default();
        result.remove("journal");
        result.extend(recovery);
        result.insert("status".to_owned(), json!(vm.status().await));
        result.insert("event".to_owned(), json!("workspace.refresh_recovered"));
        return Ok(Value::Object(result));
    }

    let mut journal: Option<Journal> = None;
    match replace(&vm, cfg, plan, &mut journal).await {
        Ok(result) => Ok(result),
        Err(error) => {
            if journal.is_none() {
                return Err(error);
            }
            // The journal in the description is the last one written.
            let recovery = async {
                let vm_cfg = vm.config().await?;
                let journal = read_journal(cfg, vmid, &vm_cfg)?
                    .ok_or_else(|| CoreError::Failed("the journal is gone".to_owned()))?;
                recover(&vm, cfg, journal, false).await
            }
            .await;
            Err(match recovery {
                Ok(recovery) => CoreError::Failed(format!(
                    "Refresh failed: {error}. Automatic recovery succeeded: {}.",
                    recovery.get("recovery").and_then(Value::as_str).unwrap_or_default()
                )),
                Err(recovery_error) => CoreError::Failed(format!(
                    "Refresh failed: {error}. Automatic recovery also failed: {recovery_error}. \
                     Transaction journal retained in the description of VM {vmid}."
                )),
            })
        }
    }
}

/// The steps of a refresh (`mode="replace"`). `journal` is set once the
/// first journal is written, so the caller knows to recover.
#[allow(clippy::too_many_lines, reason = "the steps of one Python function, in order")]
async fn replace<P: Pve, O: Observer>(
    vm: &Vm<'_, P, O>,
    cfg: &Config,
    plan: &Value,
    journal_slot: &mut Option<Journal>,
) -> Result<Value, CoreError> {
    let vmid = vm.vmid;
    let node = vm.node;
    let observer = vm.observer;
    let name = text(plan, "name")?;
    let ip = text(plan, "ip")?;
    let cidr = u8::try_from(number(plan, "cidr")?)
        .map_err(|_| CoreError::Invalid("The plan's CIDR is out of range".to_owned()))?;
    let gateway = text(plan, "gateway")?;
    let label = text(plan, "home_label")?;
    let root_storage = text(plan, "root_storage")?;
    let gold_root = text(plan, "gold_root_volume")?;
    let gold_disk_config = text(plan, "gold_root_disk_config")?;
    let initial = PowerState::parse(text(plan, "status")?).ok_or_else(|| {
        CoreError::Failed(format!(
            "Workspace VM {vmid} has unsupported pre-refresh power state {}",
            py_repr(plan.get("status").and_then(Value::as_str))
        ))
    })?;

    let current = resolve_existing_workspace(vm.pve, cfg, vmid).await?;
    require_refresh_unlocked(vmid, &current.vm_config)?;
    if read_journal(cfg, vmid, &current.vm_config)?.is_some() {
        return Err(CoreError::Refused(format!(
            "VM {vmid} has a refresh journal now; plan again to recover it"
        )));
    }
    if current.status != initial.as_str() {
        return Err(CoreError::Failed(
            "Workspace power state changed after refresh confirmation".to_owned(),
        ));
    }
    if Some(current.home_volume.as_str()) != plan.get("home_volume").and_then(Value::as_str) {
        return Err(CoreError::Failed(
            "Persistent home identity changed after refresh confirmation".to_owned(),
        ));
    }
    let vm_cfg = current.vm_config.clone();
    let boot = vm_cfg.get("boot").cloned().unwrap_or_default();
    if !boot_order_contains_disk(&boot, &cfg.root_disk) {
        return Err(CoreError::Failed(format!(
            "Workspace boot order does not include {}",
            cfg.root_disk
        )));
    }
    let net0 = vm_cfg
        .get("net0")
        .filter(|n| !n.is_empty())
        .ok_or_else(|| CoreError::Failed(format!("Workspace VM {vmid} has no net0")))?;
    let mac =
        extract_mac(net0).ok_or_else(|| CoreError::Failed(format!("Workspace VM {vmid} net0 has no MAC address")))?;
    let staging = free_staging_slot(&vm_cfg)?;
    let restore = RESTORED_OPTIONS
        .iter()
        .map(|key| ((*key).to_owned(), vm_cfg.get(*key).cloned().unwrap_or_default()))
        .collect();
    let mut journal = Journal {
        version: JOURNAL_VERSION,
        transaction_id: uuid::Uuid::new_v4().simple().to_string(),
        phase: Phase::Prepared,
        vmid,
        name: name.to_owned(),
        node: node.to_owned(),
        initial_status: initial,
        old_root_spec: vm_cfg.get(&cfg.root_disk).cloned().unwrap_or_default(),
        old_root_volume: current.root_volume.clone(),
        root_disk: cfg.root_disk.clone(),
        staging_slot: staging.clone(),
        gold_root_disk_config: gold_disk_config.to_owned(),
        new_root_volume: String::new(),
        new_unused_key: String::new(),
        old_unused_key: String::new(),
        home_volume: current.home_volume.clone(),
        home_label: label.to_owned(),
        home_disk: cfg.home_disk.clone(),
        restore,
        rolling_back: false,
    };
    validate_journal(cfg, &journal)?;
    let vm_cfg = vm.write(&vm_cfg, Some(&journal)).await?;
    *journal_slot = Some(journal.clone());

    let step = "Import Gold root";
    step_started(observer, step);
    let imported = async {
        let spec = format!("{root_storage}:0,import-from={gold_root}");
        let vm_cfg = vm
            .set(
                &vm_cfg,
                &[(staging.as_str(), spec.as_str())],
                &format!("Copying Gold's root into VM {vmid}"),
            )
            .await?;
        require_home(&journal, &vm_cfg)?;
        require_refresh_unlocked(vmid, &vm_cfg)?;
        let new = vm_cfg
            .get(&staging)
            .map(|value| disk_volume(value).to_owned())
            .unwrap_or_default();
        if new.is_empty() || new == journal.old_root_volume || new == journal.home_volume {
            return Err(CoreError::Failed(
                "Imported root did not produce a distinct volume".to_owned(),
            ));
        }
        journal.new_root_volume = new;
        journal.phase = Phase::Imported;
        vm.write(&vm_cfg, Some(&journal)).await
    }
    .await;
    step_finished(observer, step, imported.is_ok());
    imported?;
    *journal_slot = Some(journal.clone());
    let new_volume = journal.new_root_volume.clone();

    let step = "Stop workspace";
    step_started(observer, step);
    let stopped = if initial == PowerState::Running {
        vm.shutdown().await
    } else {
        Ok(())
    };
    step_finished(observer, step, stopped.is_ok());
    stopped?;

    let step = "Switch root disks";
    step_started(observer, step);
    let switched = async {
        let vm_cfg = vm.config().await?;
        require_refresh_unlocked(vmid, &vm_cfg)?;
        require_home(&journal, &vm_cfg)?;
        if root_of(cfg, &vm_cfg) != journal.old_root_volume {
            return Err(CoreError::Failed(
                "Root identity changed before refresh switch".to_owned(),
            ));
        }
        journal.phase = Phase::Switching;
        vm.write(&vm_cfg, Some(&journal)).await?;
        // The copy leaves its slot only now, with the VM stopped: a running
        // VM would unplug it, which Proxmox records as a pending change
        // first and may leave half done.
        let vm_cfg = vm.unlink(&journal.staging_slot, &new_volume, false).await?;
        journal.new_unused_key = unused_key_of(&vm_cfg, &new_volume)
            .ok_or_else(|| CoreError::Failed("Proxmox did not keep the imported root as an unused disk".to_owned()))?;
        vm.write(&vm_cfg, Some(&journal)).await?;
        let vm_cfg = vm.unlink(&cfg.root_disk, &journal.old_root_volume, false).await?;
        let old_unused = unused_key_of(&vm_cfg, &journal.old_root_volume).ok_or_else(|| {
            CoreError::Failed("Proxmox did not retain the original root as an unused disk".to_owned())
        })?;
        journal.old_unused_key = old_unused;
        let vm_cfg = vm.write(&vm_cfg, Some(&journal)).await?;
        let spec = root_attachment_spec(&new_volume, gold_disk_config);
        let vm_cfg = vm
            .set(
                &vm_cfg,
                &[(cfg.root_disk.as_str(), spec.as_str())],
                &format!("Attaching the new root of VM {vmid}"),
            )
            .await?;
        if root_of(cfg, &vm_cfg) != new_volume {
            return Err(CoreError::Failed(
                "New root was not attached to the configured root slot".to_owned(),
            ));
        }
        if unused_key_of(&vm_cfg, &new_volume).is_some() {
            return Err(CoreError::Failed(
                "New root is both attached and retained as an unused disk".to_owned(),
            ));
        }
        require_home(&journal, &vm_cfg)?;
        journal.phase = Phase::Switched;
        vm.write(&vm_cfg, Some(&journal)).await
    }
    .await;
    step_finished(observer, step, switched.is_ok());
    let vm_cfg = switched?;
    *journal_slot = Some(journal.clone());

    let step = "Cloud-Init";
    step_started(observer, step);
    let configured = async {
        let mut settings = cloud_init_settings(cfg, HomeFormat::Keep, ip, cidr, gateway, None)?;
        settings.insert(0, ("boot", boot.clone()));
        let form: Vec<(&str, &str)> = settings.iter().map(|(key, value)| (*key, value.as_str())).collect();
        let vm_cfg = vm
            .set(&vm_cfg, &form, &format!("Configuring Cloud-Init of VM {vmid}"))
            .await?;
        verify_workspace_role_tags(vmid, &vm_cfg)?;
        Ok::<_, CoreError>(vm_cfg)
    }
    .await;
    step_finished(observer, step, configured.is_ok());
    let vm_cfg = configured?;

    let step = "Boot verification";
    step_started(observer, step);
    let verified = async {
        journal.phase = Phase::Verifying;
        vm.write(&vm_cfg, Some(&journal)).await?;
        vm.start().await?;
        let expectation = GuestExpectation {
            name: name.to_owned(),
            address: format!("{ip}/{cidr}"),
            user: cfg.user.clone(),
            home_label: label.to_owned(),
        };
        verify_refreshed_guest(vm.pve, node, vmid, &expectation, &vm.timing.verify).await
    }
    .await;
    step_finished(observer, step, verified.is_ok());
    let cloud_id = verified?;
    *journal_slot = Some(journal.clone());

    let step = "Restore power state";
    step_started(observer, step);
    let restored = if initial == PowerState::Stopped {
        vm.shutdown().await
    } else {
        Ok(())
    };
    step_finished(observer, step, restored.is_ok());
    restored?;

    let step = "Delete previous root";
    step_started(observer, step);
    let cleaned = async {
        let vm_cfg = vm.config().await?;
        journal.phase = Phase::Verified;
        let vm_cfg = vm.write(&vm_cfg, Some(&journal)).await?;
        require_home(&journal, &vm_cfg)?;
        let key = unused_key_of(&vm_cfg, &journal.old_root_volume).ok_or_else(|| {
            CoreError::Failed(format!(
                "Cannot safely delete volume {}: its unused reference is missing",
                py_repr(Some(&journal.old_root_volume))
            ))
        })?;
        let vm_cfg = vm.unlink(&key, &journal.old_root_volume, true).await?;
        vm.write(&vm_cfg, None).await
    }
    .await;
    step_finished(observer, step, cleaned.is_ok());
    cleaned?;
    *journal_slot = None;

    let final_status = vm.status().await;
    if final_status != initial.as_str() {
        return Err(CoreError::Failed(format!(
            "Refresh completed with power state {}, expected {}",
            py_repr(Some(&final_status)),
            py_repr(Some(initial.as_str()))
        )));
    }
    let mut result: Map<String, Value> = plan.as_object().cloned().unwrap_or_default();
    result.insert("status".to_owned(), json!(final_status));
    result.insert("mac".to_owned(), json!(mac));
    result.insert("new_root_volume".to_owned(), json!(new_volume));
    result.insert("home_preserved".to_owned(), json!(true));
    result.insert("guest_verified".to_owned(), json!(true));
    result.insert("power_state_preserved".to_owned(), json!(true));
    result.insert("cloud_id".to_owned(), json!(cloud_id.filter(|id| !id.is_empty())));
    result.insert("rollback_performed".to_owned(), json!(false));
    result.insert("cleanup_pending".to_owned(), json!(false));
    result.insert("host_key_changed".to_owned(), json!(true));
    result.insert("event".to_owned(), json!("workspace.refreshed"));
    Ok(Value::Object(result))
}

// --- recovery ----------------------------------------------------------------

/// `_recover_refresh` under decision 4: the VM's config is compared with
/// the journal first, and nothing changes while they disagree, unless
/// `force`. A `verified` journal finishes the cleanup; any other is rolled
/// back to the original root, after `rolling_back` is written.
async fn recover<P: Pve, O: Observer>(
    vm: &Vm<'_, P, O>,
    cfg: &Config,
    mut journal: Journal,
    force: bool,
) -> Result<Map<String, Value>, CoreError> {
    let vmid = vm.vmid;
    validate_journal(cfg, &journal)?;
    let vm_cfg = vm.config().await?;
    let mismatches = journal_mismatches(&journal, vm.node, &vm_cfg);
    if !mismatches.is_empty() {
        if force {
            return force_new_root(vm, cfg, &journal, &vm_cfg, &mismatches).await;
        }
        return Err(CoreError::Failed(format!(
            "VM {vmid} does not agree with its refresh journal ({}); nothing was changed",
            mismatches.join("; ")
        )));
    }
    let staged = staged_volume(&journal, &vm_cfg);
    if journal.new_root_volume.is_empty()
        && let Some(staged) = &staged
    {
        journal.new_root_volume.clone_from(staged);
    }
    if vm.status().await == "running" {
        vm.shutdown().await?;
    }
    // Changes Proxmox left pending: rewriting the journal applies them now
    // that the VM is stopped (they agreed with the journal, as checked).
    let vm_cfg = vm.config().await?;
    if has_pending(&vm_cfg) {
        let vm_cfg = vm.write(&vm_cfg, Some(&journal)).await?;
        if has_pending(&vm_cfg) {
            return Err(CoreError::Failed(format!(
                "Proxmox did not apply the pending changes of VM {vmid}"
            )));
        }
    }

    if journal.phase == Phase::Verified && !journal.rolling_back {
        let vm_cfg = vm.config().await?;
        if root_of(cfg, &vm_cfg) != journal.new_root_volume {
            return Err(CoreError::Failed(
                "Verified refresh journal does not match the attached root".to_owned(),
            ));
        }
        if let Some(key) = unused_key_of(&vm_cfg, &journal.old_root_volume) {
            vm.unlink(&key, &journal.old_root_volume, true).await?;
        }
        if journal.initial_status == PowerState::Running {
            vm.start().await?;
        }
        vm.write(&vm.config().await?, None).await?;
        let mut done = Map::new();
        done.insert("recovery".to_owned(), json!("completed verified cleanup"));
        done.insert("host_key_changed".to_owned(), json!(true));
        return Ok(done);
    }

    // Rollback to the original root.
    let vm_cfg = vm.config().await?;
    if !journal.rolling_back {
        journal.rolling_back = true;
        vm.write(&vm_cfg, Some(&journal)).await?;
    }
    let new = journal.new_root_volume.clone();
    let mut vm_cfg = vm.config().await?;
    if !new.is_empty() && root_of(cfg, &vm_cfg) == new {
        vm_cfg = vm.unlink(&cfg.root_disk, &new, false).await?;
        require_home(&journal, &vm_cfg)?;
    }
    let attached = root_of(cfg, &vm_cfg);
    if !attached.is_empty() && attached != journal.old_root_volume {
        return Err(CoreError::Failed(format!(
            "Refresh recovery found unexpected root volume {}",
            py_repr(Some(&attached))
        )));
    }
    if attached.is_empty() {
        if unused_key_of(&vm_cfg, &journal.old_root_volume).is_none() {
            return Err(CoreError::Failed(
                "Refresh recovery cannot find the original root volume".to_owned(),
            ));
        }
        vm_cfg = vm
            .set(
                &vm_cfg,
                &[(cfg.root_disk.as_str(), journal.old_root_spec.as_str())],
                &format!("Re-attaching the original root of VM {vmid}"),
            )
            .await?;
    }
    if root_of(cfg, &vm_cfg) != journal.old_root_volume {
        return Err(CoreError::Failed(
            "Refresh recovery did not restore the original root".to_owned(),
        ));
    }
    if unused_key_of(&vm_cfg, &journal.old_root_volume).is_some() {
        return Err(CoreError::Failed(
            "Original root is both attached and retained as an unused disk".to_owned(),
        ));
    }
    require_home(&journal, &vm_cfg)?;
    if !new.is_empty() {
        let slot = if vm_cfg.get(&journal.staging_slot).map(|v| disk_volume(v)) == Some(new.as_str()) {
            Some(journal.staging_slot.clone())
        } else {
            unused_key_of(&vm_cfg, &new)
        };
        if let Some(slot) = slot {
            vm_cfg = vm.unlink(&slot, &new, true).await?;
        }
    }
    let set: Vec<(&str, &str)> = journal
        .restore
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let unset: Vec<&str> = journal
        .restore
        .iter()
        .filter(|(key, value)| value.is_empty() && vm_cfg.contains_key(key.as_str()))
        .map(|(key, _)| key.as_str())
        .collect();
    let deleted = unset.join(",");
    let mut form = set;
    if !deleted.is_empty() {
        form.push(("delete", deleted.as_str()));
    }
    if !form.is_empty() {
        vm.set(&vm_cfg, &form, &format!("Restoring the options of VM {vmid}"))
            .await?;
    }
    if journal.initial_status == PowerState::Running {
        vm.start().await?;
    }
    vm.write(&vm.config().await?, None).await?;
    let mut done = Map::new();
    done.insert("recovery".to_owned(), json!("rolled back to original root"));
    done.insert("host_key_changed".to_owned(), json!(false));
    Ok(done)
}

/// Rule 5 of decision 4: with the state not agreeing with the journal,
/// attach the staged new root anyway and keep every other volume as an
/// unused disk; never delete one. The home must still be the journal's.
async fn force_new_root<P: Pve, O: Observer>(
    vm: &Vm<'_, P, O>,
    cfg: &Config,
    journal: &Journal,
    vm_cfg: &VmConfig,
    mismatches: &[String],
) -> Result<Map<String, Value>, CoreError> {
    let vmid = vm.vmid;
    if let Some(home) = home_mismatch(journal, vm_cfg) {
        return Err(CoreError::Failed(format!(
            "force does not apply: the persistent home is not the journal's ({home}); nothing was changed"
        )));
    }
    if vm.node != journal.node {
        return Err(CoreError::Failed(format!(
            "force does not apply: VM {vmid} is not on the journal's node; nothing was changed"
        )));
    }
    let Some(new) = staged_volume(journal, vm_cfg) else {
        return Err(CoreError::Failed(
            "force does not apply: the journal names no new root; nothing was changed".to_owned(),
        ));
    };
    if place_of(journal, vm_cfg, &new) == Place::Absent {
        return Err(CoreError::Failed(format!(
            "force does not apply: the new root {} is not in the VM's config; nothing was changed",
            py_repr(Some(&new))
        )));
    }
    if vm.status().await == "running" {
        vm.shutdown().await?;
    }
    let mut vm_cfg = vm.config().await?;
    if has_pending(&vm_cfg) {
        // Applies what Proxmox left pending; a pending unlink keeps the volume.
        vm_cfg = vm.write(&vm_cfg, Some(journal)).await?;
    }
    let root = root_of(cfg, &vm_cfg);
    if root != new {
        if !root.is_empty() {
            vm_cfg = vm.unlink(&cfg.root_disk, &root, false).await?;
        }
        let slots: Vec<String> = vm_cfg
            .iter()
            .filter(|(key, value)| key.as_str() != "description" && !is_unused_key(key) && disk_volume(value) == new)
            .map(|(key, _)| key.clone())
            .collect();
        for slot in slots {
            vm_cfg = vm.unlink(&slot, &new, false).await?;
        }
        let spec = root_attachment_spec(&new, &journal.gold_root_disk_config);
        vm_cfg = vm
            .set(
                &vm_cfg,
                &[(cfg.root_disk.as_str(), spec.as_str())],
                &format!("Attaching the new root of VM {vmid}"),
            )
            .await?;
    }
    if root_of(cfg, &vm_cfg) != new {
        return Err(CoreError::Failed(
            "New root was not attached to the configured root slot".to_owned(),
        ));
    }
    let (ip, cidr, gateway) = parse_ipconfig0(
        journal
            .restore
            .get("ipconfig0")
            .map(String::as_str)
            .filter(|value| !value.is_empty())
            .or_else(|| vm_cfg.get("ipconfig0").map(String::as_str))
            .unwrap_or_default(),
    )?;
    let mut settings = cloud_init_settings(cfg, HomeFormat::Keep, &ip, cidr, &gateway, None)?;
    if let Some(boot) = journal.restore.get("boot").filter(|boot| !boot.is_empty()) {
        settings.insert(0, ("boot", boot.clone()));
    }
    let form: Vec<(&str, &str)> = settings.iter().map(|(key, value)| (*key, value.as_str())).collect();
    vm.set(&vm_cfg, &form, &format!("Configuring Cloud-Init of VM {vmid}"))
        .await?;
    vm.start().await?;
    let expectation = GuestExpectation {
        name: journal.name.clone(),
        address: format!("{ip}/{cidr}"),
        user: cfg.user.clone(),
        home_label: journal.home_label.clone(),
    };
    let cloud_id = verify_refreshed_guest(vm.pve, vm.node, vmid, &expectation, &vm.timing.verify).await?;
    if journal.initial_status == PowerState::Stopped {
        vm.shutdown().await?;
    }
    let vm_cfg_now = vm.config().await?;
    vm.write(&vm_cfg_now, None).await?;
    let kept: Vec<String> = vm_cfg_now
        .iter()
        .filter(|(key, _)| is_unused_key(key))
        .map(|(key, value)| format!("{key}={}", disk_volume(value)))
        .collect();
    let mut done = Map::new();
    done.insert("recovery".to_owned(), json!("forced the new root"));
    done.insert("mismatches".to_owned(), json!(mismatches));
    done.insert("unused_volumes_kept".to_owned(), json!(kept));
    done.insert("cloud_id".to_owned(), json!(cloud_id));
    done.insert("host_key_changed".to_owned(), json!(true));
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipconfig0_needs_a_static_address_and_a_gateway() -> Result<(), CoreError> {
        assert_eq!(
            parse_ipconfig0("ip=192.0.2.200/24,gw=192.0.2.1")?,
            ("192.0.2.200".to_owned(), 24, "192.0.2.1".to_owned())
        );
        assert!(parse_ipconfig0("ip=dhcp").is_err());
        assert!(parse_ipconfig0("ip=192.0.2.200/24").is_err());
        assert!(parse_ipconfig0("").is_err());
        Ok(())
    }

    #[test]
    fn the_journal_block_keeps_the_rest_of_the_description() -> Result<(), CoreError> {
        let (rest, payload) = split_description("notes\n<!-- homestack-journal: abc -->")?;
        assert_eq!(rest, "notes");
        assert_eq!(payload, Some("abc"));
        assert_eq!(split_description("plain")?, ("plain".to_owned(), None));
        assert!(split_description("x <!-- homestack-journal: abc").is_err());
        Ok(())
    }

    #[test]
    fn root_attachment_takes_golds_options() {
        assert_eq!(
            root_attachment_spec("s:vm-200-disk-1", "s:vm-101-disk-0,iothread=1,size=16G,discard=on"),
            "s:vm-200-disk-1,iothread=1,discard=on"
        );
    }
}
