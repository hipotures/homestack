//! Global and per-VM status, storage capacity, detached volumes and
//! workspace targets (`status.py`, `proxmox.orphaned_homestack_volumes`,
//! `lifecycle.resolve_workspace_target`).
//!
//! Guest statistics use `VM.GuestAgent.Audit`: `ping`, `get-host-name`,
//! `network-get-interfaces` and `get-fsinfo` stand in for Python's `exec` of
//! `hostname`, `ip`, `findmnt`, `blkid` and `df` (mapping A2, D3). The home
//! filesystem label is read from its ext4 superblock with `VM.GuestAgent.FileRead`.

use std::collections::{BTreeMap, BTreeSet};

use hs_model::config::Config;
use hs_model::status::{
    Capacity, GlobalStatus, GuestBackup, HomeUsage, HomesSummary, LayoutCapacity, NodeState, OrphanVolume,
    ServiceStatus, SshCommands, StatusEntry, StatusSummary, StorageStatus, WorkspaceStatus,
};
use serde_json::{Map, Value};

use crate::error::CoreError;
pub use crate::inventory::StatusResource;
use crate::inventory::{
    ALIEN_WORKSPACE_TAG, GOLD_TAG, VmConfig, WORKSPACE_TAG, cluster_container_resources, cluster_node_statuses,
    cluster_quorum, cluster_vm_maps, cluster_vm_resources, disk_option, disk_storage, disk_volume, has_tag, home_label,
    integer_value, layout_name_for_node, parse_disk_size_gb, parse_tags, py_repr, py_str, storage_ids_for_node,
    tag_list, truthy, u64_value, vm_config, vm_status, vm_volume_inventory, volume_identity,
};
use crate::pve::Pve;

/// `expected_ip`: the workspace's address (`hs_model::address::address_of`).
#[must_use]
pub fn expected_ip(cfg: &Config, vmid: u32) -> Option<String> {
    hs_model::address::address_of(vmid, &cfg.network).map(|address| address.to_string())
}

/// `homestack_status_resources`: Gold and VMs tagged as workspaces.
#[must_use]
pub fn status_resources<'r>(resources: &'r [StatusResource], cfg: &Config) -> Vec<&'r StatusResource> {
    resources
        .iter()
        .filter(|item| {
            Some(item.vmid) == cfg.gold_vmid
                || has_tag(item.tags.as_deref(), WORKSPACE_TAG)
                || has_tag(item.tags.as_deref(), ALIEN_WORKSPACE_TAG)
        })
        .collect()
}

/// `storage_capacity`
#[must_use]
pub fn storage_capacity(name: &str, data: &Map<String, Value>) -> Capacity {
    let total = u64_value(data.get("total"));
    let used = u64_value(data.get("used"));
    let available = u64_value(data.get("avail")).or_else(|| u64_value(data.get("available")));
    #[allow(
        clippy::cast_precision_loss,
        reason = "Python computes the percentage in floating point too"
    )]
    let percent = match (total, used) {
        (Some(total), Some(used)) if total != 0 => Some(100.0 * used as f64 / total as f64),
        _ => None,
    };
    Capacity {
        name: name.to_owned(),
        total_bytes: total,
        used_bytes: used,
        available_bytes: available,
        percent,
    }
}

/// All configured storages, with shared capacity counted once and backup ages
/// based on completed backup volumes rather than scheduled backup jobs.
pub async fn storage_inventory(
    pve: &impl Pve,
    nodes: &[NodeState],
) -> (Vec<StorageStatus>, Vec<GuestBackup>, bool, Vec<String>) {
    let mut warnings = Vec::new();
    let definitions = match crate::inventory::storage_definitions(pve).await {
        Ok(definitions) => definitions,
        Err(error) => {
            return (
                Vec::new(),
                Vec::new(),
                false,
                vec![format!("Cannot read storage definitions: {error}")],
            );
        }
    };
    let mut inventories = BTreeMap::new();
    for node in nodes.iter().filter(|node| node.online) {
        match pve.get(&format!("/nodes/{}/storage", node.node), &[]).await {
            Ok(Value::Array(items)) => {
                inventories.insert(node.node.clone(), items);
            }
            result => warnings.push(format!("Cannot read storage inventory on {}: {result:?}", node.node)),
        }
    }
    let mut storages: Vec<StorageStatus> = Vec::new();
    let mut backups: BTreeMap<u32, GuestBackup> = BTreeMap::new();
    let mut complete = true;
    for definition in definitions {
        let id = definition["storage"].as_str().unwrap_or_default();
        let shared = truthy(definition.get("shared"));
        let content = definition.get("content").and_then(Value::as_str).unwrap_or_default();
        let backup = content.split(',').any(|kind| kind == "backup");
        let assigned = definition.get("nodes").and_then(Value::as_str);
        let mut scanned = false;
        for node in nodes
            .iter()
            .filter(|node| assigned.is_none_or(|list| list.split(',').any(|name| name == node.node)))
        {
            let data = inventories
                .get(&node.node)
                .and_then(|items| items.iter().find(|item| item["storage"] == id))
                .and_then(Value::as_object);
            let available = data.is_some_and(|data| truthy(data.get("active")) && truthy(data.get("enabled")));
            let capacity = storage_capacity(id, data.unwrap_or(&Map::new()));
            if shared
                && let Some(existing) = storages
                    .iter_mut()
                    .find(|storage| storage.shared && storage.storage == id)
            {
                existing.nodes.push(node.node.clone());
                if available && !existing.available {
                    existing.capacity = capacity;
                    existing.available = true;
                }
            } else {
                storages.push(StorageStatus {
                    storage: id.to_owned(),
                    storage_type: definition
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned(),
                    content: content.to_owned(),
                    shared,
                    nodes: vec![node.node.clone()],
                    capacity,
                    available,
                });
            }
            if !backup || (shared && scanned) {
                continue;
            }
            if !available {
                if !shared {
                    complete = false;
                }
                continue;
            }
            match pve
                .get(
                    &format!("/nodes/{}/storage/{id}/content", node.node),
                    &[("content", "backup")],
                )
                .await
            {
                Ok(Value::Array(volumes)) => {
                    scanned = true;
                    for volume in volumes {
                        let (Some(vmid), Some(created_at)) = (
                            u64_value(volume.get("vmid")).and_then(|id| u32::try_from(id).ok()),
                            u64_value(volume.get("ctime")),
                        ) else {
                            continue;
                        };
                        let candidate = GuestBackup {
                            vmid,
                            created_at,
                            storage: id.to_owned(),
                        };
                        if backups.get(&vmid).is_none_or(|old| old.created_at < created_at) {
                            backups.insert(vmid, candidate);
                        }
                    }
                }
                result => {
                    if !shared {
                        complete = false;
                    }
                    warnings.push(format!("Cannot read backups on {id} ({}): {result:?}", node.node));
                }
            }
        }
        if backup && !scanned {
            complete = false;
        }
    }
    storages.sort_by(|a, b| (&a.storage, &a.nodes).cmp(&(&b.storage, &b.nodes)));
    (storages, backups.into_values().collect(), complete, warnings)
}

/// `homestack_storage_capacities`: every storage of every node's layout.
pub async fn storage_capacities(
    pve: &impl Pve,
    cfg: &Config,
    nodes: &[NodeState],
) -> (Vec<LayoutCapacity>, Vec<String>) {
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for node in nodes {
        let Some(layout) = layout_name_for_node(&node.node) else {
            continue;
        };
        for storage in storage_ids_for_node(cfg, &node.node) {
            let (capacity, ok) = if node.online {
                match pve
                    .get(&format!("/nodes/{}/storage/{storage}/status", node.node), &[])
                    .await
                {
                    Ok(Value::Object(data)) => (storage_capacity(storage, &data), true),
                    Ok(_) => {
                        warnings.push(format!(
                            "HomeStack storage capacity for {layout} ({storage} on {}) did not return a JSON object",
                            node.node
                        ));
                        (storage_capacity(storage, &Map::new()), false)
                    }
                    Err(error) => {
                        warnings.push(format!(
                            "HomeStack storage capacity is unavailable for {layout} ({storage} on {}): {error}",
                            node.node
                        ));
                        (storage_capacity(storage, &Map::new()), false)
                    }
                }
            } else {
                (storage_capacity(storage, &Map::new()), false)
            };
            entries.push(LayoutCapacity {
                layout: layout.clone(),
                node: node.node.clone(),
                storage: storage.clone(),
                node_status: node.status.clone(),
                ok,
                name: capacity.name,
                total_bytes: capacity.total_bytes,
                used_bytes: capacity.used_bytes,
                available_bytes: capacity.available_bytes,
                percent: capacity.percent,
            });
        }
    }
    (entries, warnings)
}

/// `orphaned_homestack_volumes`: HomeStack volumes on the layouts' image
/// storages that no VM uses. Returns the volumes by `volid`, the scan
/// warnings and whether the scan covered every assigned storage.
pub async fn orphaned_volumes(
    pve: &impl Pve,
    cfg: &Config,
    attached: &BTreeSet<String>,
    nodes: &[NodeState],
) -> (Vec<OrphanVolume>, Vec<String>, bool) {
    let mut found: BTreeMap<String, OrphanVolume> = BTreeMap::new();
    let mut warnings = Vec::new();
    let mut complete = true;
    for node in nodes {
        let name = &node.node;
        let assigned: BTreeSet<&str> = storage_ids_for_node(cfg, name).iter().map(String::as_str).collect();
        if !node.online {
            if !assigned.is_empty() {
                complete = false;
            }
            continue;
        }
        let storages = match pve.get(&format!("/nodes/{name}/storage"), &[]).await {
            Ok(Value::Array(items)) => items,
            Ok(_) => {
                warnings.push(format!("Storage inventory on {name} did not return a JSON array"));
                continue;
            }
            Err(error) => {
                warnings.push(format!("Cannot inspect storages on {name}: {error}"));
                continue;
            }
        };
        if assigned.is_empty() {
            continue;
        }
        let mut image_storages: Vec<String> = Vec::new();
        for info in storages.iter().filter(|info| info.is_object()) {
            let storage = info
                .get("storage")
                .map(|v| if truthy(Some(v)) { py_str(v) } else { String::new() });
            let storage = storage.unwrap_or_default().trim().to_owned();
            let content: BTreeSet<&str> = info
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .collect();
            if storage.is_empty() || !assigned.contains(storage.as_str()) || !content.contains("images") {
                continue;
            }
            if integer_value(info.get("enabled")) == Some(0) || integer_value(info.get("active")) == Some(0) {
                continue;
            }
            image_storages.push(storage);
        }
        for storage in assigned
            .iter()
            .filter(|s| !image_storages.iter().any(|seen| seen == *s))
        {
            warnings.push(format!(
                "Assigned HomeStack storage '{storage}' is unavailable for image scanning on {name}"
            ));
        }
        for storage in &image_storages {
            let entries = match pve
                .get(
                    &format!("/nodes/{name}/storage/{storage}/content"),
                    &[("content", "images")],
                )
                .await
            {
                Ok(Value::Array(entries)) => entries,
                Ok(_) => {
                    warnings.push(format!(
                        "Storage content on {storage} ({name}) did not return a JSON array"
                    ));
                    continue;
                }
                Err(error) => {
                    warnings.push(format!("Cannot inspect image volumes on {storage} ({name}): {error}"));
                    continue;
                }
            };
            for entry in entries.iter().filter(|e| e.is_object()) {
                let volid = entry
                    .get("volid")
                    .filter(|v| truthy(Some(v)))
                    .map(py_str)
                    .unwrap_or_default();
                let volid = volid.trim();
                let Some(identity) = volume_identity(volid) else {
                    continue;
                };
                if attached.contains(volid) {
                    continue;
                }
                found.entry(volid.to_owned()).or_insert(OrphanVolume {
                    volid: identity.volid,
                    vmid: identity.vmid,
                    role: identity.role,
                    name: identity.name,
                    size_bytes: u64_value(entry.get("size")),
                    storage: storage.clone(),
                    node: name.clone(),
                });
            }
        }
    }
    let complete = complete && warnings.is_empty();
    (found.into_values().collect(), warnings, complete)
}

/// `guest.workspace_home_usage`: the size from the VM config,
/// and usage from `get-fsinfo` for the mounted filesystem labeled
/// `HS_HOME_<vmid>`, regardless of its device type, slot or mountpoint.
pub async fn workspace_home_usage(pve: &impl Pve, node: &str, vmid: u32, status: &str, home_cfg: &str) -> HomeUsage {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "Python's int(size_gb * 1024**3)"
    )]
    let size_bytes = parse_disk_size_gb(home_cfg).map(|gb| (gb * 1024.0_f64.powi(3)) as u64);
    let mut usage = HomeUsage {
        size_bytes,
        used_bytes: None,
        free_bytes: None,
        free_percent: None,
    };
    if status != "running" {
        return usage;
    }
    let Some(fs) = home_filesystem(pve, node, vmid).await else {
        return usage;
    };
    let (Some(used), Some(total)) = (fs.used, fs.total) else {
        return usage;
    };
    let free = total.saturating_sub(used);
    let size = fs.total_privileged.unwrap_or(total);
    #[allow(clippy::cast_precision_loss, reason = "df's percentage is a small integer")]
    let free_percent = (100.0 - df_percent(used, free) as f64).max(0.0);
    usage.size_bytes = Some(size);
    usage.used_bytes = Some(used);
    usage.free_bytes = Some(free);
    usage.free_percent = Some(free_percent);
    usage
}

/// The size of a disk option (`size=32G`), when it has one.
fn sized(option: &str) -> HomeUsage {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a disk size in whole bytes"
    )]
    let size_bytes = parse_disk_size_gb(option).map(|gb| (gb * 1024.0_f64.powi(3)) as u64);
    HomeUsage {
        size_bytes,
        used_bytes: None,
        free_bytes: None,
        free_percent: None,
    }
}

/// `used` of `total` bytes with what is free.
fn measured(used: u64, total: u64) -> HomeUsage {
    let free = total.saturating_sub(used);
    #[allow(clippy::cast_precision_loss, reason = "df's percentage is a small integer")]
    let free_percent = (100.0 - df_percent(used, free) as f64).max(0.0);
    HomeUsage {
        size_bytes: Some(total),
        used_bytes: Some(used),
        free_bytes: Some(free),
        free_percent: Some(free_percent),
    }
}

/// A workspace's root disk: the size from the VM config, and usage of `/`
/// from `get-fsinfo` when the VM runs and the agent answers.
async fn workspace_root_usage(pve: &impl Pve, node: &str, vmid: u32, status: &str, root_cfg: &str) -> HomeUsage {
    let usage = sized(root_cfg);
    if status != "running" {
        return usage;
    }
    let Ok(info) = pve
        .get(&format!("/nodes/{node}/qemu/{vmid}/agent/get-fsinfo"), &[])
        .await
    else {
        return usage;
    };
    let root = info.get("result").and_then(Value::as_array).and_then(|filesystems| {
        filesystems
            .iter()
            .find(|fs| fs.get("mountpoint").and_then(Value::as_str) == Some("/"))
    });
    match root.map(|fs| (u64_value(fs.get("used-bytes")), u64_value(fs.get("total-bytes")))) {
        Some((Some(used), Some(total))) => measured(used, total),
        _ => usage,
    }
}

/// `df`'s use percentage: used / (used + avail), rounded up.
#[must_use]
pub fn df_percent(used: u64, avail: u64) -> u64 {
    let total = u128::from(used) + u128::from(avail);
    if total == 0 {
        return 0;
    }
    u64::try_from((u128::from(used) * 100).div_ceil(total)).unwrap_or(100)
}

/// The mounted filesystem with the workspace's persistent home label.
struct HomeFilesystem {
    mount: String,
    used: Option<u64>,
    /// Used plus what unprivileged users may still use.
    total: Option<u64>,
    /// The whole filesystem (qemu-ga 8.0 and later).
    total_privileged: Option<u64>,
}

/// The guest device whose ext4 label can identify a home filesystem.
fn home_label_device(fs: &Value) -> Option<String> {
    if fs.get("type").and_then(Value::as_str) != Some("ext4") {
        return None;
    }
    let name = fs.get("name")?.as_str()?;
    Some(if name.starts_with("/dev/") {
        name.to_owned()
    } else {
        format!("/dev/{name}")
    })
}

async fn home_filesystem(pve: &impl Pve, node: &str, vmid: u32) -> Option<HomeFilesystem> {
    let info = pve
        .get(&format!("/nodes/{node}/qemu/{vmid}/agent/get-fsinfo"), &[])
        .await
        .ok()?;
    let expected = home_label(vmid).ok()?;
    let mut matched = None;
    let mut labels = BTreeMap::new();
    for fs in info.get("result")?.as_array()? {
        let Some(device) = home_label_device(fs) else {
            continue;
        };
        // A device may be mounted more than once. Read its identity once, but
        // retain every matching mount below so ambiguity remains visible.
        let label = if let Some(label) = labels.get(&device) {
            label
        } else {
            let label = crate::guest::ext4_label(pve, node, vmid, &device).await.ok().flatten();
            labels.entry(device.clone()).or_insert(label)
        };
        if label.as_deref() == Some(expected.as_str()) {
            if matched.is_some() {
                return None;
            }
            matched = Some((fs, device));
        }
    }
    let (fs, device) = matched?;
    let mountpoint = fs.get("mountpoint")?.as_str()?;
    let fstype = fs.get("type").map(py_str).unwrap_or_default();
    Some(HomeFilesystem {
        mount: format!("{device} {fstype} {mountpoint}"),
        used: u64_value(fs.get("used-bytes")),
        total: u64_value(fs.get("total-bytes")),
        total_privileged: u64_value(fs.get("total-bytes-privileged")),
    })
}

/// Configuration of every managed VM on an online node (empty otherwise),
/// in VMID order.
async fn managed_configs(
    pve: &impl Pve,
    cfg: &Config,
    managed: &[&StatusResource],
    nodes: &BTreeMap<String, NodeState>,
) -> Result<BTreeMap<u32, VmConfig>, CoreError> {
    let mut configs = BTreeMap::new();
    for item in managed {
        let node = if item.node.is_empty() {
            cfg.node.as_str()
        } else {
            item.node.as_str()
        };
        let online = nodes.get(node).is_some_and(|n| n.online);
        let config = if online {
            vm_config(pve, node, item.vmid).await?
        } else {
            VmConfig::new()
        };
        configs.insert(item.vmid, config);
    }
    Ok(configs)
}

/// Cached managed VM configurations and cluster resources. Orphan scanning
/// verifies the remaining visible VMs independently of workspace selection.
pub struct Inventory {
    pub resources: Vec<StatusResource>,
    pub nodes: Vec<NodeState>,
    pub configs: BTreeMap<u32, VmConfig>,
}

impl Inventory {
    pub async fn read(pve: &impl Pve, cfg: &Config) -> Result<Self, CoreError> {
        let resources = cluster_vm_resources(pve).await?;
        let nodes = cluster_node_statuses(pve).await?;
        let by_name: BTreeMap<String, NodeState> = nodes.iter().map(|n| (n.node.clone(), n.clone())).collect();
        let managed = status_resources(&resources, cfg);
        let configs = managed_configs(pve, cfg, &managed, &by_name).await?;
        Ok(Self {
            resources,
            nodes,
            configs,
        })
    }

    /// Check disk references of every visible VM, including additional Golds
    /// and VMs in other HomeStack pools, before classifying orphan volumes.
    pub async fn scan_orphans(&self, pve: &impl Pve, cfg: &Config) -> (Vec<OrphanVolume>, Vec<String>, bool) {
        let mut referenced: BTreeSet<String> = self
            .configs
            .values()
            .flat_map(|config| vm_volume_inventory(config, cfg).into_iter().map(|entry| entry.volume))
            .collect();
        let mut warnings = Vec::new();
        let mut offline = BTreeSet::new();
        for resource in &self.resources {
            let online = self.nodes.iter().any(|node| node.node == resource.node && node.online);
            if !online {
                offline.insert(resource.vmid);
                continue;
            }
            if self.configs.contains_key(&resource.vmid) {
                continue;
            }
            match vm_config(pve, &resource.node, resource.vmid).await {
                Ok(config) => {
                    referenced.extend(vm_volume_inventory(&config, cfg).into_iter().map(|entry| entry.volume))
                }
                Err(error) => warnings.push(format!(
                    "Cannot verify volume references of VM {} on {}: {error}",
                    resource.vmid, resource.node
                )),
            }
        }
        let references_complete = warnings.is_empty();
        let (mut volumes, storage_warnings, complete) = orphaned_volumes(pve, cfg, &referenced, &self.nodes).await;
        warnings.extend(storage_warnings);
        if !references_complete {
            // An unreadable VM may reference any candidate, regardless of the
            // VMID embedded in the volume name after a move or reattachment.
            volumes.clear();
        }
        volumes.retain(|volume| !offline.contains(&volume.vmid));
        (volumes, warnings, complete && references_complete && offline.is_empty())
    }
}

/// `global_status`, with Proxmox read `parallel` requests at a time.
pub async fn global_status(
    pve: &impl Pve,
    cfg: &Config,
    parallel: usize,
    observations: &crate::prefetch::Observations,
    prometheus: Option<&reqwest::Url>,
) -> Result<(GlobalStatus, Vec<crate::virtiofs::Target>), CoreError> {
    let pve = &crate::prefetch::Prefetch::new(pve, parallel, observations);
    let (prefetched, mut telemetry) = futures_util::join!(prefetch_status(pve, cfg), async {
        match prometheus {
            Some(url) => crate::service_status::read(url).await,
            None => BTreeMap::new(),
        }
    });
    prefetched?;
    let inventory = Inventory::read(pve, cfg).await?;
    let containers = cluster_container_resources(pve).await.unwrap_or_default();
    // The last space measured in the machines that are not running, and in
    // every VM, whose guest reads can fail between two status requests.
    let measured_before: Vec<u32> = inventory
        .resources
        .iter()
        .chain(
            containers
                .iter()
                .filter(|item| item.status.as_deref() != Some("running")),
        )
        .map(|item| item.vmid)
        .collect();
    let last = match prometheus {
        Some(url) => crate::service_status::last_space(url, &measured_before).await,
        None => BTreeMap::new(),
    };
    let last_root = |vmid: u32| last.get(&vmid).and_then(|mounts| mounts.get("/").cloned());
    let nodes: BTreeMap<String, NodeState> = inventory.nodes.iter().map(|n| (n.node.clone(), n.clone())).collect();
    let configs = &inventory.configs;
    let mut warnings = Vec::new();

    let gold = match cfg
        .gold_vmid
        .and_then(|vmid| inventory.resources.iter().find(|r| r.vmid == vmid))
    {
        None => {
            warnings.push(match cfg.gold_vmid {
                Some(vmid) => format!("Configured Gold VM {vmid} does not exist."),
                None => "Gold is not configured; creating and refreshing workspaces is unavailable.".to_owned(),
            });
            None
        }
        Some(resource) => {
            let gold_node = if resource.node.is_empty() {
                cfg.node.to_string()
            } else {
                resource.node.clone()
            };
            let empty = VmConfig::new();
            let gold_cfg = configs.get(&resource.vmid).unwrap_or(&empty);
            let tags = parse_tags(non_empty(gold_cfg.get("tags")).or(resource.tags.as_deref()));
            let role_tag_ok = tags.contains(GOLD_TAG) && !tags.contains(ALIEN_WORKSPACE_TAG);
            let warning = (!role_tag_ok).then(|| {
                if tags.contains(ALIEN_WORKSPACE_TAG) {
                    format!(
                        "Configured Gold VM {} has forbidden tag '{ALIEN_WORKSPACE_TAG}'.",
                        resource.vmid
                    )
                } else {
                    format!(
                        "Configured Gold VM {} is missing required tag '{GOLD_TAG}'.",
                        resource.vmid
                    )
                }
            });
            warnings.extend(warning.clone());
            Some(StatusEntry {
                vmid: resource.vmid,
                role: "GOLD".to_owned(),
                name: resource
                    .name
                    .clone()
                    .or_else(|| non_empty(gold_cfg.get("name")).map(str::to_owned)),
                status: resource.status.clone().unwrap_or_else(|| "unknown".to_owned()),
                node_status: nodes
                    .get(&gold_node)
                    .map_or_else(|| "unknown".to_owned(), |n| n.status.clone()),
                pool: resource.pool.clone(),
                tags: tag_list(non_empty(gold_cfg.get("tags")).or(resource.tags.as_deref())),
                node: gold_node,
                ip: expected_ip(cfg, resource.vmid),
                root_storage: disk_storage(gold_cfg.get(&cfg.root_disk).map(String::as_str)),
                root: gold_cfg
                    .get(&cfg.root_disk)
                    .map(|option| last_root(resource.vmid).unwrap_or_else(|| sized(option))),
                home: None,
                service_status: None,
                role_tag_ok,
                warning,
                image: None,
                image_update: None,
                service: None,
            })
        }
    };

    let mut workspaces = Vec::new();
    let mut virtiofs = Vec::new();
    let (mut total_home, mut known_used, mut known_free, mut known_count, mut missing) =
        (0_u64, 0_u64, 0_u64, 0_u64, 0_u64);
    let managed = status_resources(&inventory.resources, cfg);
    for item in managed.iter().filter(|item| Some(item.vmid) != cfg.gold_vmid) {
        let empty = VmConfig::new();
        let vm_cfg = configs.get(&item.vmid).unwrap_or(&empty);
        let node_info = nodes.get(&item.node);
        let node_online = node_info.is_some_and(|n| n.online);
        let tags = parse_tags(non_empty(vm_cfg.get("tags")).or(item.tags.as_deref()));
        let alien = tags.contains(ALIEN_WORKSPACE_TAG);
        let mut problems = Vec::new();
        if tags.contains(GOLD_TAG) {
            problems.push(format!("forbidden tag '{GOLD_TAG}'"));
        }
        if alien && tags.contains(WORKSPACE_TAG) {
            problems.push("conflicting workspace role tags".to_owned());
        }
        let home_cfg = if alien {
            ""
        } else {
            vm_cfg.get(&cfg.home_disk).map(String::as_str).unwrap_or_default()
        };
        let shared_home = crate::virtiofs::Target::of(item, vm_cfg);
        if node_online
            && item.status.as_deref() == Some("running")
            && let Some(target) = shared_home.clone()
        {
            virtiofs.push(target);
        }
        let mut home = None;
        // A running VM's virtiofs home is also measured in the background,
        // and that newer reading replaces this one when there is one.
        if shared_home.is_some() {
            home = last
                .get(&item.vmid)
                .and_then(|mounts| crate::service_status::last_home(mounts, false, item.vmid, None));
        }
        if node_online && shared_home.is_none() {
            let status = item.status.as_deref().unwrap_or("unknown");
            let mut usage = workspace_home_usage(pve, &item.node, item.vmid, status, home_cfg).await;
            if (status != "running" || usage.used_bytes.is_none())
                && let Some(previous) = last
                    .get(&item.vmid)
                    .and_then(|mounts| crate::service_status::last_home(mounts, false, item.vmid, usage.size_bytes))
            {
                usage = previous;
            }
            if let Some(size) = usage.size_bytes {
                total_home += size;
            } else {
                missing += 1;
                if status == "running" {
                    problems.push(format!(
                        "cannot read mounted home filesystem with label '{}'",
                        home_label(item.vmid)?
                    ));
                }
            }
            if let (Some(used), Some(free)) = (usage.used_bytes, usage.free_bytes) {
                known_count += 1;
                known_used += used;
                known_free += free;
            }
            home = Some(usage);
        }
        if pve.used_previous_guest_data(&format!("/nodes/{}/qemu/{}", item.node, item.vmid)) {
            problems.push("guest data is temporarily unavailable; showing the last successful observation".to_owned());
        }
        let warning = (!problems.is_empty()).then(|| problems.join("; "));
        if let Some(warning) = &warning {
            warnings.push(format!("Workspace VM {}: {warning}.", item.vmid));
        }
        workspaces.push(StatusEntry {
            vmid: item.vmid,
            role: if alien { "ALIEN" } else { "WS" }.to_owned(),
            name: item
                .name
                .clone()
                .or_else(|| non_empty(vm_cfg.get("name")).map(str::to_owned)),
            status: item.status.clone().unwrap_or_else(|| "unknown".to_owned()),
            node: item.node.clone(),
            node_status: node_info.map_or_else(|| "unknown".to_owned(), |n| n.status.clone()),
            pool: item.pool.clone(),
            tags: tag_list(non_empty(vm_cfg.get("tags")).or(item.tags.as_deref())),
            ip: expected_ip(cfg, item.vmid),
            root_storage: disk_storage(vm_cfg.get(&cfg.root_disk).map(String::as_str)),
            root: match vm_cfg.get(&cfg.root_disk) {
                Some(option) if item.status.as_deref() != Some("running") => {
                    Some(last_root(item.vmid).unwrap_or_else(|| sized(option)))
                }
                Some(option) if node_online => Some(
                    workspace_root_usage(
                        pve,
                        &item.node,
                        item.vmid,
                        item.status.as_deref().unwrap_or("unknown"),
                        option,
                    )
                    .await,
                ),
                Some(option) => Some(sized(option)),
                None => None,
            },
            home,
            image: None,
            service_status: if node_online && item.status.as_deref() == Some("running") {
                telemetry
                    .remove(&(item.node.clone(), item.vmid))
                    .map(|measured| ServiceStatus {
                        application: None,
                        container: Some(measured.telemetry),
                    })
            } else {
                None
            },
            image_update: None,
            service: None,
            role_tag_ok: !tags.contains(GOLD_TAG) && !(alien && tags.contains(WORKSPACE_TAG)),
            warning,
        });
    }

    // The other guests of the pools the token sees: more Golds, containers.
    let listed: BTreeSet<u32> = managed.iter().map(|item| item.vmid).chain(cfg.gold_vmid).collect();
    let guests = inventory
        .resources
        .iter()
        .map(|item| (item, false))
        .chain(containers.iter().map(|item| (item, true)));
    let mut others = Vec::new();
    let mut applications = Vec::new();
    for (item, container) in guests {
        if item.pool.is_none() || listed.contains(&item.vmid) {
            continue;
        }
        let node_info = nodes.get(&item.node);
        let online = node_info.is_some_and(|n| n.online);
        let container_config = if online && container {
            pve.get(&format!("/nodes/{}/lxc/{}/config", item.node, item.vmid), &[])
                .await
                .ok()
        } else {
            None
        };
        let image = container_config.as_ref().and_then(crate::registry::deployed_image);
        let service = container_config.as_ref().and_then(crate::registry::deployed_service);
        let (root_storage, mut root) = if !online {
            (None, None)
        } else if container {
            let rootfs = container_config
                .as_ref()
                .and_then(|config| config.get("rootfs").and_then(Value::as_str));
            // PVE's LXC disk/maxdisk counters describe the root filesystem.
            let root = match (item.disk_bytes, item.maxdisk_bytes) {
                (Some(used), Some(total)) if total > 0 && item.status.as_deref() == Some("running") => {
                    Some(measured(used, total))
                }
                _ => rootfs.map(|rootfs| last_root(item.vmid).unwrap_or_else(|| sized(rootfs))),
            };
            (disk_storage(rootfs), root)
        } else {
            let config = vm_config(pve, &item.node, item.vmid).await.ok();
            let option = config.as_ref().and_then(|config| config.get(&cfg.root_disk));
            (
                disk_storage(option.map(String::as_str)),
                option.map(|option| {
                    if item.status.as_deref() == Some("running") {
                        sized(option)
                    } else {
                        last_root(item.vmid).unwrap_or_else(|| sized(option))
                    }
                }),
            )
        };
        let role = if container {
            "CT"
        } else if parse_tags(item.tags.as_deref()).contains(GOLD_TAG) {
            "GOLD"
        } else {
            "VM"
        };
        let mut home = container_config.as_ref().and_then(|config| {
            let mount = config["mp0"].as_str()?;
            if !mount.split(',').any(|part| part == "mp=/home/app") {
                return None;
            }
            Some(HomeUsage {
                size_bytes: parse_disk_size_gb(mount).map(|gib| (gib * 1024.0_f64.powi(3)) as u64),
                // PVE's LXC disk/maxdisk counters describe rootfs, not mp0.
                used_bytes: None,
                free_bytes: None,
                free_percent: None,
            })
        });
        let ip = if container {
            container_config.as_ref().and_then(|config| {
                config["net0"].as_str()?.split(',').find_map(|part| {
                    let value = part.strip_prefix("ip=")?.split('/').next()?;
                    value.parse::<std::net::Ipv4Addr>().ok().map(|ip| ip.to_string())
                })
            })
        } else {
            expected_ip(cfg, item.vmid)
        };
        let running = online && item.status.as_deref() == Some("running");
        if !running
            && let (Some(home), Some(previous)) = (
                &mut home,
                last.get(&item.vmid)
                    .and_then(|mounts| crate::service_status::last_home(mounts, container, item.vmid, None)),
            )
        {
            *home = previous;
        }
        let measured = if running {
            telemetry.remove(&(item.node.clone(), item.vmid))
        } else {
            None
        };
        if let Some(measured) = &measured {
            if let (Some(home), Some(space)) = (&mut home, &measured.home) {
                *home = space.clone();
            }
            if measured.root.is_some() {
                root.clone_from(&measured.root);
            }
        }
        if container
            && running
            && let (Some(config), Some(ip)) = (container_config.clone(), ip.clone())
        {
            let index = others.len();
            applications.push(async move { (index, crate::service_status::application(&config, &ip).await) });
        }
        let service_status = measured.map(|measured| ServiceStatus {
            application: None,
            container: Some(measured.telemetry),
        });
        others.push(StatusEntry {
            vmid: item.vmid,
            role: role.to_owned(),
            name: item.name.clone(),
            status: item.status.clone().unwrap_or_else(|| "unknown".to_owned()),
            node: item.node.clone(),
            node_status: node_info.map_or_else(|| "unknown".to_owned(), |n| n.status.clone()),
            pool: item.pool.clone(),
            tags: tag_list(item.tags.as_deref()),
            ip,
            root_storage,
            root,
            home,
            service_status,
            role_tag_ok: true,
            warning: None,
            image,
            image_update: None,
            service,
        });
    }
    for (index, application) in futures_util::future::join_all(applications).await {
        if let Some(application) = application {
            others[index]
                .service_status
                .get_or_insert(ServiceStatus {
                    application: None,
                    container: None,
                })
                .application = Some(application);
        }
    }
    others.sort_by_key(|entry| entry.vmid);
    let pools = crate::inventory::pools(pve).await;

    let (orphans, orphan_warnings, orphan_complete) = inventory.scan_orphans(pve, cfg).await;
    if !orphans.is_empty() {
        let list: Vec<&str> = orphans.iter().map(|o| o.volid.as_str()).collect();
        warnings.push(format!("Detached HomeStack volumes: {}", list.join(", ")));
    }
    warnings.extend(
        orphan_warnings
            .iter()
            .map(|w| format!("HomeStack orphan scan incomplete: {w}")),
    );

    let (layouts, storage_warnings) = storage_capacities(pve, cfg, &inventory.nodes).await;
    warnings.extend(storage_warnings);
    let root_storage = layouts
        .iter()
        .find(|e| e.node == cfg.node.as_str() && e.storage == cfg.default_storage())
        .map_or_else(
            || storage_capacity(cfg.default_storage(), &Map::new()),
            |e| Capacity {
                name: e.name.clone(),
                total_bytes: e.total_bytes,
                used_bytes: e.used_bytes,
                available_bytes: e.available_bytes,
                percent: e.percent,
            },
        );

    let managed_homes = workspaces.len() as u64;
    let all_known = known_count == managed_homes && managed_homes > 0;
    #[allow(
        clippy::cast_precision_loss,
        reason = "Python computes the percentage in floating point too"
    )]
    let free_percent = (all_known && total_home != 0).then(|| 100.0 * known_free as f64 / total_home as f64);
    let running = workspaces.iter().filter(|w| w.status == "running").count() as u64;
    let stopped = workspaces.iter().filter(|w| w.status == "stopped").count() as u64;
    Ok((
        GlobalStatus {
            gold,
            summary: StatusSummary {
                workspace_count: workspaces.len() as u64,
                running,
                stopped,
                root_storage,
                storage_layouts: layouts,
                nodes: inventory.nodes.clone(),
                homes: HomesSummary {
                    used_bytes: (known_count > 0).then_some(known_used),
                    quota_bytes: total_home,
                    free_bytes: all_known.then_some(known_free),
                    free_percent,
                    physical_available_bytes: None,
                    missing_count: missing,
                },
                orphaned_volumes: orphans,
                orphan_scan_complete: orphan_complete,
                quorum: cluster_quorum(pve).await,
            },
            workspaces,
            others,
            pools,
            warnings,
        },
        virtiofs,
    ))
}

/// Everything [`global_status`] reads after the cluster's guests and nodes:
/// their configurations, the running workspaces' filesystems, the storages,
/// the pools and the quorum.
async fn prefetch_status<P: Pve>(pve: &crate::prefetch::Prefetch<'_, P>, cfg: &Config) -> Result<(), CoreError> {
    let resources = cluster_vm_resources(pve).await?;
    let nodes = cluster_node_statuses(pve).await?;
    let containers = cluster_container_resources(pve).await.unwrap_or_default();
    let online = |node: &str| nodes.iter().any(|state| state.node == node && state.online);
    pve.retain_guests(
        &resources
            .iter()
            .filter(|item| online(&item.node) && item.status.as_deref() == Some("running"))
            .map(|item| format!("/nodes/{}/qemu/{}", item.node, item.vmid))
            .collect(),
    );
    let mut requests: Vec<(String, Vec<(&str, &str)>)> =
        vec![("/pools".to_owned(), vec![]), ("/cluster/status".to_owned(), vec![])];
    for item in resources.iter().filter(|item| online(&item.node)) {
        requests.push((format!("/nodes/{}/qemu/{}/config", item.node, item.vmid), vec![]));
    }
    for item in status_resources(&resources, cfg)
        .into_iter()
        .filter(|item| online(&item.node) && item.status.as_deref() == Some("running"))
    {
        requests.push((
            format!("/nodes/{}/qemu/{}/agent/get-fsinfo", item.node, item.vmid),
            vec![],
        ));
    }
    for item in containers.iter().filter(|item| online(&item.node)) {
        requests.push((format!("/nodes/{}/lxc/{}/config", item.node, item.vmid), vec![]));
    }
    for node in nodes.iter().filter(|node| node.online) {
        requests.push((format!("/nodes/{}/storage", node.node), vec![]));
        for storage in storage_ids_for_node(cfg, &node.node) {
            requests.push((format!("/nodes/{}/storage/{storage}/status", node.node), vec![]));
            requests.push((
                format!("/nodes/{}/storage/{storage}/content", node.node),
                vec![("content", "images")],
            ));
        }
    }
    pve.warm(requests).await;
    // Labels require guest file reads. Fetch them with the same concurrency
    // and deadline as other agent data instead of waiting on each disk in turn.
    let mut devices = Vec::new();
    for item in status_resources(&resources, cfg).into_iter().filter(|item| {
        Some(item.vmid) != cfg.gold_vmid && online(&item.node) && item.status.as_deref() == Some("running")
    }) {
        if let Ok(config) = vm_config(pve, &item.node, item.vmid).await
            && crate::virtiofs::Target::of(item, &config).is_some()
        {
            continue;
        }
        let Ok(info) = pve
            .get(
                &format!("/nodes/{}/qemu/{}/agent/get-fsinfo", item.node, item.vmid),
                &[],
            )
            .await
        else {
            continue;
        };
        for fs in info.get("result").and_then(Value::as_array).into_iter().flatten() {
            if let Some(device) = home_label_device(fs) {
                devices.push((
                    format!("/nodes/{}/qemu/{}/agent/file-read", item.node, item.vmid),
                    device,
                ));
            }
        }
    }
    pve.warm(
        devices
            .iter()
            .map(|(path, device)| {
                (
                    path.clone(),
                    vec![
                        ("file", device.as_str()),
                        ("offset", "1024"),
                        ("count", "256"),
                        ("decode", "0"),
                    ],
                )
            })
            .collect(),
    )
    .await;
    Ok(())
}

fn non_empty(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|v| !v.is_empty())
}

/// `lifecycle.resolve_workspace_target`: a VMID, or the name of exactly one
/// VM tagged `homestack-ws` or `homestack-alien`.
pub async fn resolve_workspace_target(pve: &impl Pve, _cfg: &Config, target: &str) -> Result<u32, CoreError> {
    let text = target.trim();
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
        return text
            .parse()
            .map_err(|_| CoreError::Invalid(format!("Invalid workspace target '{text}'.")));
    }
    let valid_name = text.len() <= 63
        && text.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if !valid_name {
        return Err(CoreError::Invalid(format!(
            "Invalid workspace target '{text}'. Use a numeric VMID or a workspace name whose first character is a letter."
        )));
    }
    let mut matches = Vec::new();
    let mut other = Vec::new();
    for (vmid, item) in cluster_vm_maps(pve).await? {
        if item.get("name").map(py_str).as_deref() != Some(text) {
            continue;
        }
        let node = item
            .get("node")
            .filter(|n| truthy(Some(n)))
            .map(py_str)
            .unwrap_or_default();
        if node.is_empty() {
            continue;
        }
        let vm_cfg = vm_config(pve, &node, vmid).await?;
        let tags = vm_cfg.get("tags").map(String::as_str);
        if has_tag(tags, WORKSPACE_TAG) || has_tag(tags, ALIEN_WORKSPACE_TAG) {
            matches.push(vmid);
        } else {
            other.push(vmid);
        }
    }
    match (matches.as_slice(), other.first()) {
        ([vmid], _) => Ok(*vmid),
        ([], Some(vmid)) => Err(CoreError::Refused(format!(
            "VM name '{text}' exists but is not a HomeStack workspace (VMID {vmid})."
        ))),
        ([], None) => Err(CoreError::NotFound(format!(
            "No HomeStack workspace named '{text}' exists."
        ))),
        (many, _) => {
            let mut sorted = many.to_vec();
            sorted.sort_unstable();
            let list: Vec<String> = sorted.iter().map(ToString::to_string).collect();
            Err(CoreError::Ambiguous(format!(
                "Workspace name '{text}' is ambiguous; matching VMIDs: {}",
                list.join(", ")
            )))
        }
    }
}

/// `workspace_status`: one VM's role, disks and guest view. `None` when the
/// VM does not exist.
pub async fn workspace_status(pve: &impl Pve, cfg: &Config, vmid: u32) -> Result<Option<WorkspaceStatus>, CoreError> {
    let Some(resource) = cluster_vm_resources(pve).await?.into_iter().find(|r| r.vmid == vmid) else {
        return Ok(None);
    };
    workspace_status_of(pve, cfg, resource)
        .await
        .map(|(status, _)| Some(status))
}

/// [`workspace_status`] for a resource already read from
/// `cluster/resources`.
pub async fn workspace_status_of(
    pve: &impl Pve,
    cfg: &Config,
    resource: StatusResource,
) -> Result<(WorkspaceStatus, Option<crate::virtiofs::Target>), CoreError> {
    let vmid = resource.vmid;
    let node = resource.node.clone();
    let vm_cfg = vm_config(pve, &node, vmid).await?;
    let name = vm_cfg.get("name").cloned().unwrap_or_default();
    let tags = parse_tags(vm_cfg.get("tags").map(String::as_str));
    let (role, role_tag_ok, role_warning) = if Some(vmid) == cfg.gold_vmid {
        let ok = tags.contains(GOLD_TAG) && !tags.contains(ALIEN_WORKSPACE_TAG);
        let warning = (!ok).then(|| {
            if tags.contains(ALIEN_WORKSPACE_TAG) {
                format!("Configured Gold VM has forbidden tag '{ALIEN_WORKSPACE_TAG}'.")
            } else {
                format!("Configured Gold VM is missing required tag '{GOLD_TAG}'.")
            }
        });
        (Some("GOLD"), ok, warning)
    } else if tags.contains(ALIEN_WORKSPACE_TAG) {
        let ok = !tags.contains(GOLD_TAG) && !tags.contains(WORKSPACE_TAG);
        (
            Some("ALIEN"),
            ok,
            (!ok).then(|| "Alien workspace has conflicting role tags.".to_owned()),
        )
    } else if tags.contains(WORKSPACE_TAG) {
        let ok = !tags.contains(GOLD_TAG);
        (
            Some("WS"),
            ok,
            (!ok).then(|| format!("Workspace also has forbidden Gold tag '{GOLD_TAG}'.")),
        )
    } else {
        (
            None,
            false,
            Some(format!("Required workspace tag '{WORKSPACE_TAG}' is missing.")),
        )
    };

    let ip = expected_ip(cfg, vmid);
    let status = match &resource.status {
        Some(status) => status.clone(),
        None => vm_status(pve, &node, vmid).await,
    };
    let alien = role == Some("ALIEN");
    let home_cfg = if alien {
        ""
    } else {
        vm_cfg.get(&cfg.home_disk).map(String::as_str).unwrap_or_default()
    };
    let expected_label = if matches!(role, Some("WS" | "ALIEN")) {
        Some(home_label(vmid)?)
    } else {
        None
    };
    let shared_home = expected_label
        .as_ref()
        .and_then(|_| crate::virtiofs::Target::of(&resource, &vm_cfg));
    let home_serial = (!home_cfg.is_empty())
        .then(|| disk_option(home_cfg, "serial"))
        .flatten();
    let home_storage = (!home_cfg.is_empty()).then(|| disk_storage(Some(home_cfg))).flatten();
    let home_size_gb = (!home_cfg.is_empty()).then(|| parse_disk_size_gb(home_cfg)).flatten();

    let mut qga = false;
    let mut actual_hostname = None;
    let mut actual_ip = None;
    let mut home = None;
    if status == "running" {
        qga = pve
            .post(&format!("/nodes/{node}/qemu/{vmid}/agent/ping"), &[])
            .await
            .is_ok();
        if qga {
            actual_hostname = pve
                .get(&format!("/nodes/{node}/qemu/{vmid}/agent/get-host-name"), &[])
                .await
                .ok()
                .and_then(|info| {
                    info.pointer("/result/host-name")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .filter(|name| !name.trim().is_empty());
            actual_ip = pve
                .get(&format!("/nodes/{node}/qemu/{vmid}/agent/network-get-interfaces"), &[])
                .await
                .ok()
                .and_then(|info| first_ipv4(&info, vm_cfg.get("net0")?));
            if shared_home.is_none() {
                home = home_filesystem(pve, &node, vmid).await;
            }
        }
    }
    let home_mount = home.as_ref().map(|fs| fs.mount.clone());
    // A virtiofs home has no labelled filesystem to look for: unknown here,
    // the server's measurement of the share decides.
    let home_mount_ok = (expected_label.is_some() && qga && shared_home.is_none()).then(|| home.is_some());
    let home_identity_ok = home_mount_ok;
    #[allow(clippy::cast_precision_loss, reason = "Disk sizes are displayed in GiB")]
    let home_size_gb = home
        .as_ref()
        .and_then(|fs| fs.total_privileged.or(fs.total))
        .map(|bytes| bytes as f64 / 1024.0_f64.powi(3))
        .or(home_size_gb);
    let target = shared_home.filter(|_| status == "running");
    Ok((
        WorkspaceStatus {
            vmid,
            name: (!name.is_empty()).then_some(name),
            role: role.map(str::to_owned),
            tags: tags.into_iter().collect(),
            role_tag_ok,
            role_warning,
            status,
            node,
            configured_ip: ip.clone(),
            actual_hostname,
            actual_ip,
            qga,
            root_storage: disk_storage(vm_cfg.get(&cfg.root_disk).map(String::as_str)),
            volumes: vm_volume_inventory(&vm_cfg, cfg),
            home_disk: (!home_cfg.is_empty()).then(|| cfg.home_disk.clone()),
            home_storage,
            home_size_gb,
            home_label: expected_label,
            home_serial,
            home_identity_ok,
            home_mount,
            home_mount_ok,
            ssh: SshCommands {
                root: ip.as_ref().map(|ip| format!("ssh root@{ip}")),
                user: ip.as_ref().map(|ip| format!("ssh {}@{ip}", cfg.user.name)),
            },
        },
        target,
    ))
}

/// `address/prefix` of the primary virtual NIC, matched by its MAC address.
fn first_ipv4(info: &Value, net0: &str) -> Option<String> {
    let mac = crate::gold::extract_mac(net0)?;
    info.get("result")?
        .as_array()?
        .iter()
        .find(|interface| {
            interface
                .get("hardware-address")
                .and_then(Value::as_str)
                .is_some_and(|address| address.eq_ignore_ascii_case(&mac))
        })?
        .get("ip-addresses")?
        .as_array()?
        .iter()
        .find(|a| a.get("ip-address-type").and_then(Value::as_str) == Some("ipv4"))
        .and_then(|a| Some(format!("{}/{}", a.get("ip-address")?.as_str()?, a.get("prefix")?)))
}

/// What `resolve_existing_workspace` verified about a workspace before a
/// lifecycle operation.
#[derive(Debug, Clone)]
pub struct ExistingWorkspace {
    pub vmid: u32,
    pub name: String,
    pub node: String,
    pub status: String,
    pub vm_config: VmConfig,
    pub root_volume: String,
    pub root_storage: Option<String>,
    pub home_volume: String,
    pub home_storage: Option<String>,
    pub home_label: String,
    pub home_usage: HomeUsage,
}

/// `resolve_existing_workspace` (without the network part, which arrives
/// with the operations that need it).
pub async fn resolve_existing_workspace(
    pve: &impl Pve,
    cfg: &Config,
    vmid: u32,
) -> Result<ExistingWorkspace, CoreError> {
    if Some(vmid) == cfg.gold_vmid {
        return Err(CoreError::Refused(format!(
            "Refusing lifecycle operation on Gold VM {vmid}"
        )));
    }
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
    let status = match resource.status {
        Some(status) => status,
        None => vm_status(pve, &node, vmid).await,
    };
    let vm_cfg = vm_config(pve, &node, vmid).await?;
    let tags = vm_cfg.get("tags").map(String::as_str);
    if has_tag(tags, ALIEN_WORKSPACE_TAG) {
        return Err(CoreError::Refused(format!(
            "VM {vmid} has forbidden workspace role tag '{ALIEN_WORKSPACE_TAG}'; lifecycle operations require native '{WORKSPACE_TAG}'."
        )));
    }
    if !has_tag(tags, WORKSPACE_TAG) {
        return Err(CoreError::Refused(format!(
            "VM {vmid} is not a HomeStack workspace: required tag '{WORKSPACE_TAG}' is missing."
        )));
    }
    let name = vm_cfg.get("name").map(|n| n.trim().to_owned()).unwrap_or_default();
    if name.is_empty() {
        return Err(CoreError::Unusable(format!("VM {vmid} has no name")));
    }
    let valid_name = name.len() <= 63
        && name.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if !valid_name {
        return Err(CoreError::Unusable(
            "Invalid VM name. Use 1-63 characters: letters, digits, dot and hyphen; the first character must be a letter."
                .to_owned(),
        ));
    }
    let root_cfg = vm_cfg.get(&cfg.root_disk).cloned().unwrap_or_default();
    if root_cfg.is_empty() {
        return Err(CoreError::Unusable(format!(
            "VM {vmid} has no disposable root disk {}",
            cfg.root_disk
        )));
    }
    let home_cfg = vm_cfg.get(&cfg.home_disk).cloned().unwrap_or_default();
    if home_cfg.is_empty() {
        return Err(CoreError::Unusable(format!(
            "VM {vmid} has no persistent home disk {}",
            cfg.home_disk
        )));
    }
    let expected = home_label(vmid)?;
    let root_volume = disk_volume(&root_cfg).to_owned();
    let home_volume = disk_volume(&home_cfg).to_owned();
    if root_volume == home_volume {
        return Err(CoreError::Unusable(format!(
            "VM {vmid} root disk {} and persistent home {} reference the same volume '{root_volume}'",
            cfg.root_disk, cfg.home_disk
        )));
    }
    let serial = disk_option(&home_cfg, "serial");
    if serial.as_deref() != Some(expected.as_str()) {
        return Err(CoreError::Unusable(format!(
            "VM {vmid} {} serial is {}, expected '{expected}'",
            cfg.home_disk,
            py_repr(serial.as_deref())
        )));
    }
    let home_usage = workspace_home_usage(pve, &node, vmid, &status, &home_cfg).await;
    Ok(ExistingWorkspace {
        vmid,
        name,
        status,
        root_storage: disk_storage(Some(&root_cfg)),
        home_storage: disk_storage(Some(&home_cfg)),
        node,
        vm_config: vm_cfg,
        root_volume,
        home_volume,
        home_label: expected,
        home_usage,
    })
}
