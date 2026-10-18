//! `create` on another node than Gold's (mapping B2; phase 10).
//!
//! Ports `create_transfer.capture_gold_metadata`,
//! `validate_cross_node_create`, `lifecycle._revalidate_stream_source` and
//! `_verify_streamed_restore` with decision 3 = A: instead of Python's
//! `vzdump | ssh qm create` stream, a full clone on Gold's node is migrated
//! offline with its disks to the target node (`POST migrate`, phase 8's
//! task tracking), then checked as Python checked the restored VM.
//! `docs/migration/phase-10.md` lists what differs.

use std::collections::{BTreeMap, BTreeSet};

use hs_model::config::Config;
use serde_json::{Map, Value, json};

use crate::error::CoreError;
use crate::gold::{boot_order_contains_disk, cloud_init_slots, is_disk_slot};
use crate::inventory::{
    VmConfig, cluster_node_statuses, cluster_vm_resources, disk_option, disk_volume, node_network, py_repr, py_str,
    resolve_homestack_storage, vm_config, vm_status,
};
use crate::lifecycle::{LifecycleTiming, require_gold_tag, text, vmid_of};
use crate::migrate::{migrate_offline, migration_volume_plan};
use crate::pve::Pve;
use crate::tasks::Observer;

/// `diskN`, `efidiskN` or `tpmstateN`: the slots whose volumes move.
pub(crate) fn is_volume_slot(slot: &str) -> bool {
    is_disk_slot(slot)
        || ["efidisk", "tpmstate"].iter().any(|prefix| {
            slot.strip_prefix(prefix)
                .is_some_and(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
        })
}

fn is_numbered(key: &str, prefix: &str) -> bool {
    key.strip_prefix(prefix)
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
}

/// `_configured_volume`
fn configured_volume(value: &str) -> &str {
    disk_volume(value).trim()
}

/// `capture_gold_metadata`: Gold's root, Cloud-Init drives, bridges and
/// volumes, without its tags. Python's `backup=0` rule is not kept: it
/// guarded what `vzdump` leaves out, and a migration copies every disk.
pub fn capture_gold_metadata(gold_vmid: u32, gold_cfg: &VmConfig, root_disk: &str) -> Result<Value, CoreError> {
    let root_config = gold_cfg.get(root_disk).map(String::as_str).unwrap_or_default();
    if root_config.is_empty() {
        return Err(CoreError::Failed(format!(
            "Gold VM {gold_vmid} has no {root_disk} disk"
        )));
    }
    let cloud = cloud_init_slots(gold_cfg);
    if cloud.is_empty() {
        return Err(CoreError::Failed(format!(
            "Gold VM {gold_vmid} has no Cloud-Init drive"
        )));
    }
    let used = |value: &String| !matches!(configured_volume(value), "" | "none");
    let mut unsupported: Vec<&str> = gold_cfg
        .iter()
        .filter(|(slot, value)| is_disk_slot(slot) && *slot != root_disk && !cloud.contains(slot) && used(value))
        .map(|(slot, _)| slot.as_str())
        .collect();
    unsupported.extend(
        gold_cfg
            .iter()
            .filter(|(slot, value)| is_numbered(slot, "unused") && used(value))
            .map(|(slot, _)| slot.as_str()),
    );
    if !unsupported.is_empty() {
        return Err(CoreError::Failed(format!(
            "Gold VM {gold_vmid} has unsupported extra data disks: {}",
            unsupported.join(", ")
        )));
    }
    let bridges = network_bridges(gold_cfg, |slot| format!("Gold VM {slot} has no bridge configured"))?;
    if !bridges.contains_key("net0") {
        return Err(CoreError::Failed(format!(
            "Gold VM {gold_vmid} has no bridged net0 NIC"
        )));
    }
    let volumes: Vec<Value> = gold_cfg
        .iter()
        .filter(|(slot, _)| is_volume_slot(slot))
        .filter_map(|(slot, value)| {
            let volume = configured_volume(value);
            if volume.is_empty() || volume == "none" {
                return None;
            }
            let storage = volume.split_once(':').map_or("", |(storage, _)| storage);
            Some(json!({"slot": slot, "volume": volume, "storage": storage}))
        })
        .collect();
    Ok(json!({
        "gold_vmid": gold_vmid,
        "root_disk": root_disk,
        "root_config": root_config,
        "cloudinit_slots": cloud,
        "bridges": bridges,
        "volumes": volumes,
    }))
}

/// `_network_bridges`: the bridge of every `netN` with a value.
fn network_bridges(vm_cfg: &VmConfig, missing: impl Fn(&str) -> String) -> Result<BTreeMap<String, String>, CoreError> {
    vm_cfg
        .iter()
        .filter(|(slot, value)| is_numbered(slot, "net") && !value.trim().is_empty())
        .map(|(slot, value)| {
            disk_option(value, "bridge")
                .map(|bridge| (slot.clone(), bridge))
                .ok_or_else(|| CoreError::Failed(missing(slot)))
        })
        .collect()
}

/// `_storage_is_active`
fn storage_is_active(item: &Map<String, Value>) -> bool {
    ["enabled", "active"].iter().all(|key| {
        item.get(*key).filter(|value| !value.is_null()).is_none_or(|value| {
            !matches!(
                py_str(value).trim().to_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        })
    })
}

/// `_storage_item`: the storage as the target node lists it, active and
/// holding `content`.
fn require_storage(inventory: &[Map<String, Value>], storage: &str, content: &str) -> Result<(), CoreError> {
    let Some(item) = inventory
        .iter()
        .find(|item| item.get("storage").map(py_str).as_deref() == Some(storage))
    else {
        return Err(CoreError::Refused(format!(
            "Target node does not provide storage {}",
            py_repr(Some(storage))
        )));
    };
    if !storage_is_active(item) {
        return Err(CoreError::Refused(format!(
            "Target storage {} is disabled or inactive on node",
            py_repr(Some(storage))
        )));
    }
    let available: BTreeSet<String> = item
        .get("content")
        .map(py_str)
        .unwrap_or_default()
        .split(',')
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
        .collect();
    if !available.contains(content) {
        return Err(CoreError::Refused(format!(
            "Target storage {} does not support {content} content",
            py_repr(Some(storage))
        )));
    }
    Ok(())
}

/// `node_storage_inventory`: `GET /nodes/{node}/storage`, the storages the
/// token may audit or allocate on.
async fn node_storages(pve: &impl Pve, node: &str) -> Result<Vec<Map<String, Value>>, CoreError> {
    match pve.get(&format!("/nodes/{node}/storage"), &[]).await? {
        Value::Array(items) => Ok(items
            .into_iter()
            .filter_map(|item| match item {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .collect()),
        _ => Err(CoreError::Unusable(format!(
            "Proxmox storage inventory on {node} did not return a JSON array"
        ))),
    }
}

/// What planning a create on another node found.
pub(crate) struct Transfer {
    pub target_storage: String,
    pub source_metadata: Value,
}

/// `validate_cross_node_create`, without the SSH checks of the nodes: both
/// nodes online (read before anything else on them), Gold's contract, the
/// target storage with `images` and the snippet storage with `snippets` on
/// the target node, Gold's bridges there.
pub(crate) async fn validate_cross_node_create(
    pve: &impl Pve,
    cfg: &Config,
    target_node: &str,
    gold_cfg: &VmConfig,
    requested_storage: Option<&str>,
) -> Result<Transfer, CoreError> {
    let source_node = cfg.node.as_str();
    let statuses = cluster_node_statuses(pve).await?;
    let state = |node: &str| statuses.iter().find(|state| state.node == node);
    let Some(source) = state(source_node) else {
        return Err(CoreError::Refused(format!(
            "Configured Gold node {} is not in the Proxmox cluster",
            py_repr(Some(source_node))
        )));
    };
    let Some(target) = state(target_node) else {
        return Err(CoreError::Refused(format!(
            "Target node {} is not in the Proxmox cluster",
            py_repr(Some(target_node))
        )));
    };
    if !source.online || source.status != "online" {
        return Err(CoreError::Refused(format!(
            "Configured Gold node {} is offline",
            py_repr(Some(source_node))
        )));
    }
    if !target.online || target.status != "online" {
        return Err(CoreError::Refused(format!(
            "Target node {} is offline",
            py_repr(Some(target_node))
        )));
    }

    let source_metadata = capture_gold_metadata(crate::gold::configured_gold(cfg)?, gold_cfg, &cfg.root_disk)?;
    let target_storage = resolve_homestack_storage(cfg, target_node, requested_storage)?;
    let inventory = node_storages(pve, target_node).await?;
    require_storage(&inventory, &target_storage, "images")?;
    require_storage(&inventory, &cfg.cloud_init.snippet_storage, "snippets")?;

    let available: BTreeSet<String> = node_network(pve, target_node)
        .await?
        .iter()
        .filter(|item| {
            matches!(
                item.get("type").map(py_str).unwrap_or_default().to_lowercase().as_str(),
                "bridge" | "ovsbridge"
            )
        })
        .filter_map(|item| item.get("iface").map(py_str))
        .filter(|iface| !iface.is_empty())
        .collect();
    let wanted: BTreeSet<&str> = source_metadata["bridges"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, bridge)| bridge.as_str())
        .collect();
    let missing: Vec<&str> = wanted
        .into_iter()
        .filter(|bridge| !available.contains(*bridge))
        .collect();
    if !missing.is_empty() {
        return Err(CoreError::Refused(format!(
            "Target node {} is missing Gold network bridge(s): {}",
            py_repr(Some(target_node)),
            missing.join(", ")
        )));
    }
    Ok(Transfer {
        target_storage,
        source_metadata,
    })
}

/// `_revalidate_stream_source`: Gold read and captured again must match the
/// plan.
pub(crate) async fn revalidate_source(pve: &impl Pve, cfg: &Config, plan: &Value) -> Result<(), CoreError> {
    let gold = crate::gold::configured_gold(cfg)?;
    let source_node = text(plan, "source_node")?;
    let gold_cfg = vm_config(pve, source_node, gold).await?;
    require_gold_tag(gold, &gold_cfg)?;
    let metadata = capture_gold_metadata(gold, &gold_cfg, &cfg.root_disk)?;
    if plan.get("source_metadata") != Some(&metadata) {
        return Err(CoreError::Refused(
            "Gold VM source configuration changed after create confirmation; re-run create and review the new plan"
                .to_owned(),
        ));
    }
    Ok(())
}

/// `_restored_volume_storage`
fn volume_storage(value: &str) -> Option<&str> {
    configured_volume(value).split_once(':').map(|(storage, _)| storage)
}

/// `_volume_belongs_to_vm`
pub(crate) fn volume_belongs_to_vm(volume: &str, vmid: u32) -> bool {
    let id = vmid.to_string();
    // `(?:^|/)(<vmid>)/` on the whole volume.
    let parts: Vec<&str> = volume.split('/').collect();
    let in_directory = parts[..parts.len() - 1].contains(&id.as_str());
    let tail = volume.split_once(':').map_or(volume, |(_, path)| path);
    let tail = tail.rsplit('/').next().unwrap_or(tail);
    let marker = format!("vm-{id}");
    let named = tail.match_indices(&marker).any(|(at, _)| {
        let before = at == 0 || tail.as_bytes()[at - 1] == b'-';
        let rest = &tail[at + marker.len()..];
        before && (rest.is_empty() || rest.starts_with('-'))
    });
    in_directory || named
}

/// `_disk_option_values`: the options after the volume, `format` aside,
/// sorted.
fn disk_option_values(value: &str) -> Vec<(String, String)> {
    let Some((_, options)) = value.split_once(',') else {
        return Vec::new();
    };
    let mut values: Vec<(String, String)> = options
        .split(',')
        .filter_map(|item| item.split_once('='))
        .filter(|(key, _)| *key != "format")
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    values.sort();
    values
}

fn show_options(values: &[(String, String)]) -> String {
    values
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// `_verify_streamed_restore` on the migrated clone: `node` is where
/// `cluster/resources` lists it, `status` its power state there.
pub fn check_migrated_clone(
    cfg: &Config,
    plan: &Value,
    node: &str,
    status: &str,
    vm_cfg: &VmConfig,
) -> Result<(), CoreError> {
    let vmid = vmid_of(plan)?;
    let expected_node = text(plan, "node")?;
    let name = text(plan, "name")?;
    let expected_storage = text(plan, "root_storage")?;
    let failed = |detail: String| CoreError::Failed(format!("Migrated Gold clone VM {vmid} {detail}"));
    if node != expected_node {
        return Err(failed(format!(
            "is on {}, expected {}",
            py_repr(Some(node)),
            py_repr(Some(expected_node))
        )));
    }
    if status != "stopped" {
        return Err(failed(format!(
            "on {} with status {}; expected stopped",
            py_repr(Some(node)),
            py_repr(Some(status))
        )));
    }
    let root_config = vm_cfg.get(&cfg.root_disk).map(String::as_str).unwrap_or_default();
    if root_config.is_empty() {
        return Err(failed(format!("has no {}", cfg.root_disk)));
    }
    if volume_storage(root_config) != Some(expected_storage) {
        return Err(failed(format!(
            "root disk is not on target storage {}: {root_config}",
            py_repr(Some(expected_storage))
        )));
    }

    let metadata = plan
        .get("source_metadata")
        .and_then(Value::as_object)
        .ok_or_else(|| CoreError::Invalid("The plan has no source metadata".to_owned()))?;
    let cloud: Vec<&str> = metadata
        .get("cloudinit_slots")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if cloud.is_empty() {
        return Err(CoreError::Invalid("The plan has no Cloud-Init metadata".to_owned()));
    }
    for slot in &cloud {
        let drive = vm_cfg.get(*slot).map(String::as_str).unwrap_or_default();
        if !drive.contains("cloudinit") || disk_option(drive, "media").as_deref() != Some("cdrom") {
            return Err(failed(format!("is missing Cloud-Init drive {slot}")));
        }
    }

    let records: Vec<&Map<String, Value>> = metadata
        .get("volumes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .collect();
    let source_volumes: BTreeSet<&str> = records
        .iter()
        .filter_map(|record| record.get("volume").and_then(Value::as_str))
        .collect();
    let expected_slots: BTreeSet<&str> = records
        .iter()
        .filter_map(|record| record.get("slot").and_then(Value::as_str))
        .collect();
    let attached: Vec<(&str, &str)> = vm_cfg
        .iter()
        .filter(|(slot, _)| is_volume_slot(slot))
        .filter(|(_, value)| !matches!(configured_volume(value), "" | "none"))
        .map(|(slot, value)| (slot.as_str(), value.as_str()))
        .collect();
    let actual_slots: BTreeSet<&str> = attached.iter().map(|(slot, _)| *slot).collect();
    if actual_slots != expected_slots {
        let mut details = Vec::new();
        let missing: Vec<&str> = expected_slots.difference(&actual_slots).copied().collect();
        let extra: Vec<&str> = actual_slots.difference(&expected_slots).copied().collect();
        if !missing.is_empty() {
            details.push(format!("missing {}", missing.join(", ")));
        }
        if !extra.is_empty() {
            details.push(format!("unexpected {}", extra.join(", ")));
        }
        return Err(failed(format!("attached disk slots changed: {}", details.join("; "))));
    }
    if let Some(volume) = attached
        .iter()
        .map(|(_, value)| configured_volume(value))
        .find(|volume| source_volumes.contains(volume))
    {
        return Err(failed(format!(
            "still references source volume {}",
            py_repr(Some(volume))
        )));
    }
    for (_, value) in &attached {
        let volume = configured_volume(value);
        let storage = volume_storage(value);
        if storage != Some(expected_storage) {
            return Err(failed(format!(
                "volume {} is on {}, expected target storage {}",
                py_repr(Some(volume)),
                py_repr(storage),
                py_repr(Some(expected_storage))
            )));
        }
        if !volume_belongs_to_vm(volume, vmid) {
            return Err(failed(format!(
                "volume {} does not have target VM ownership",
                py_repr(Some(volume))
            )));
        }
    }

    let expected_root = disk_option_values(metadata.get("root_config").and_then(Value::as_str).unwrap_or_default());
    let actual_root = disk_option_values(root_config);
    if expected_root != actual_root {
        return Err(failed(format!(
            "root disk options changed: expected {}, got {}",
            show_options(&expected_root),
            show_options(&actual_root)
        )));
    }
    let expected_bridges: BTreeMap<String, String> = metadata
        .get("bridges")
        .and_then(Value::as_object)
        .ok_or_else(|| CoreError::Invalid("The plan has no Gold network metadata".to_owned()))?
        .iter()
        .map(|(slot, bridge)| (slot.clone(), py_str(bridge)))
        .collect();
    let actual_bridges = network_bridges(vm_cfg, |slot| {
        format!("Migrated Gold clone VM {vmid} NIC {slot} has no bridge")
    })?;
    if actual_bridges != expected_bridges {
        return Err(failed(format!(
            "network bridges changed: expected {expected_bridges:?}, got {actual_bridges:?}"
        )));
    }
    let actual_name = vm_cfg.get("name").map(String::as_str).unwrap_or_default();
    if actual_name != name {
        return Err(failed(format!(
            "has name {}, expected {}",
            py_repr(Some(actual_name)),
            py_repr(Some(name))
        )));
    }
    if !matches!(vm_cfg.get("template").map_or("0", String::as_str), "0" | "false" | "no") {
        return Err(failed("is still marked as a template".to_owned()));
    }
    if !boot_order_contains_disk(
        vm_cfg.get("boot").map(String::as_str).unwrap_or_default(),
        &cfg.root_disk,
    ) {
        return Err(failed(format!("boot order does not include {}", cfg.root_disk)));
    }
    Ok(())
}

/// `Migrate to target node`: the clone, stopped and without a home, moves
/// from Gold's node with its disks to the target storage.
pub(crate) async fn migrate_clone(
    pve: &impl Pve,
    cfg: &Config,
    plan: &Value,
    timing: &LifecycleTiming,
    observer: &impl Observer,
) -> Result<(), CoreError> {
    let vmid = vmid_of(plan)?;
    let source = text(plan, "source_node")?;
    let target = text(plan, "node")?;
    let storage = text(plan, "root_storage")?;
    let volumes = migration_volume_plan(&vm_config(pve, source, vmid).await?, cfg);
    migrate_offline(pve, source, target, vmid, storage, &volumes, timing, observer).await
}

/// `Verify migrated clone`: where `cluster/resources` lists it, its power
/// state and config there.
pub(crate) async fn verify_migrated_clone(pve: &impl Pve, cfg: &Config, plan: &Value) -> Result<(), CoreError> {
    let vmid = vmid_of(plan)?;
    let node = cluster_vm_resources(pve)
        .await?
        .into_iter()
        .find(|resource| resource.vmid == vmid)
        .map(|resource| resource.node)
        .unwrap_or_default();
    let target = text(plan, "node")?;
    if node != target {
        return check_migrated_clone(cfg, plan, &node, "unknown", &VmConfig::new());
    }
    let status = vm_status(pve, &node, vmid).await;
    let vm_cfg = vm_config(pve, &node, vmid).await?;
    check_migrated_clone(cfg, plan, &node, &status, &vm_cfg)
}
