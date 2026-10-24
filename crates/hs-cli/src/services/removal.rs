//! Authorization for service uninstall, kept separately from maintenance reads
//! that can resolve an interrupted replacement through its holder.

use std::collections::BTreeMap;

use hs_core::pve::Pve;
use hs_deploy::recreate;
use serde_json::Value;

use super::instance::{self, Instance};
use super::manifest::MOUNT;

/// Exact configuration (including PVE's digest), storage
/// configuration and volume attributes reviewed for irreversible deletion.
#[derive(Debug, Clone)]
pub(super) struct Reviewed {
    pub target: Instance,
    config: Value,
    storage: Value,
    volumes: BTreeMap<String, Value>,
}

async fn get(pve: &impl Pve, path: &str, query: &[(&str, &str)]) -> Result<Value, String> {
    pve.get(path, query).await.map_err(|e| format!("{path}: {e}"))
}

fn array<'a>(value: &'a Value, what: &str) -> Result<&'a [Value], String> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| format!("{what}: incomplete inventory"))
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("Missing or invalid {key}"))
}

/// Cluster location, explicit pool membership and the node's own inventory
/// must agree. Never use a holder as the uninstall target.
async fn placement(pve: &impl Pve, ctid: u32, pool: &str) -> Result<String, String> {
    if pool.is_empty() || ctid == recreate::HOLDER_ID {
        return Err("No authorized service pool or target is a replacement holder".to_owned());
    }
    let resources = get(pve, "/cluster/resources", &[("type", "vm")]).await?;
    let resources = array(&resources, "Cluster guests")?;
    if resources
        .iter()
        .any(|r| r["vmid"].as_u64().is_none() || r["type"].as_str().is_none())
    {
        return Err("Incomplete cluster guest inventory".to_owned());
    }
    // The holder can carry a stateless replacement too. Its absence is required
    // even when the target still has mp0; do not mask an unreadable holder.
    if resources
        .iter()
        .any(|r| r["vmid"].as_u64() == Some(u64::from(recreate::HOLDER_ID)))
    {
        return Err("Replacement holder present; finish replacement before removal".to_owned());
    }
    let matches: Vec<_> = resources
        .iter()
        .filter(|r| r["vmid"].as_u64() == Some(u64::from(ctid)))
        .collect();
    let [found] = matches.as_slice() else {
        return Err("Container location is missing or ambiguous".to_owned());
    };
    if found["type"] != "lxc" || found["pool"].as_str() != Some(pool) {
        return Err(format!("Container is not an LXC in authorized pool {pool}"));
    }
    let node = text(found, "node")?;
    let membership = get(pve, &format!("/pools/{pool}"), &[]).await?;
    let members = array(&membership["members"], "Pool members")?;
    if members.iter().any(|r| r["type"].as_str().is_none()) {
        return Err("Incomplete pool membership inventory".to_owned());
    }
    let matches: Vec<_> = members
        .iter()
        .filter(|r| r["vmid"].as_u64() == Some(u64::from(ctid)))
        .collect();
    if matches.len() != 1 || matches[0]["type"] != "lxc" || matches[0]["node"].as_str() != Some(node) {
        return Err("Pool membership and cluster location disagree".to_owned());
    }
    let local = get(pve, &format!("/nodes/{node}/lxc"), &[]).await?;
    let local = array(&local, "Node containers")?;
    if local.iter().any(|r| r["vmid"].as_u64().is_none()) {
        return Err("Incomplete node container inventory".to_owned());
    }
    if local
        .iter()
        .any(|r| r["vmid"].as_u64() == Some(u64::from(recreate::HOLDER_ID)))
    {
        return Err("Replacement holder present on node; finish replacement before removal".to_owned());
    }
    if local
        .iter()
        .filter(|r| r["vmid"].as_u64() == Some(u64::from(ctid)))
        .count()
        != 1
    {
        return Err("Container missing or ambiguous in node inventory".to_owned());
    }
    Ok(node.to_owned())
}

fn safe_config(cfg: &Value) -> Result<(), String> {
    let fields = cfg.as_object().ok_or("Incomplete container configuration")?;
    text(cfg, "digest")?;
    text(cfg, "hostname")?;
    if cfg.get("lock").is_some()
        || cfg.get("protection").is_some_and(|v| v.as_u64() != Some(0))
        || cfg.get("template").is_some_and(|v| v.as_u64() != Some(0))
        || cfg["unprivileged"].as_u64() != Some(1)
    {
        return Err("Locked, protected, template or privileged container requires manual review".to_owned());
    }
    for key in fields.keys() {
        if (key.starts_with("mp") && key != "mp0")
            || (key.starts_with("net") && key != "net0")
            || key.starts_with("unused")
            || key.starts_with("dev")
            || matches!(
                key.as_str(),
                "hookscript" | "parent" | "snapstate" | "pending" | "snapshots"
            )
        {
            return Err(format!(
                "Unexpected {key}; removal authorizes only rootfs and optional mp0"
            ));
        }
    }
    if let Some(lxc) = cfg.get("lxc") {
        // OCI images use these runtime settings. Other low-level LXC entries
        // may hide mounts or lifecycle hooks outside the storage inventory.
        let allowed = [
            "lxc.init.uid",
            "lxc.init.gid",
            "lxc.init.groups",
            "lxc.init.cwd",
            "lxc.signal.halt",
        ];
        if array(lxc, "LXC configuration")?.iter().any(|entry| {
            entry.as_array().is_none_or(|parts| {
                parts.len() != 2 || parts[0].as_str().is_none_or(|key| !allowed.contains(&key)) || !parts[1].is_string()
            })
        }) {
            return Err("Custom LXC mounts, devices or hooks require manual review".to_owned());
        }
    }
    let description = text(cfg, "description")?;
    if description.lines().any(|line| {
        let line = line.trim();
        line.starts_with("replacement-") || line.starts_with("HomeStack holder")
    }) {
        return Err("Unresolved replacement metadata; finish replacement before removal".to_owned());
    }
    // Duplicate identity lines would make ownership dependent on parser order.
    for prefix in ["HomeStack service ", "repository ", "image ", "digest "] {
        if description.lines().filter(|l| l.trim().starts_with(prefix)).count() != 1
            || description
                .lines()
                .find_map(|l| l.trim().strip_prefix(prefix))
                .is_none_or(|v| v.trim().is_empty())
        {
            return Err(format!("Missing or ambiguous service metadata: {}", prefix.trim()));
        }
    }
    if description.lines().filter(|l| l.trim() == "made by homestack").count() != 1 {
        return Err("Missing or ambiguous managed ownership marker".to_owned());
    }
    for key in ["rootfs", "mp0"] {
        if key == "mp0" && !fields.contains_key(key) {
            continue;
        }
        let mount = text(cfg, key)?;
        let mut parts = mount.split(',');
        let volume = parts.next().unwrap_or_default();
        if !volume
            .split_once(':')
            .is_some_and(|(storage, id)| !storage.is_empty() && !id.is_empty())
        {
            return Err(format!("{key}: expected an exact storage volume ID, not a bind mount"));
        }
        let mut options = BTreeMap::new();
        for part in parts {
            let (key, value) = part.split_once('=').ok_or("Invalid mount options")?;
            if options.insert(key, value).is_some() {
                return Err("Ambiguous mount options".to_owned());
            }
        }
        if key == "mp0" && options.get("mp") != Some(&MOUNT) {
            return Err(format!("mp0 is not the persistent home at {MOUNT}"));
        }
    }
    if hs_deploy::node::extract_hwaddr(text(cfg, "net0")?).is_none() {
        return Err("Network identity has no MAC address".to_owned());
    }
    Ok(())
}

async fn read(pve: &impl Pve, pool: &str, ctid: u32) -> Result<Reviewed, String> {
    let node = placement(pve, ctid, pool).await?;
    let base = format!("/nodes/{node}/lxc/{ctid}");
    let config = get(pve, &format!("{base}/config"), &[]).await?;
    safe_config(&config)?;
    let pending = get(pve, &format!("{base}/pending"), &[]).await?;
    for entry in array(&pending, "Pending configuration")? {
        text(entry, "key")?;
        if entry.get("pending").is_some() || entry.get("delete").is_some_and(|v| v.as_u64() != Some(0)) {
            return Err("Pending configuration changes require a fresh review after resolution".to_owned());
        }
    }
    let snapshots = get(pve, &format!("{base}/snapshot"), &[]).await?;
    for entry in array(&snapshots, "Snapshots")? {
        if text(entry, "name")? != "current" {
            return Err("Snapshots are outside the authorized deletion scope".to_owned());
        }
    }
    let status = get(pve, &format!("{base}/status/current"), &[]).await?;
    let running = match status["status"].as_str() {
        Some("running") => true,
        Some("stopped") => false,
        _ => return Err("Power state unavailable".to_owned()),
    };
    if status["ha"]["managed"].as_u64() != Some(0) && status["ha"]["managed"].as_bool() != Some(false) {
        return Err("HA-managed or unknown HA state requires manual review".to_owned());
    }
    let guest = ctid.to_string();
    let replication = get(pve, &format!("/nodes/{node}/replication"), &[("guest", &guest)]).await?;
    if !array(&replication, "Replication jobs")?.is_empty() {
        return Err("Replication jobs require manual review; removal will not purge them".to_owned());
    }
    let target = instance::parse(ctid, &node, pool, &config, running)?;
    if target.held || target.root_volume == target.home_volume {
        return Err("Holder or ambiguous root/home identity".to_owned());
    }
    // Read storage configuration as well as content: reusing a storage ID for
    // another backend must invalidate the reviewed destructive identity.
    let storage = get(pve, "/storage", &[]).await?;
    let stores = array(&storage, "Storage configuration")?;
    let mut volumes = BTreeMap::new();
    for store in stores {
        let id = text(store, "storage")?;
        if !text(store, "content")?.split(',').any(|c| c == "rootdir")
            || store["disable"].as_u64() == Some(1)
            || store["nodes"]
                .as_str()
                .is_some_and(|nodes| !nodes.split(',').any(|n| n == node))
        {
            continue;
        }
        let status = get(pve, &format!("/nodes/{node}/storage/{id}/status"), &[]).await?;
        if status["active"].as_u64() != Some(1) || status["enabled"].as_u64() != Some(1) {
            return Err(format!(
                "Storage {id} is unavailable; its volume inventory is incomplete"
            ));
        }
        let content = get(
            pve,
            &format!("/nodes/{node}/storage/{id}/content"),
            &[("content", "rootdir")],
        )
        .await?;
        for entry in array(&content, "Storage volumes")? {
            let volid = text(entry, "volid")?;
            let owner = entry["vmid"].as_u64().ok_or("Volume ownership unavailable")?;
            if owner != u64::from(ctid) {
                if volid == target.root_volume || volid == target.home_volume {
                    return Err(format!("Reviewed volume {volid} is owned by another guest"));
                }
                continue;
            }
            if volid != target.root_volume && volid != target.home_volume {
                return Err(format!("Unexpected detached volume {volid}; it will not be cleaned up"));
            }
            if !volid.starts_with(&format!("{id}:")) || volumes.contains_key(volid) {
                return Err(format!("Ambiguous storage inventory for {volid}"));
            }
            // Used bytes change during normal operation; all other attributes,
            // including any creation time, parent, size and format, must match.
            let mut identity = entry.clone();
            identity
                .as_object_mut()
                .ok_or("Invalid volume attributes")?
                .remove("used");
            text(&identity, "format")?;
            volumes.insert(volid.to_owned(), identity);
        }
    }
    for volid in [&target.root_volume, &target.home_volume]
        .into_iter()
        .filter(|v| !v.is_empty())
    {
        if !volumes.contains_key(volid) {
            return Err(format!(
                "Exact volume {volid} missing from authoritative ownership inventory"
            ));
        }
    }
    if placement(pve, ctid, pool).await? != node {
        return Err("Node changed while reading removal inventory".to_owned());
    }
    let final_status = get(pve, &format!("{base}/status/current"), &[]).await?;
    if final_status["status"] != status["status"] || final_status["ha"] != status["ha"] {
        return Err("Power or HA state changed while reading removal inventory".to_owned());
    }
    if get(pve, &format!("{base}/config"), &[]).await? != config {
        return Err("Configuration changed while reading removal inventory".to_owned());
    }
    Ok(Reviewed {
        target,
        config,
        storage,
        volumes,
    })
}

pub(super) async fn review(pve: &impl Pve, pool: &str, ctid: u32) -> Result<Reviewed, String> {
    read(pve, pool, ctid)
        .await
        .map_err(|e| format!("CT {ctid}: {e}; review removal again"))
}

impl Reviewed {
    /// Only our own running -> stopped transition may differ from the review.
    pub async fn revalidate(&self, pve: &impl Pve, stopped: bool) -> Result<(), String> {
        let ctid = self.target.ctid;
        let current = review(pve, &self.target.pool, ctid).await?;
        let reviewed_config = self
            .config
            .as_object()
            .ok_or("Incomplete reviewed container configuration")?;
        let current_config = current
            .config
            .as_object()
            .ok_or("Incomplete current container configuration")?;
        let changed = reviewed_config
            .keys()
            .chain(current_config.keys())
            .find(|key| self.config[*key] != current.config[*key]);
        let reason = if let Some(key) = changed {
            Some(format!("configuration field {key} changed"))
        } else if current.target.node != self.target.node {
            Some("node changed".to_owned())
        } else if current.storage != self.storage {
            Some("storage configuration changed".to_owned())
        } else if current.volumes != self.volumes {
            Some("volume identity or attributes changed".to_owned())
        } else if current.target.running != (!stopped && self.target.running) {
            Some("unexpected power state".to_owned())
        } else {
            None
        };
        match reason {
            Some(reason) => Err(format!("CT {ctid}: {reason}; review removal again")),
            None => Ok(()),
        }
    }
}
