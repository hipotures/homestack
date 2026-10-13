//! Cluster inventory and VM configuration helpers (`proxmox.py`,
//! `status.vm_volume_inventory`).
//!
//! Proxmox data is read leniently, as Python reads it: numbers may arrive as
//! numbers or strings (`integer_value`), configuration values are kept as
//! text, and a missing or empty value counts as absent.

use std::collections::{BTreeMap, BTreeSet};

use hs_model::config::Config;
use hs_model::status::{NodeState, VolumeEntry};
use serde_json::{Map, Value};

use crate::error::CoreError;
use crate::pve::Pve;

pub const GOLD_TAG: &str = "homestack-gold";
pub const WORKSPACE_TAG: &str = "homestack-ws";
pub const ALIEN_WORKSPACE_TAG: &str = "homestack-alien";
pub const REFRESH_LOCK_TAG: &str = "homestack-lock";
pub const HOME_LABEL_PREFIX: &str = "HS_HOME_";

/// A VM configuration with every value as text (`qm_config_on_node`).
pub type VmConfig = BTreeMap<String, String>;

/// A QEMU VM from `/cluster/resources`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusResource {
    pub vmid: u32,
    pub node: String,
    pub name: Option<String>,
    pub status: Option<String>,
    pub tags: Option<String>,
    /// Only with `Pool.Audit` on the pool.
    pub pool: Option<String>,
}

/// Python's `str(value)` for JSON scalars.
#[must_use]
pub fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

/// Python's `repr()` of an optional string, as used in messages.
#[must_use]
pub fn py_repr(value: Option<&str>) -> String {
    value.map_or_else(|| "None".to_owned(), |text| format!("'{text}'"))
}

/// Python's truthiness for a JSON value.
#[must_use]
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null | Value::Bool(false)) => false,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
        Some(Value::Bool(true)) => true,
    }
}

/// `str(value)` of a truthy value, else `None` (Python's `value or None`).
#[must_use]
pub fn text_or_none(value: Option<&Value>) -> Option<String> {
    truthy(value).then(|| value.map(py_str).unwrap_or_default())
}

/// `models.integer_value`: `int(value)`, or `None`.
#[must_use]
pub fn integer_value(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::Bool(flag) => Some(i64::from(*flag)),
        Value::String(text) => text.trim().replace('_', "").parse().ok(),
        _ => None,
    }
}

fn non_negative(value: Option<&Value>) -> Option<u64> {
    integer_value(value).and_then(|n| u64::try_from(n).ok())
}

/// `parse_tags`: semicolon-separated, trimmed, non-empty.
#[must_use]
pub fn parse_tags(value: Option<&str>) -> BTreeSet<String> {
    value
        .unwrap_or_default()
        .split(';')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned)
        .collect()
}

#[must_use]
pub fn has_tag(value: Option<&str>, tag: &str) -> bool {
    parse_tags(value).contains(tag)
}

/// `disk_storage`: the storage ID of a disk specification.
#[must_use]
pub fn disk_storage(disk_config: Option<&str>) -> Option<String> {
    let volume = disk_config.filter(|c| !c.is_empty())?.split(',').next()?.trim();
    let (storage, _) = volume.split_once(':')?;
    (!storage.is_empty()).then(|| storage.to_owned())
}

/// `disk_option`: `key=value` after the volume of a disk specification.
#[must_use]
pub fn disk_option(disk_config: &str, key: &str) -> Option<String> {
    disk_config
        .split(',')
        .skip(1)
        .filter_map(|item| item.split_once('='))
        .find(|(item_key, _)| *item_key == key)
        .map(|(_, value)| value.to_owned())
}

/// The volume of a disk specification (`value.split(",", 1)[0].strip()`).
#[must_use]
pub fn disk_volume(disk_config: &str) -> &str {
    disk_config.split(',').next().unwrap_or_default().trim()
}

fn slot_parts(slot: &str) -> Option<(&str, u32)> {
    let digits = slot.len() - slot.trim_start_matches(|c: char| c.is_ascii_alphabetic()).len();
    let (prefix, index) = slot.split_at(digits);
    (!index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
        .then(|| index.parse().ok().map(|n| (prefix, n)))
        .flatten()
}

/// `attached_disk_volumes`: volumes of disk slots (not `unusedN`).
#[must_use]
pub fn attached_disk_volumes(vm_cfg: &VmConfig) -> BTreeSet<String> {
    vm_cfg
        .iter()
        .filter(|(key, _)| {
            slot_parts(key).is_some_and(|(prefix, _)| {
                matches!(prefix, "ide" | "sata" | "scsi" | "virtio" | "efidisk" | "tpmstate")
            })
        })
        .map(|(_, value)| disk_volume(value))
        .filter(|volume| volume.contains(':') && *volume != "none")
        .map(str::to_owned)
        .collect()
}

/// `homestack_storage_layout_name`: `homestack-storage-<N>` for a node whose
/// name ends in a number without leading zero.
#[must_use]
pub fn layout_name_for_node(node: &str) -> Option<String> {
    let digits = node.len() - node.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let number = node[node.len() - digits..].trim_start_matches('0');
    (!number.is_empty()).then(|| format!("homestack-storage-{number}"))
}

/// `homestack_storage_ids_for_node`
#[must_use]
pub fn storage_ids_for_node<'c>(cfg: &'c Config, node: &str) -> &'c [String] {
    layout_name_for_node(node)
        .and_then(|layout| cfg.storage_layouts.get(&layout))
        .map_or(&[], Vec::as_slice)
}

/// `resolve_homestack_storage`
pub fn resolve_homestack_storage(cfg: &Config, node: &str, requested: Option<&str>) -> Result<String, CoreError> {
    let storages = storage_ids_for_node(cfg, node);
    let Some(default) = storages.first() else {
        let via = layout_name_for_node(node)
            .map(|layout| format!(" via {layout}"))
            .unwrap_or_default();
        return Err(CoreError::Invalid(format!(
            "No HomeStack storages are assigned to node '{node}'{via}"
        )));
    };
    let Some(requested) = requested else {
        return Ok(default.clone());
    };
    let storage = requested.trim();
    if storages.iter().any(|s| s == storage) {
        Ok(storage.to_owned())
    } else {
        Err(CoreError::Invalid(format!(
            "Storage '{storage}' is not assigned to node '{node}'; allowed HomeStack storages: {}",
            storages.join(", ")
        )))
    }
}

/// A HomeStack volume name: `vm-<vmid>-hs-(root|home)-<name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeIdentity {
    pub volid: String,
    pub vmid: u32,
    pub role: String,
    pub name: String,
}

/// `homestack_volume_identity`
#[must_use]
pub fn volume_identity(volid: &str) -> Option<VolumeIdentity> {
    let (_, path) = volid.split_once(':')?;
    let mut basename = path.rsplit('/').next().unwrap_or(path);
    for extension in [".raw", ".qcow2", ".vmdk"] {
        if let Some(stripped) = basename.strip_suffix(extension) {
            basename = stripped;
            break;
        }
    }
    let rest = basename.strip_prefix("vm-")?;
    let (vmid, rest) = rest.split_once("-hs-")?;
    if vmid.is_empty() || !vmid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (role, name) = rest.split_once('-')?;
    let valid_name = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if !matches!(role, "root" | "home") || !valid_name {
        return None;
    }
    Some(VolumeIdentity {
        volid: volid.to_owned(),
        vmid: vmid.parse().ok()?,
        role: role.to_owned(),
        name: name.to_owned(),
    })
}

/// `home_label`: `HS_HOME_<vmid>`, at most 16 bytes (ext4 labels).
pub fn home_label(vmid: u32) -> Result<String, CoreError> {
    let label = format!("{HOME_LABEL_PREFIX}{vmid}");
    if label.len() > 16 {
        return Err(CoreError::Invalid(format!(
            "Home filesystem label is too long for ext4: {label}"
        )));
    }
    Ok(label)
}

/// `guest.parse_disk_size_gb`
#[must_use]
pub fn parse_disk_size_gb(disk_config: &str) -> Option<f64> {
    let value = disk_config.split(',').find_map(|item| item.strip_prefix("size="))?;
    let unit = value.chars().last()?;
    let number = &value[..value.len() - unit.len_utf8()];
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    let factor = match unit {
        'K' => 1.0 / (1024.0 * 1024.0),
        'M' => 1.0 / 1024.0,
        'G' => 1.0,
        'T' => 1024.0,
        _ => return None,
    };
    number.parse::<f64>().ok().map(|n| n * factor)
}

/// `status.vm_volume_inventory`: Proxmox-backed volumes, `unusedN` included,
/// by bus order.
#[must_use]
pub fn vm_volume_inventory(vm_cfg: &VmConfig, cfg: &Config) -> Vec<VolumeEntry> {
    const ORDER: [&str; 7] = ["ide", "sata", "scsi", "virtio", "efidisk", "tpmstate", "unused"];
    let managed_layout = !has_tag(vm_cfg.get("tags").map(String::as_str), ALIEN_WORKSPACE_TAG);
    let mut entries: Vec<((usize, u32), VolumeEntry)> = vm_cfg
        .iter()
        .filter_map(|(slot, value)| {
            let (prefix, index) = slot_parts(slot)?;
            let order = ORDER.iter().position(|p| *p == prefix)?;
            let volume = disk_volume(value);
            if volume.is_empty() || volume == "none" || !volume.contains(':') {
                return None;
            }
            let role = if managed_layout && *slot == cfg.root_disk {
                "root"
            } else if managed_layout && *slot == cfg.home_disk {
                "home"
            } else if prefix == "unused" {
                "unused"
            } else if prefix == "efidisk" {
                "efi"
            } else if prefix == "tpmstate" {
                "tpm"
            } else if disk_option(value, "media").as_deref() == Some("cdrom") && volume.contains("cloudinit") {
                "cloud-init"
            } else {
                "disk"
            };
            Some((
                (order, index),
                VolumeEntry {
                    slot: slot.clone(),
                    role: role.to_owned(),
                    volume: volume.to_owned(),
                    storage: disk_storage(Some(value)),
                    size: disk_option(value, "size"),
                },
            ))
        })
        .collect();
    entries.sort_by_key(|(key, _)| *key);
    entries.into_iter().map(|(_, entry)| entry).collect()
}

/// `cluster_node_statuses`: nodes by name, `online` unless Proxmox says
/// `offline`.
pub async fn cluster_node_statuses(pve: &impl Pve) -> Result<Vec<NodeState>, CoreError> {
    let Value::Array(items) = pve.get("/nodes", &[]).await? else {
        return Err(CoreError::Unusable(
            "Proxmox node inventory did not return a JSON array".to_owned(),
        ));
    };
    let mut nodes: Vec<NodeState> = items
        .iter()
        .filter(|item| truthy(item.get("node")))
        .map(|item| {
            let status = text_or_none(item.get("status"))
                .unwrap_or_else(|| "unknown".to_owned())
                .trim()
                .to_lowercase();
            NodeState {
                node: py_str(&item["node"]),
                online: status != "offline",
                status,
            }
        })
        .collect();
    nodes.sort_by(|a, b| a.node.cmp(&b.node));
    if nodes.is_empty() {
        return Err(CoreError::Unusable("Proxmox cluster returned no nodes".to_owned()));
    }
    Ok(nodes)
}

/// The QEMU entries of `/cluster/resources?type=vm` in API order, with the
/// VMID as an integer (`cluster_vm_resources` before sorting).
pub async fn cluster_vm_maps(pve: &impl Pve) -> Result<Vec<(u32, Map<String, Value>)>, CoreError> {
    let Value::Array(items) = pve.get("/cluster/resources", &[("type", "vm")]).await? else {
        return Err(CoreError::Unusable(
            "Proxmox VM inventory did not return a JSON array".to_owned(),
        ));
    };
    Ok(items
        .into_iter()
        .filter_map(|item| match item {
            Value::Object(mut map) if map.get("type").and_then(Value::as_str) == Some("qemu") => {
                let vmid = integer_value(map.get("vmid")).and_then(|v| u32::try_from(v).ok())?;
                map.insert("vmid".to_owned(), Value::from(vmid));
                Some((vmid, map))
            }
            _ => None,
        })
        .collect())
}

/// `cluster_vm_resources`: QEMU VMs by VMID.
pub async fn cluster_vm_resources(pve: &impl Pve) -> Result<Vec<StatusResource>, CoreError> {
    let mut resources: Vec<StatusResource> = cluster_vm_maps(pve)
        .await?
        .into_iter()
        .map(|(vmid, map)| StatusResource {
            vmid,
            node: text_or_none(map.get("node")).unwrap_or_default(),
            name: text_or_none(map.get("name")),
            status: text_or_none(map.get("status")),
            tags: text_or_none(map.get("tags")),
            pool: text_or_none(map.get("pool")),
        })
        .collect();
    resources.sort_by_key(|r| r.vmid);
    Ok(resources)
}

/// `qm_config_on_node`
pub async fn vm_config(pve: &impl Pve, node: &str, vmid: u32) -> Result<VmConfig, CoreError> {
    let Value::Object(map) = pve.get(&format!("/nodes/{node}/qemu/{vmid}/config"), &[]).await? else {
        return Err(CoreError::Unusable(format!(
            "Proxmox config for VM {vmid} on {node} did not return a JSON object"
        )));
    };
    Ok(map
        .into_iter()
        .map(|(key, value)| {
            let text = if value.is_null() { String::new() } else { py_str(&value) };
            (key, text)
        })
        .collect())
}

/// `qm set VMID --KEY VALUE ... [--digest D]` through `PUT config`
/// (synchronous). With `digest`, Proxmox refuses the change when the config
/// changed since it was read (`lifecycle._digest_args`, mapping T5). A
/// change refused for a taken lock is tried again
/// ([`crate::tasks::retry_while_locked`]).
pub async fn set_vm_config(
    pve: &impl Pve,
    node: &str,
    vmid: u32,
    values: &[(&str, &str)],
    digest: Option<&str>,
) -> Result<(), CoreError> {
    let mut form = values.to_vec();
    if let Some(digest) = digest.map(str::trim).filter(|digest| !digest.is_empty()) {
        form.push(("digest", digest));
    }
    let path = format!("/nodes/{node}/qemu/{vmid}/config");
    crate::tasks::retry_while_locked(std::time::Duration::from_secs(1), || async {
        Ok(pve.put(&path, &form).await?)
    })
    .await?;
    Ok(())
}

/// `qm_status_on_node`: `absent` when the call fails, `unknown` when the
/// answer is unusable.
pub async fn vm_status(pve: &impl Pve, node: &str, vmid: u32) -> String {
    match pve.get(&format!("/nodes/{node}/qemu/{vmid}/status/current"), &[]).await {
        Err(_) => "absent".to_owned(),
        Ok(Value::Object(map)) => text_or_none(map.get("status"))
            .map(|status| status.trim().to_owned())
            .filter(|status| !status.is_empty())
            .unwrap_or_else(|| "unknown".to_owned()),
        Ok(_) => "unknown".to_owned(),
    }
}

/// `node_network_inventory`: interfaces with an `iface`.
pub async fn node_network(pve: &impl Pve, node: &str) -> Result<Vec<Map<String, Value>>, CoreError> {
    match pve.get(&format!("/nodes/{node}/network"), &[]).await? {
        Value::Array(items) => Ok(items
            .into_iter()
            .filter_map(|item| match item {
                Value::Object(map) if truthy(map.get("iface")) => Some(map),
                _ => None,
            })
            .collect()),
        _ => Err(CoreError::Unusable(format!(
            "Proxmox network inventory on {node} did not return a JSON array"
        ))),
    }
}

/// `node_dns_config`
pub async fn node_dns(pve: &impl Pve, node: &str) -> Result<Map<String, Value>, CoreError> {
    match pve.get(&format!("/nodes/{node}/dns"), &[]).await? {
        Value::Object(map) => Ok(map),
        _ => Err(CoreError::Unusable(format!(
            "Proxmox DNS configuration on {node} did not return a JSON object"
        ))),
    }
}

/// `cluster_storage_definitions`: definitions with a `storage`.
pub async fn storage_definitions(pve: &impl Pve) -> Result<Vec<Map<String, Value>>, CoreError> {
    match pve.get("/storage", &[]).await? {
        Value::Array(items) => Ok(items
            .into_iter()
            .filter_map(|item| match item {
                Value::Object(map) if truthy(map.get("storage")) => Some(map),
                _ => None,
            })
            .collect()),
        _ => Err(CoreError::Unusable(
            "Proxmox storage configuration did not return a JSON array".to_owned(),
        )),
    }
}

/// Values read as Python reads them: `str(x)` or `int(x)`.
pub(crate) fn u64_value(value: Option<&Value>) -> Option<u64> {
    non_negative(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn integer_value_follows_python_int() {
        assert_eq!(integer_value(Some(&json!(5))), Some(5));
        assert_eq!(integer_value(Some(&json!("5"))), Some(5));
        assert_eq!(integer_value(Some(&json!(" 7 "))), Some(7));
        assert_eq!(integer_value(Some(&json!(2.9))), Some(2));
        assert_eq!(integer_value(Some(&json!(true))), Some(1));
        assert_eq!(integer_value(Some(&json!("2.5"))), None);
        assert_eq!(integer_value(Some(&json!(null))), None);
    }

    #[test]
    fn disk_sizes_like_python() {
        assert_eq!(parse_disk_size_gb("x:y,size=20G"), Some(20.0));
        assert_eq!(parse_disk_size_gb("x:y,size=4M,ssd=1"), Some(4.0 / 1024.0));
        assert_eq!(parse_disk_size_gb("x:y,size=1T"), Some(1024.0));
        assert_eq!(parse_disk_size_gb("x:y,size=20"), None);
        assert_eq!(parse_disk_size_gb("x:y"), None);
    }

    #[test]
    fn volume_names_with_extensions() {
        assert!(volume_identity("s:vm-1-hs-home-a.b").is_some_and(|v| v.name == "a.b"));
        assert!(volume_identity("s:vm-1-hs-home-a.qcow2").is_some_and(|v| v.name == "a"));
        assert!(volume_identity("s:vm-x-hs-home-a").is_none());
        assert!(volume_identity("no-colon").is_none());
    }
}
