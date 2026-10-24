//! Destructive ownership review for QEMU workspaces only.

use std::collections::{BTreeMap, BTreeSet};

use hs_model::config::Config;
use serde_json::{Value, json};

use crate::CoreError;
use crate::create_transfer::is_volume_slot;
use crate::inventory::{
    ALIEN_WORKSPACE_TAG, GOLD_TAG, REFRESH_LOCK_TAG, VmConfig, WORKSPACE_TAG, cluster_vm_resources, disk_option,
    disk_volume, has_tag, home_label, vm_config, vm_volume_inventory,
};
use crate::lifecycle::verify_workspace_role_tags;
use crate::pve::Pve;

fn refused(detail: impl std::fmt::Display) -> CoreError {
    CoreError::Refused(format!(
        "Workspace destroy refused: {detail}. Resolve the condition and review a new plan."
    ))
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, CoreError> {
    value[key]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| refused(format!("missing {key}")))
}

fn array<'a>(value: &'a Value, what: &str) -> Result<&'a Vec<Value>, CoreError> {
    value
        .as_array()
        .ok_or_else(|| refused(format!("incomplete {what} inventory")))
}

async fn placement(pve: &impl Pve, vmid: u32, pool: &str) -> Result<String, CoreError> {
    let resources = cluster_vm_resources(pve).await?;
    let mut matches = resources.iter().filter(|r| r.vmid == vmid);
    let resource = matches
        .next()
        .ok_or_else(|| refused("VM missing from cluster inventory"))?;
    if matches.next().is_some() || resource.node.is_empty() {
        return Err(refused("ambiguous VM placement"));
    }
    if resource.pool.as_deref() != Some(pool) {
        return Err(CoreError::OutsidePool(format!("VM {vmid} is not in pool {pool}")));
    }
    Ok(resource.node.clone())
}

async fn require_membership(pve: &impl Pve, node: &str, pool: &str, vmid: u32) -> Result<(), CoreError> {
    let membership = pve.get(&format!("/pools/{pool}"), &[]).await?;
    let members: Vec<_> = array(&membership["members"], "pool member")?
        .iter()
        .filter(|v| v["vmid"].as_u64() == Some(u64::from(vmid)))
        .collect();
    if members.len() != 1 || members[0]["type"] != "qemu" || members[0]["node"] != node {
        return Err(refused("VM placement conflicts with explicit pool membership"));
    }
    // Cluster inventory can lag a migration. Require a node-local QEMU entry too.
    let local = pve.get(&format!("/nodes/{node}/qemu"), &[]).await?;
    if array(&local, "node VM")?
        .iter()
        .filter(|v| v["vmid"].as_u64() == Some(u64::from(vmid)))
        .count()
        != 1
    {
        return Err(refused("VM missing or ambiguous in node inventory"));
    }
    Ok(())
}

fn disks(config: &VmConfig, cfg: &Config, vmid: u32) -> Result<Value, CoreError> {
    let tags = config.get("tags").map(String::as_str);
    if has_tag(tags, ALIEN_WORKSPACE_TAG) {
        return Err(refused("forbidden workspace role tag homestack-alien"));
    }
    verify_workspace_role_tags(vmid, config)?;
    if has_tag(tags, GOLD_TAG) || has_tag(tags, REFRESH_LOCK_TAG) {
        return Err(refused("Gold or unresolved refresh role"));
    }
    for key in ["name", "digest", &cfg.root_disk, &cfg.home_disk] {
        if config.get(key).is_none_or(|s| s.trim().is_empty()) {
            return Err(refused(format!("missing configuration identity {key}")));
        }
    }
    for key in ["template", "protection"] {
        if config.get(key).is_some_and(|v| v != "0") {
            return Err(refused(format!("VM is {key}")));
        }
    }
    for key in config.keys() {
        if key.starts_with("unused")
            || matches!(
                key.as_str(),
                "lock"
                    | "args"
                    | "hookscript"
                    | "vmstate"
                    | "parent"
                    | "snapstate"
                    | "pending"
                    | "snapshots"
                    | "special-sections"
            )
        {
            return Err(refused(format!("unsupported configuration {key}")));
        }
    }
    let inventory = vm_volume_inventory(config, cfg);
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for (slot, spec) in config.iter().filter(|(k, _)| is_volume_slot(k)) {
        let entry = inventory
            .iter()
            .find(|v| &v.slot == slot)
            .ok_or_else(|| refused(format!("incomplete disk {slot}")))?;
        let volume = disk_volume(spec);
        if !volume
            .split_once(':')
            .is_some_and(|(s, v)| !s.is_empty() && !v.is_empty() && v != "cloudinit")
            || !seen.insert(volume)
        {
            return Err(refused(format!("missing or ambiguous exact volume ID for {slot}")));
        }
        // Additional data disks and external CD-ROMs require manual handling.
        // EFI, TPM and Cloud-Init volumes are explicitly included in the review.
        if !matches!(entry.role.as_str(), "root" | "home" | "efi" | "tpm" | "cloud-init") {
            return Err(refused(format!("unexpected disk {slot}")));
        }
        if matches!(entry.role.as_str(), "root" | "home") && disk_option(spec, "media").is_some() {
            return Err(refused(format!("unsupported root/home media at {slot}")));
        }
        let mut options = BTreeSet::new();
        for option in spec.split(',').skip(1) {
            let (key, _) = option.split_once('=').ok_or_else(|| refused("invalid disk options"))?;
            if !options.insert(key) || key == "import-from" {
                return Err(refused("ambiguous or unresolved disk options"));
            }
        }
        result.push(json!({"slot": slot, "role": entry.role, "volume": volume,
            "storage": entry.storage, "config": spec, "serial": disk_option(spec, "serial")}));
    }
    if !result.iter().any(|v| v["role"] == "root") || !result.iter().any(|v| v["role"] == "home") {
        return Err(refused("root/home disk mapping is incomplete"));
    }
    if disk_option(&config[&cfg.home_disk], "serial").as_deref() != Some(home_label(vmid)?.as_str()) {
        return Err(refused("unexpected home serial"));
    }
    Ok(json!(result))
}

/// Read an exact, fail-closed identity. Dynamic usage counters are excluded;
/// storage backend configuration and all other volume attributes are retained.
pub(crate) async fn review(pve: &impl Pve, cfg: &Config, pool: &str, vmid: u32) -> Result<Value, CoreError> {
    if cfg.gold_vmid == Some(vmid) || pool.is_empty() {
        return Err(refused("Gold VM or missing workspace pool"));
    }
    let node = placement(pve, vmid, pool).await?;
    let base = format!("/nodes/{node}/qemu/{vmid}");
    let config = vm_config(pve, &node, vmid).await?;
    let disks = disks(&config, cfg, vmid)?;
    require_membership(pve, &node, pool, vmid).await?;
    let pending = pve.get(&format!("{base}/pending"), &[]).await?;
    for entry in array(&pending, "pending configuration")? {
        text(entry, "key")?;
        if entry.get("pending").is_some() || entry.get("delete").is_some_and(|v| v.as_u64() != Some(0)) {
            return Err(refused("pending configuration changes"));
        }
    }
    let snapshots = pve.get(&format!("{base}/snapshot"), &[]).await?;
    let snapshots = array(&snapshots, "snapshot")?;
    if snapshots.iter().any(|v| v["name"].as_str() != Some("current")) || snapshots.len() > 1 {
        return Err(refused("snapshots or incomplete snapshot identity"));
    }
    let status = pve.get(&format!("{base}/status/current"), &[]).await?;
    if !matches!(status["status"].as_str(), Some("running" | "stopped")) {
        return Err(refused("unknown power state"));
    }
    if status["ha"]["managed"] != json!(0) && status["ha"]["managed"] != json!(false) {
        return Err(refused("managed or unknown HA state"));
    }
    let guest = vmid.to_string();
    let replication = pve
        .get(&format!("/nodes/{node}/replication"), &[("guest", &guest)])
        .await?;
    if !array(&replication, "replication")?.is_empty() {
        return Err(refused("replication jobs"));
    }
    let storage = pve.get("/storage", &[]).await?;
    let mut stores = BTreeMap::new();
    let mut volumes = BTreeMap::new();
    let expected: BTreeSet<&str> = array(&disks, "disk")?
        .iter()
        .filter_map(|v| v["volume"].as_str())
        .collect();
    // Inspect all node-available image stores, not only the configured layout.
    // This also rejects detached owned volumes and left-over fleecing images.
    for store in array(&storage, "storage configuration")? {
        let id = text(store, "storage")?;
        if !text(store, "content")?.split(',').any(|c| c == "images")
            || store["disable"].as_u64() == Some(1)
            || store["nodes"]
                .as_str()
                .is_some_and(|ns| !ns.split(',').any(|n| n == node))
        {
            continue;
        }
        if stores.insert(id, store).is_some() {
            return Err(refused("duplicate storage identity"));
        }
        let state = pve.get(&format!("/nodes/{node}/storage/{id}/status"), &[]).await?;
        if state["active"].as_u64() != Some(1) || state["enabled"].as_u64() != Some(1) {
            return Err(refused(format!("storage {id} inventory unavailable")));
        }
        let content = pve
            .get(&format!("/nodes/{node}/storage/{id}/content"), &[("content", "images")])
            .await?;
        for entry in array(&content, "storage volume")? {
            let volid = text(entry, "volid")?;
            let owner = entry["vmid"]
                .as_u64()
                .ok_or_else(|| refused("volume ownership unavailable"))?;
            if owner != u64::from(vmid) {
                if expected.contains(volid) {
                    return Err(refused(format!("volume {volid} owned by another VM")));
                }
                continue;
            }
            if !expected.contains(volid) || !volid.starts_with(&format!("{id}:")) || volumes.contains_key(volid) {
                return Err(refused(format!("unexpected detached or ambiguous volume {volid}")));
            }
            let mut identity = entry.clone();
            identity
                .as_object_mut()
                .ok_or_else(|| refused("invalid volume identity"))?
                .remove("used");
            text(&identity, "format")?;
            volumes.insert(volid.to_owned(), identity);
        }
    }
    if volumes.len() != expected.len() {
        return Err(refused(
            "exact disk volumes missing from authoritative storage inventory",
        ));
    }
    if placement(pve, vmid, pool).await? != node {
        return Err(refused("VM placement changed while reading inventory"));
    }
    require_membership(pve, &node, pool, vmid).await?;
    if pve.get("/storage", &[]).await? != storage {
        return Err(refused("storage backend configuration changed while reading inventory"));
    }
    let final_status = pve.get(&format!("{base}/status/current"), &[]).await?;
    if final_status["status"] != status["status"] || final_status["ha"] != status["ha"] {
        return Err(refused("power or HA state changed while reading inventory"));
    }
    if vm_config(pve, &node, vmid).await? != config {
        return Err(refused("configuration changed while reading inventory"));
    }
    Ok(json!({"vmid": vmid, "name": config["name"], "node": node, "pool": pool,
        "role": WORKSPACE_TAG, "status": status["status"], "config": config,
        "disks": disks, "storage": stores, "volumes": volumes,
        "snapshots": [], "pending_changes": [], "replication": [], "ha_managed": false}))
}

/// Only this operation's graceful running -> stopped transition is authorized.
pub(crate) async fn validate(
    pve: &impl Pve,
    cfg: &Config,
    pool: &str,
    plan: &Value,
    stopped: bool,
) -> Result<(), CoreError> {
    let vmid = crate::lifecycle::vmid_of(plan)?;
    let mut current = review(pve, cfg, pool, vmid).await?;
    let reviewed = plan
        .get("destructive_identity")
        .filter(|v| v.is_object())
        .ok_or_else(|| refused("reviewed destructive identity missing"))?;
    for key in ["vmid", "name", "node", "pool", "status"] {
        if plan.get(key) != reviewed.get(key) {
            return Err(refused(format!("incomplete or inconsistent reviewed {key}")));
        }
    }
    if plan["command"] != "destroy" {
        return Err(refused("missing destroy command"));
    }
    for disk in array(&reviewed["disks"], "reviewed disk")? {
        if let Some(role @ ("root" | "home")) = disk["role"].as_str() {
            for (suffix, field) in [("disk", "slot"), ("volume", "volume"), ("storage", "storage")] {
                if plan.get(format!("{role}_{suffix}")) != disk.get(field) {
                    return Err(refused(format!("inconsistent reviewed {role} {suffix}")));
                }
            }
            if role == "home" && plan.get("home_label") != disk.get("serial") {
                return Err(refused("inconsistent reviewed home serial"));
            }
        }
    }
    if stopped {
        if current["status"] != "stopped" {
            return Err(refused("VM is not confirmed stopped"));
        }
        current["status"] = reviewed["status"].clone();
    }
    if &current != reviewed {
        return Err(refused(
            "VM configuration, placement, power state or exact storage identity changed",
        ));
    }
    Ok(())
}

/// Proxmox can finish qmdestroy with disk removal warnings. Do not claim
/// persistent home deletion until the reviewed volumes are confirmed absent.
pub(crate) async fn confirm_absent(pve: &impl Pve, plan: &Value) -> Result<(), CoreError> {
    let identity = &plan["destructive_identity"];
    let node = text(identity, "node")?;
    let stores = identity["storage"]
        .as_object()
        .ok_or_else(|| refused("missing reviewed storage"))?;
    let volumes = identity["volumes"]
        .as_object()
        .ok_or_else(|| refused("missing reviewed volumes"))?;
    for storage in stores.keys() {
        let content = pve
            .get(
                &format!("/nodes/{node}/storage/{storage}/content"),
                &[("content", "images")],
            )
            .await?;
        for entry in array(&content, "post-destroy volume")? {
            if volumes.contains_key(text(entry, "volid")?) {
                return Err(CoreError::Failed("VM DELETE finished but a reviewed disk still exists. Inspect storage explicitly; destruction will not be retried.".to_owned()));
            }
        }
    }
    Ok(())
}
