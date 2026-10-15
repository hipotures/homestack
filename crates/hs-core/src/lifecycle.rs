//! `create` on Gold's node and `destroy` (mapping B1, B7; phase 5).
//!
//! Ports `lifecycle.build_create_plan`, `create_workspace`,
//! `build_destroy_plan` and `destroy_workspace` onto the API, with
//! decisions 1 = A (Proxmox's Cloud-Init and a static vendor snippet, so no
//! per-VM files), 2 = A (the root keeps Proxmox's name) and the instance's
//! pool: the clone goes into it and `destroy` refuses anything outside it.
//! Plans and results are JSON objects, as Python's dicts are;
//! `docs/migration/phase-5.md` lists what differs.

use std::collections::BTreeSet;
use std::time::Duration;

use hs_model::NodeName;
use hs_model::config::Config;
use hs_model::jobs::JobEvent;
use serde_json::{Map, Value, json};

use crate::cloudinit::{HomeFormat, cloud_init_settings};
use crate::create_transfer::{migrate_clone, revalidate_source, validate_cross_node_create, verify_migrated_clone};
use crate::error::CoreError;
use crate::gold::extract_mac;
use crate::inventory::{
    ALIEN_WORKSPACE_TAG, GOLD_TAG, REFRESH_LOCK_TAG, TEST_TAG, VmConfig, WORKSPACE_TAG, cluster_vm_maps,
    cluster_vm_resources, disk_option, has_tag, home_label, parse_disk_size_gb, parse_tags, py_repr,
    resolve_homestack_storage, text_or_none, vm_config, vm_status, workspace_tags,
};
use crate::keys::{parse_authorized_key_records, workspace_authorized_keys};
use crate::power::{SHUTDOWN_TIMEOUT_SECS, START_TIMEOUT_SECS, shutdown_vm, start_vm};
use crate::pve::Pve;
use crate::status::resolve_existing_workspace;
use crate::tasks::{Observer, TaskWait, retry_while_locked, wait_task};
use crate::verify::{GuestExpectation, VerifyTimeouts, verify_guest};

/// Pauses and limits of the operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleTiming {
    /// Between two reads of a task's status.
    pub poll: Duration,
    pub verify: VerifyTimeouts,
}

impl Default for LifecycleTiming {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(1),
            verify: VerifyTimeouts::default(),
        }
    }
}

impl LifecycleTiming {
    pub(crate) const fn wait(&self, seconds: u64) -> TaskWait {
        TaskWait {
            poll: self.poll,
            timeout: Duration::from_secs(seconds),
        }
    }
}

/// `models.validate_name`
pub fn validate_name(name: &str) -> Result<(), CoreError> {
    let valid = (1..=63).contains(&name.len())
        && name.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(CoreError::Invalid(
            "Invalid VM name. Use 1-63 characters: letters, digits, dot and hyphen; the first character must be a letter."
                .to_owned(),
        ))
    }
}

/// `guest.derive_ip`: VMIDs 2–254 are `<prefix>.<vmid>` as in Python; from
/// 1000 the address is the VMID's last two octets in the gateway's network
/// (owner's decision of 2026-10-14; `hs_model::address::address_of`).
pub fn derive_ip(cfg: &Config, vmid: u32) -> Result<String, CoreError> {
    if let Some(address) = hs_model::address::address_of(vmid, &cfg.network) {
        return Ok(address.to_string());
    }
    let network = cfg
        .network
        .gateway
        .parse::<std::net::Ipv4Addr>()
        .map(|gateway| {
            let mask = u32::MAX.checked_shl(32 - u32::from(cfg.network.cidr)).unwrap_or(0);
            format!(
                "{}/{}",
                std::net::Ipv4Addr::from(u32::from(gateway) & mask),
                cfg.network.cidr
            )
        })
        .unwrap_or_else(|_| format!("{}.0/{}", cfg.network.prefix, cfg.network.cidr));
    Err(CoreError::Invalid(format!(
        "VMID {vmid} has no address in the network {network}: VMIDs 2..254 are {}.<vmid>, and from 1000 the VMID is \
         the last two octets of the address (101010 is .101.10).",
        cfg.network.prefix
    )))
}

/// `proxmox.parse_home_size`: `<n>M`, `<n>G` or `<n>T`, as text and in MiB.
pub fn parse_home_size(size: &str) -> Result<(String, u64), CoreError> {
    let text = size.trim().to_ascii_uppercase();
    let invalid = || {
        CoreError::Invalid(
            "Home size must be an integer number of MiB, GiB or TiB, for example 500M, 20G or 1T".to_owned(),
        )
    };
    let (number, unit) = text.split_at(text.len().saturating_sub(1));
    let valid_number = !number.is_empty() && !number.starts_with('0') && number.bytes().all(|b| b.is_ascii_digit());
    if !valid_number {
        return Err(invalid());
    }
    let value: u64 = number.parse().map_err(|_| invalid())?;
    let mib = match unit {
        "M" => value,
        "G" => value.checked_mul(1024).ok_or_else(invalid)?,
        "T" => value.checked_mul(1024 * 1024).ok_or_else(invalid)?,
        _ => return Err(invalid()),
    };
    Ok((text, mib))
}

/// `proxmox.home_volume_name`
pub fn home_volume_name(vmid: u32, user: &str) -> Result<String, CoreError> {
    let valid = (1..=32).contains(&user.len())
        && user.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid {
        Ok(format!("vm-{vmid}-hs-home-{user}"))
    } else {
        Err(CoreError::Invalid(format!(
            "Cannot derive persistent home volume name from user {}",
            py_repr(Some(user))
        )))
    }
}

/// `status.occupancy_risk` with `DESTROY_WARN_PERCENT` 10 and
/// `DESTROY_DANGER_PERCENT` 50.
fn occupancy_risk(percent: Option<f64>) -> &'static str {
    match percent {
        Some(p) if p <= 10.0 => "low",
        Some(p) if p <= 50.0 => "warning",
        _ => "danger",
    }
}

/// `proxmox.verify_workspace_role_tags`
pub fn verify_workspace_role_tags(vmid: u32, vm_cfg: &VmConfig) -> Result<(), CoreError> {
    let tags = parse_tags(vm_cfg.get("tags").map(String::as_str));
    if !tags.contains(WORKSPACE_TAG) {
        return Err(CoreError::Failed(format!(
            "VM {vmid} tag verification failed: '{WORKSPACE_TAG}' is missing"
        )));
    }
    if tags.contains(GOLD_TAG) {
        return Err(CoreError::Failed(format!(
            "VM {vmid} tag verification failed: inherited '{GOLD_TAG}'"
        )));
    }
    let allowed: BTreeSet<String> = [WORKSPACE_TAG, REFRESH_LOCK_TAG, TEST_TAG]
        .iter()
        .map(|t| (*t).to_owned())
        .collect();
    if !tags.is_subset(&allowed) {
        let got: Vec<String> = tags.iter().map(|t| format!("'{t}'")).collect();
        return Err(CoreError::Failed(format!(
            "VM {vmid} tag verification failed: expected '{WORKSPACE_TAG}' with optional '{REFRESH_LOCK_TAG}' and '{TEST_TAG}', got [{}]",
            got.join(", ")
        )));
    }
    Ok(())
}

/// `proxmox.require_gold_tag`
pub(crate) fn require_gold_tag(vmid: u32, vm_cfg: &VmConfig) -> Result<(), CoreError> {
    let tags = vm_cfg.get("tags").map(String::as_str);
    if has_tag(tags, ALIEN_WORKSPACE_TAG) {
        return Err(CoreError::Refused(format!(
            "Configured Gold VM {vmid} has forbidden workspace role tag '{ALIEN_WORKSPACE_TAG}'."
        )));
    }
    if !has_tag(tags, GOLD_TAG) {
        return Err(CoreError::Refused(format!(
            "Configured Gold VM {vmid} is missing required tag '{GOLD_TAG}'."
        )));
    }
    Ok(())
}

/// The pool `cluster/resources` names for a VM (needs `Pool.Audit`).
async fn vm_pool(pve: &impl Pve, vmid: u32) -> Result<Option<String>, CoreError> {
    let (_, map) = cluster_vm_maps(pve)
        .await?
        .into_iter()
        .find(|(id, _)| *id == vmid)
        .ok_or_else(|| CoreError::NotFound(format!("VMID {vmid} does not exist")))?;
    Ok(text_or_none(map.get("pool")))
}

/// The gate before phase 5: changes only to VMs in the instance's pool.
pub(crate) async fn require_pool(pve: &impl Pve, vmid: u32, pool: &str) -> Result<(), CoreError> {
    match vm_pool(pve, vmid).await? {
        Some(found) if found == pool => Ok(()),
        found => Err(CoreError::OutsidePool(format!(
            "VM {vmid} is not in pool {pool}{}; HomeStack changes only the VMs of its pool",
            found.map(|p| format!(" (it is in {p})")).unwrap_or_default()
        ))),
    }
}

/// The clone keeps Gold's `net0`: the token needs `SDN.Use` on its bridge
/// (`/sdn/zones/<zone>/<bridge>`, zone `localnetwork` for a plain Linux
/// bridge, else the zone of the SDN VNet of that name).
async fn require_bridge_use(pve: &impl Pve, gold: u32, gold_cfg: &VmConfig) -> Result<(), CoreError> {
    let net0 = gold_cfg.get("net0").map(String::as_str).unwrap_or_default();
    let bridge =
        disk_option(net0, "bridge").ok_or_else(|| CoreError::Failed(format!("Gold VM {gold} net0 has no bridge")))?;
    let zone = match pve.get("/cluster/sdn/vnets", &[]).await {
        Ok(Value::Array(vnets)) => vnets
            .iter()
            .find(|v| v.get("vnet").and_then(Value::as_str) == Some(bridge.as_str()))
            .and_then(|v| v.get("zone").and_then(Value::as_str))
            .map(str::to_owned),
        _ => None,
    };
    let path = format!("/sdn/zones/{}/{bridge}", zone.as_deref().unwrap_or("localnetwork"));
    let granted = pve.get("/access/permissions", &[("path", &path)]).await?;
    let allowed = granted
        .get(&path)
        .and_then(Value::as_object)
        .is_some_and(|privileges| privileges.contains_key("SDN.Use"));
    if allowed {
        Ok(())
    } else {
        Err(CoreError::Refused(format!(
            "The server's Proxmox token lacks SDN.Use on {path}, the bridge of Gold's net0; run deploy/pve/homestack-pve.sh install"
        )))
    }
}

fn upid(answer: &Value) -> Result<String, CoreError> {
    answer
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| CoreError::Unusable(format!("Proxmox did not return a task id: {answer}")))
}

/// Start a Proxmox task with `POST` and wait for it to succeed; a call
/// refused for a taken lock is tried again ([`retry_while_locked`]).
pub(crate) async fn post_task(
    pve: &impl Pve,
    path: &str,
    form: &[(&str, &str)],
    what: &str,
    wait: &TaskWait,
    observer: &impl Observer,
) -> Result<(), CoreError> {
    retry_while_locked(wait.poll, || async {
        let answer = pve.post(path, form).await?;
        let result = wait_task(pve, &upid(&answer)?, wait, observer).await?;
        if result.ok() {
            Ok(())
        } else {
            Err(CoreError::Failed(format!("{what} failed: {}", result.exitstatus)))
        }
    })
    .await
}

/// `DELETE` that starts a task, then wait for it; tried again like
/// [`post_task`].
async fn delete_task(
    pve: &impl Pve,
    path: &str,
    query: &[(&str, &str)],
    what: &str,
    wait: &TaskWait,
    observer: &impl Observer,
) -> Result<(), CoreError> {
    retry_while_locked(wait.poll, || async {
        let answer = pve.delete(path, query).await?;
        let result = wait_task(pve, &upid(&answer)?, wait, observer).await?;
        if result.ok() {
            Ok(())
        } else {
            Err(CoreError::Failed(format!("{what} failed: {}", result.exitstatus)))
        }
    })
    .await
}

pub(crate) fn step_started(observer: &impl Observer, name: &str) {
    observer.event(JobEvent::StepStarted { name: name.to_owned() });
}

pub(crate) fn step_finished(observer: &impl Observer, name: &str, ok: bool) {
    observer.event(JobEvent::StepFinished {
        name: name.to_owned(),
        ok,
    });
}

/// What `create` is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRequest {
    pub vmid: u32,
    pub name: String,
    /// `[home] default_size` when absent.
    pub home_size: Option<String>,
    pub storage: Option<String>,
    /// Gold's node; another node is B2 (phase 10).
    pub node: Option<String>,
}

/// `build_create_plan` on Gold's node, cloning into `pool`.
pub async fn plan_create(
    pve: &impl Pve,
    cfg: &Config,
    pool: &str,
    request: &CreateRequest,
) -> Result<Value, CoreError> {
    let vmid = request.vmid;
    let name = request.name.as_str();
    validate_name(name)?;
    let ip = derive_ip(cfg, vmid)?;
    let node = cfg.node.as_str();
    let target = request.node.as_deref().map_or(node, str::trim);
    if target.is_empty() {
        return Err(CoreError::Invalid("Create target node cannot be empty".to_owned()));
    }
    let target = NodeName::parse(target)
        .map_err(|_| CoreError::Invalid(format!("Invalid node name {}", py_repr(Some(target)))))?
        .to_string();
    let elsewhere = target != node;
    let home_size = request.home_size.as_deref().unwrap_or(&cfg.default_home_size);
    let (home_size, home_size_mib) = parse_home_size(home_size)?;
    let home_volume = home_volume_name(vmid, &cfg.user.name)?;
    let label = home_label(vmid)?;

    let resources = cluster_vm_resources(pve).await?;
    let gold = cfg.gold_vmid;
    let gold_resource = resources
        .iter()
        .find(|r| r.vmid == gold)
        .ok_or_else(|| CoreError::Failed(format!("Gold VM {gold} does not exist")))?;
    if gold_resource.node != node {
        return Err(CoreError::Failed(format!(
            "Gold VM {gold} is on {}, expected configured node {}",
            py_repr(Some(&gold_resource.node)),
            py_repr(Some(node))
        )));
    }
    if resources.iter().any(|r| r.vmid == vmid) {
        return Err(CoreError::Refused(format!("VMID {vmid} already exists")));
    }
    let pools = pve.get("/pools", &[]).await?;
    let pool_exists = pools.as_array().is_some_and(|list| {
        list.iter()
            .any(|p| p.get("poolid").and_then(Value::as_str) == Some(pool))
    });
    if !pool_exists {
        return Err(CoreError::Refused(format!(
            "pool {pool} does not exist, or the token lacks Pool.Audit on it; run deploy/pve/homestack-pve.sh install"
        )));
    }

    let gold_cfg = vm_config(pve, node, gold).await?;
    require_gold_tag(gold, &gold_cfg)?;
    let root_cfg = gold_cfg
        .get(&cfg.root_disk)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| CoreError::Failed(format!("Gold VM {gold} has no {} disk", cfg.root_disk)))?;
    let root_gb = parse_disk_size_gb(root_cfg);
    let (storage, transfer) = if elsewhere {
        let transfer = validate_cross_node_create(pve, cfg, &target, &gold_cfg, request.storage.as_deref()).await?;
        (transfer.target_storage.clone(), Some(transfer))
    } else {
        (resolve_homestack_storage(cfg, node, request.storage.as_deref())?, None)
    };
    require_bridge_use(pve, gold, &gold_cfg).await?;

    let keys = workspace_authorized_keys(pve, cfg).await?;
    let labels: Vec<String> = parse_authorized_key_records(&keys.keys)?
        .into_iter()
        .map(|record| record.label)
        .collect();

    let mut plan = json!({
        "command": "create",
        "vmid": vmid,
        "name": name,
        "node": target,
        "source_node": node,
        "transfer_method": if elsewhere { "clone-migrate" } else { "clone" },
        "pool": pool,
        "ip": ip,
        "cidr": cfg.network.cidr,
        "gateway": cfg.network.gateway,
        "gold_vmid": gold,
        "clone_type": "full",
        "root_storage": storage,
        "root_disk": cfg.root_disk,
        "root_disk_gb": root_gb,
        "root_disk_policy": "inherit Gold unchanged",
        "home_storage": storage,
        "home_disk": cfg.home_disk,
        "home_size": home_size,
        "home_size_mib": home_size_mib,
        "home_label": label,
        "home_policy": "persistent Proxmox disk; ext4 mounted by filesystem label",
        "home_volume_name": home_volume,
        "user": cfg.user.name,
        "uid": cfg.user.uid,
        "gid": cfg.user.gid,
        "ssh_key_source": keys.source,
        "ssh_public_keys": labels,
    });
    if let Some(transfer) = transfer {
        // The clone needs room on Gold's node until it is migrated.
        plan["clone_storage"] = json!(resolve_homestack_storage(cfg, node, None)?);
        plan["source_metadata"] = transfer.source_metadata;
    }
    Ok(plan)
}

pub(crate) fn text<'a>(plan: &'a Value, key: &str) -> Result<&'a str, CoreError> {
    plan.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Invalid(format!("The plan has no {key}")))
}

pub(crate) fn number(plan: &Value, key: &str) -> Result<u64, CoreError> {
    plan.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| CoreError::Invalid(format!("The plan has no {key}")))
}

pub(crate) fn vmid_of(plan: &Value) -> Result<u32, CoreError> {
    u32::try_from(number(plan, "vmid")?).map_err(|_| CoreError::Invalid("The plan's VMID is out of range".to_owned()))
}

/// How far a create got, for the cleanup.
#[derive(Default)]
struct Progress {
    cloned: bool,
    started: bool,
}

/// `create_workspace` for a plan of [`plan_create`].
pub async fn create_workspace(
    pve: &impl Pve,
    cfg: &Config,
    plan: &Value,
    timing: &LifecycleTiming,
    observer: &impl Observer,
) -> Result<Value, CoreError> {
    let vmid = vmid_of(plan)?;
    let name = text(plan, "name")?;
    if plan.get("source_metadata").is_some() {
        revalidate_source(pve, cfg, plan).await?;
    }
    let mut progress = Progress::default();
    match create_steps(pve, cfg, plan, timing, observer, &mut progress).await {
        Ok(result) => Ok(result),
        Err(error) => {
            if progress.started {
                return Err(error);
            }
            let mut cleanup = Vec::new();
            if progress.cloned
                && let Err(failure) = remove_failed_clone(pve, vmid, name, timing, observer).await
            {
                cleanup.push(format!("VM cleanup failed: {failure}"));
            }
            if cleanup.is_empty() {
                Err(error)
            } else {
                Err(CoreError::Failed(format!(
                    "{error}. Cleanup report: {}",
                    cleanup.join("; ")
                )))
            }
        }
    }
}

/// Destroy a clone that was never started, after checking it is the one
/// this create made, on whichever node it is (a failed migration leaves it
/// on Gold's).
async fn remove_failed_clone(
    pve: &impl Pve,
    vmid: u32,
    name: &str,
    timing: &LifecycleTiming,
    observer: &impl Observer,
) -> Result<(), CoreError> {
    let Some(resource) = cluster_vm_resources(pve).await?.into_iter().find(|r| r.vmid == vmid) else {
        return Ok(());
    };
    let node = resource.node.as_str();
    let vm_cfg = vm_config(pve, node, vmid).await?;
    if vm_cfg.get("name").map(String::as_str) != Some(name) {
        return Err(CoreError::Failed(format!(
            "VM {vmid} is not the clone named {name}; left alone"
        )));
    }
    let status = vm_status(pve, node, vmid).await;
    if status != "stopped" {
        return Err(CoreError::Failed(format!("VM {vmid} is {status}; left alone")));
    }
    delete_task(
        pve,
        &format!("/nodes/{node}/qemu/{vmid}"),
        &[("purge", "1"), ("destroy-unreferenced-disks", "1")],
        &format!("Destroying VM {vmid}"),
        &timing.wait(600),
        observer,
    )
    .await
}

async fn create_steps(
    pve: &impl Pve,
    cfg: &Config,
    plan: &Value,
    timing: &LifecycleTiming,
    observer: &impl Observer,
    progress: &mut Progress,
) -> Result<Value, CoreError> {
    let vmid = vmid_of(plan)?;
    let id = vmid.to_string();
    let node = text(plan, "node")?;
    let name = text(plan, "name")?;
    let ip = text(plan, "ip")?;
    let pool = text(plan, "pool")?;
    let storage = text(plan, "root_storage")?;
    let home_storage = text(plan, "home_storage")?;
    let label = text(plan, "home_label")?;
    let home_size_mib = number(plan, "home_size_mib")?;
    let config_path = format!("/nodes/{node}/qemu/{id}/config");

    let step = "SSH keys";
    step_started(observer, step);
    let keys = workspace_authorized_keys(pve, cfg).await;
    step_finished(observer, step, keys.is_ok());
    let keys = keys?;
    let expected = parse_authorized_key_records(&keys.keys)?;

    // On another node the clone is made on Gold's and migrated (decision 3 = A).
    let migrated = plan.get("source_metadata").is_some();
    let source = text(plan, "source_node")?;
    let clone_storage = if migrated {
        text(plan, "clone_storage")?
    } else {
        storage
    };

    let step = "Full clone";
    step_started(observer, step);
    let gold = cfg.gold_vmid.to_string();
    let cloned = post_task(
        pve,
        &format!("/nodes/{source}/qemu/{gold}/clone"),
        &[
            ("newid", &id),
            ("name", name),
            ("full", "1"),
            ("storage", clone_storage),
            ("pool", pool),
        ],
        &format!("Cloning Gold VM {gold} to {vmid}"),
        &timing.wait(3600),
        observer,
    )
    .await;
    // A failed clone task may still have left the VM; the cleanup checks.
    progress.cloned = true;
    step_finished(observer, step, cloned.is_ok());
    cloned?;

    if migrated {
        let step = "Migrate to target node";
        step_started(observer, step);
        let moved = migrate_clone(pve, cfg, plan, timing, observer).await;
        step_finished(observer, step, moved.is_ok());
        moved?;

        let step = "Verify migrated clone";
        step_started(observer, step);
        let checked = verify_migrated_clone(pve, cfg, plan).await;
        step_finished(observer, step, checked.is_ok());
        checked?;
    }

    let step = "Persistent home disk";
    step_started(observer, step);
    let home = attach_home(
        pve,
        cfg,
        node,
        vmid,
        home_storage,
        home_size_mib,
        label,
        timing,
        observer,
    )
    .await;
    step_finished(observer, step, home.is_ok());
    home?;

    let step = "Clone identity";
    step_started(observer, step);
    let identity = async {
        post_task(
            pve,
            &config_path,
            &[("tags", &workspace_tags(pool))],
            &format!("Tagging VM {vmid}"),
            &timing.wait(600),
            observer,
        )
        .await?;
        let vm_cfg = vm_config(pve, node, vmid).await?;
        verify_workspace_role_tags(vmid, &vm_cfg)?;
        let net0 = vm_cfg
            .get("net0")
            .filter(|n| !n.is_empty())
            .ok_or_else(|| CoreError::Failed(format!("Cloned VM {vmid} has no net0")))?;
        let mac =
            extract_mac(net0).ok_or_else(|| CoreError::Failed(format!("Cloned VM {vmid} net0 has no MAC address")))?;
        let home = vm_cfg.get(&cfg.home_disk).map(String::as_str).unwrap_or_default();
        if disk_option(home, "serial").as_deref() != Some(label) {
            return Err(CoreError::Failed(format!(
                "VM {vmid} {} does not have expected serial {label}",
                cfg.home_disk
            )));
        }
        Ok(mac)
    }
    .await;
    step_finished(observer, step, identity.is_ok());
    let mac = identity?;

    let step = "Cloud-Init";
    step_started(observer, step);
    let cloud_init = async {
        let settings = cloud_init_settings(
            cfg,
            HomeFormat::Create,
            ip,
            cfg.network.cidr,
            &cfg.network.gateway,
            Some(&keys.keys),
        )?;
        let form: Vec<(&str, &str)> = settings.iter().map(|(key, value)| (*key, value.as_str())).collect();
        post_task(
            pve,
            &config_path,
            &form,
            &format!("Configuring Cloud-Init of VM {vmid}"),
            &timing.wait(600),
            observer,
        )
        .await?;
        verify_workspace_role_tags(vmid, &vm_config(pve, node, vmid).await?)
    }
    .await;
    step_finished(observer, step, cloud_init.is_ok());
    cloud_init?;

    let step = "Boot";
    step_started(observer, step);
    let started = start_vm(pve, node, vmid, &timing.wait(START_TIMEOUT_SECS), observer).await;
    // After a start request nothing is removed, as in Python.
    progress.started = started.is_ok();
    step_finished(observer, step, started.is_ok());
    started?;

    let step = "Verify workspace";
    step_started(observer, step);
    let verified = async {
        let expectation = GuestExpectation {
            name: name.to_owned(),
            address: format!("{ip}/{}", cfg.network.cidr),
            user: cfg.user.clone(),
            home_label: label.to_owned(),
        };
        let verified = verify_guest(pve, node, vmid, &expectation, &timing.verify)
            .await?
            .map_err(|failure| CoreError::Failed(failure.create_message(&expectation)))?;
        let actual = parse_authorized_key_records(&verified.user_authorized_keys)?;
        let expected_ids: BTreeSet<&str> = expected.iter().map(|r| r.identity.as_str()).collect();
        let actual_ids: BTreeSet<&str> = actual.iter().map(|r| r.identity.as_str()).collect();
        let missing: Vec<&str> = expected
            .iter()
            .filter(|r| !actual_ids.contains(r.identity.as_str()))
            .map(|r| r.label.as_str())
            .collect();
        let unexpected: Vec<&str> = actual
            .iter()
            .filter(|r| !expected_ids.contains(r.identity.as_str()))
            .map(|r| r.label.as_str())
            .collect();
        if !missing.is_empty() || !unexpected.is_empty() {
            let mut parts = Vec::new();
            if !missing.is_empty() {
                parts.push(format!("missing: {}", missing.join(", ")));
            }
            if !unexpected.is_empty() {
                parts.push(format!("unexpected: {}", unexpected.join(", ")));
            }
            return Err(CoreError::Failed(format!(
                "Verification failed: SSH public keys mismatch ({})",
                parts.join("; ")
            )));
        }
        Ok(verified.cloud_id)
    }
    .await;
    step_finished(observer, step, verified.is_ok());
    let cloud_id = verified?;

    let mut result = plan.as_object().cloned().unwrap_or_default();
    result.insert("status".to_owned(), json!(vm_status(pve, node, vmid).await));
    result.insert("mac".to_owned(), json!(mac));
    result.insert("ssh_key_source".to_owned(), json!(keys.source));
    result.insert(
        "ssh_public_keys".to_owned(),
        Value::Array(
            expected
                .iter()
                .map(|r| json!({"label": r.label, "verified": true}))
                .collect(),
        ),
    );
    result.insert("cloud_id".to_owned(), json!(cloud_id.filter(|id| !id.is_empty())));
    // A new guest answers at an address another may have had: the client
    // forgets its host key (Python's `forget_local_ssh_host`).
    result.insert("host_key_changed".to_owned(), json!(true));
    result.insert("event".to_owned(), json!("workspace.created"));
    Ok(Value::Object(result))
}

/// `allocate_named_raw_volume` and `qm set --<home>`; the volume is freed
/// when it cannot be attached.
#[allow(clippy::too_many_arguments, reason = "the steps of one Python block")]
async fn attach_home(
    pve: &impl Pve,
    cfg: &Config,
    node: &str,
    vmid: u32,
    storage: &str,
    size_mib: u64,
    label: &str,
    timing: &LifecycleTiming,
    observer: &impl Observer,
) -> Result<(), CoreError> {
    let filename = home_volume_name(vmid, &cfg.user.name)?;
    let id = vmid.to_string();
    let size = format!("{size_mib}M");
    let answer = pve
        .post(
            &format!("/nodes/{node}/storage/{storage}/content"),
            &[
                ("vmid", &id),
                ("filename", &filename),
                ("size", &size),
                ("format", "raw"),
            ],
        )
        .await?;
    let volume = answer
        .as_str()
        .map_or_else(|| format!("{storage}:{filename}"), str::to_owned);
    let spec = format!("{volume},discard=on,iothread=1,ssd=1,serial={label}");
    let attached = post_task(
        pve,
        &format!("/nodes/{node}/qemu/{id}/config"),
        &[(cfg.home_disk.as_str(), spec.as_str())],
        &format!("Attaching the home of VM {vmid}"),
        &timing.wait(600),
        observer,
    )
    .await;
    if let Err(error) = attached {
        // `pvesm free`, whose result Python ignores.
        let _ = delete_task(
            pve,
            &format!("/nodes/{node}/storage/{storage}/content/{volume}"),
            &[],
            &format!("Freeing {volume}"),
            &timing.wait(600),
            observer,
        )
        .await;
        return Err(error);
    }
    Ok(())
}

/// `build_destroy_plan` for a workspace in `pool`.
pub async fn plan_destroy(pve: &impl Pve, cfg: &Config, pool: &str, vmid: u32) -> Result<Value, CoreError> {
    require_pool(pve, vmid, pool).await?;
    let info = resolve_existing_workspace(pve, cfg, vmid).await?;
    let usage = &info.home_usage;
    let percent = match (usage.size_bytes, usage.used_bytes) {
        #[allow(clippy::cast_precision_loss, reason = "a percentage")]
        (Some(size), Some(used)) if size > 0 => Some(100.0 * used as f64 / size as f64),
        _ => None,
    };
    let root_storage = info.root_storage.clone().unwrap_or_else(|| "—".to_owned());
    let home_storage = info.home_storage.clone().unwrap_or_else(|| "—".to_owned());
    let size_gib = |key: &String| info.vm_config.get(key).and_then(|c| parse_disk_size_gb(c));
    Ok(json!({
        "command": "destroy",
        "vmid": vmid,
        "name": info.name,
        "node": info.node,
        "pool": pool,
        "status": info.status,
        "ip": derive_ip(cfg, vmid).ok(),
        "root_disk": cfg.root_disk,
        "root_storage": root_storage,
        "root_size_gib": size_gib(&cfg.root_disk),
        "home_disk": cfg.home_disk,
        "home_storage": home_storage,
        "home_size_gib": size_gib(&cfg.home_disk),
        "home_volume": info.home_volume,
        "home_label": info.home_label,
        "home_capacity_bytes": usage.size_bytes,
        "home_risk": occupancy_risk(percent),
        "root_action": "delete",
        "home_action": "delete permanently with VM",
        "node_files_left": python_node_files(cfg, vmid, &info.name, &info.vm_config),
    }))
}

/// The files Python wrote on the nodes for a workspace it made (its four
/// snippets, named in `cicustom`, and maybe a refresh journal), which the
/// API cannot delete. Empty for a workspace made by the server.
fn python_node_files(cfg: &Config, vmid: u32, name: &str, vm_cfg: &VmConfig) -> Vec<String> {
    let cicustom = vm_cfg.get("cicustom").map(String::as_str).unwrap_or_default();
    if !cicustom.contains(&format!("snippets/homestack-{name}-")) {
        return Vec::new();
    }
    let dir = cfg.cloud_init.snippet_dir.trim_end_matches('/');
    ["user", "vendor", "network", "meta"]
        .iter()
        .map(|kind| format!("{dir}/homestack-{name}-{kind}.yaml"))
        .chain([format!("{dir}/homestack-vm{vmid}-refresh.json")])
        .collect()
}

/// `destroy_workspace` for a plan of [`plan_destroy`]: the workspace is
/// checked again (also its pool) before the shutdown and again before the
/// VM is destroyed; the local cleanup is the client's.
pub async fn destroy_workspace(
    pve: &impl Pve,
    cfg: &Config,
    pool: &str,
    plan: &Value,
    timing: &LifecycleTiming,
    observer: &impl Observer,
) -> Result<Value, CoreError> {
    let vmid = vmid_of(plan)?;
    let node = text(plan, "node")?;
    let name = text(plan, "name")?;

    require_pool(pve, vmid, pool).await?;
    resolve_existing_workspace(pve, cfg, vmid).await?;

    let step = "Shutdown";
    step_started(observer, step);
    let stopped = shutdown_vm(pve, node, vmid, SHUTDOWN_TIMEOUT_SECS, &timing.wait(600), observer).await;
    step_finished(observer, step, stopped.is_ok());
    stopped?;

    let step = "Destroy";
    step_started(observer, step);
    let destroyed = async {
        let current = resolve_existing_workspace(pve, cfg, vmid).await?;
        require_pool(pve, vmid, pool).await?;
        if Some(current.home_label.as_str()) != plan.get("home_label").and_then(Value::as_str) {
            return Err(CoreError::Failed(
                "Persistent home identity changed after destroy confirmation".to_owned(),
            ));
        }
        delete_task(
            pve,
            &format!("/nodes/{node}/qemu/{vmid}"),
            &[("purge", "1")],
            &format!("Destroying VM {vmid}"),
            &timing.wait(600),
            observer,
        )
        .await?;
        if cluster_vm_resources(pve).await?.iter().any(|r| r.vmid == vmid) {
            return Err(CoreError::Failed(format!("VM {vmid} still exists after qm destroy")));
        }
        Ok(())
    }
    .await;
    step_finished(observer, step, destroyed.is_ok());
    destroyed?;

    let mut result: Map<String, Value> = plan.as_object().cloned().unwrap_or_default();
    result.insert("status".to_owned(), json!("absent"));
    result.insert("vm_deleted".to_owned(), json!(true));
    result.insert("home_deleted".to_owned(), json!(true));
    result.insert("name".to_owned(), json!(name));
    result.insert("event".to_owned(), json!("workspace.destroyed"));
    Ok(Value::Object(result))
}

/// `workspace.start` and `workspace.stop` (owner, 2026-10-14; Python had
/// neither): the plan names the workspace, its node and its state now.
pub async fn plan_power(pve: &impl Pve, cfg: &Config, pool: &str, vmid: u32, start: bool) -> Result<Value, CoreError> {
    require_pool(pve, vmid, pool).await?;
    let info = resolve_existing_workspace(pve, cfg, vmid).await?;
    Ok(json!({
        "command": if start { "start" } else { "stop" },
        "vmid": vmid,
        "name": info.name,
        "node": info.node,
        "pool": pool,
        "status": info.status,
        "ip": derive_ip(cfg, vmid).ok(),
    }))
}

/// Start or stop the workspace of `plan`. A stop shuts the guest down and
/// never forces it, as every operation does; a workspace already in the
/// wanted state is left alone.
pub async fn power_workspace(
    pve: &impl Pve,
    cfg: &Config,
    pool: &str,
    plan: &Value,
    timing: &LifecycleTiming,
    observer: &impl Observer,
) -> Result<Value, CoreError> {
    let vmid = vmid_of(plan)?;
    let node = text(plan, "node")?;
    let start = plan.get("command").and_then(Value::as_str) == Some("start");
    require_pool(pve, vmid, pool).await?;
    resolve_existing_workspace(pve, cfg, vmid).await?;
    let step = if start { "Start" } else { "Shutdown" };
    step_started(observer, step);
    let done = if start {
        if vm_status(pve, node, vmid).await == "running" {
            Ok(())
        } else {
            start_vm(pve, node, vmid, &timing.wait(START_TIMEOUT_SECS), observer).await
        }
    } else {
        shutdown_vm(pve, node, vmid, SHUTDOWN_TIMEOUT_SECS, &timing.wait(600), observer).await
    };
    step_finished(observer, step, done.is_ok());
    done?;
    let mut result: Map<String, Value> = plan.as_object().cloned().unwrap_or_default();
    result.insert("status".to_owned(), json!(vm_status(pve, node, vmid).await));
    result.insert(
        "event".to_owned(),
        json!(if start {
            "workspace.started"
        } else {
            "workspace.stopped"
        }),
    );
    Ok(Value::Object(result))
}
