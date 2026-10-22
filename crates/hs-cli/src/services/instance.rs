//! A service container as Proxmox shows it. HomeStack writes the service's
//! identity into the container's description when it makes it; nothing about
//! a deployment is kept on this desktop. A container that is being replaced
//! may exist only as its volume holder (`hs_deploy::recreate`), whose
//! description carries the same identity and the container's settings.

use hs_core::pve::Pve;
use hs_deploy::{node, recreate};
use serde_json::Value;

/// A deployed service container, or the holder of one under replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub ctid: u32,
    pub node: String,
    pub pool: String,
    pub hostname: String,
    pub address: String,
    pub mac: String,
    /// `net0` as Proxmox has it.
    pub network: String,
    pub nameserver: Option<String>,
    pub cores: u32,
    pub memory_mib: u32,
    pub root_volume: String,
    pub root_storage: String,
    pub root_gib: u32,
    pub home_volume: String,
    pub home_storage: String,
    pub home_gib: u32,
    /// `mp0` as Proxmox has it.
    pub home_mount: String,
    pub running: bool,
    pub onboot: bool,
    pub repository: String,
    pub service: String,
    pub image: String,
    pub digest: String,
    /// The description, as HomeStack wrote it.
    pub description: String,
    /// Only the holder of the container is left: the replacement stopped
    /// after the old container was removed.
    pub held: bool,
}

fn text<'a>(cfg: &'a Value, key: &str) -> &'a str {
    cfg[key].as_str().unwrap_or_default()
}

/// The value of a `key value` line of a description.
fn line<'a>(description: &'a str, key: &str) -> Option<&'a str> {
    description
        .lines()
        .find_map(|line| line.trim().strip_prefix(key)?.strip_prefix(' '))
        .map(str::trim)
}

/// The size of a volume option (`size=4G`) in whole GiB.
fn gib(option: &str) -> Option<u32> {
    let size = option.split(',').find_map(|v| v.strip_prefix("size="))?;
    let (number, factor) = match size.as_bytes().last()? {
        b'T' => (&size[..size.len() - 1], 1024),
        b'G' => (&size[..size.len() - 1], 1),
        _ => return None,
    };
    number.parse::<u32>().ok().map(|n| n * factor)
}

fn volume(option: &str) -> (String, String) {
    let volume = option.split(',').next().unwrap_or_default().to_owned();
    let storage = volume.split_once(':').map_or("", |(storage, _)| storage).to_owned();
    (volume, storage)
}

/// The settings a holder keeps of the container it serves, besides the
/// service's identity: what the replacement needs while that container is gone.
#[must_use]
pub fn note(instance: &Instance) -> String {
    let mut note = format!(
        "{}\nhostname {}\ncores {}\nmemory {}\nrootfs {} {}\npool {}\nnet0 {}\nrunning {}\nonboot {}",
        instance.description.trim_end(),
        instance.hostname,
        instance.cores,
        instance.memory_mib,
        instance.root_storage,
        instance.root_gib,
        instance.pool,
        instance.network,
        u8::from(instance.running),
        u8::from(instance.onboot),
    );
    if let Some(nameserver) = &instance.nameserver {
        note.push_str(&format!("\nnameserver {nameserver}"));
    }
    note
}

/// `ctid` on `node` from its configuration; `running` from the node.
pub fn parse(ctid: u32, node: &str, pool: &str, cfg: &Value, running: bool) -> Result<Instance, String> {
    let description = text(cfg, "description").to_owned();
    let service = hs_core::registry::deployed_service(cfg)
        .ok_or_else(|| format!("CT {ctid} is not a service HomeStack deployed"))?;
    let held = recreate::holder_of(&description).is_some();
    let (root_volume, mut root_storage) = volume(text(cfg, "rootfs"));
    let mut root_gib = gib(text(cfg, "rootfs")).unwrap_or_default();
    let (home_volume, home_storage) = volume(text(cfg, "mp0"));
    let mut instance = Instance {
        ctid,
        node: node.to_owned(),
        pool: pool.to_owned(),
        hostname: text(cfg, "hostname").to_owned(),
        address: String::new(),
        mac: String::new(),
        network: text(cfg, "net0").to_owned(),
        nameserver: cfg["nameserver"].as_str().map(str::to_owned),
        cores: cfg["cores"].as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(1),
        memory_mib: cfg["memory"].as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(512),
        root_volume,
        root_storage: root_storage.clone(),
        root_gib,
        home_volume,
        home_storage,
        home_gib: gib(text(cfg, "mp0")).unwrap_or_default(),
        home_mount: text(cfg, "mp0").to_owned(),
        running,
        onboot: cfg["onboot"].as_u64() == Some(1),
        repository: service.repository,
        service: service.id,
        image: service.image,
        digest: service.digest,
        description: description.clone(),
        held,
    };
    if held {
        // The holder's own settings are a holder's; the container's are in its note.
        let value = |key: &str| line(&description, key).ok_or_else(|| format!("holder of CT {ctid}: no {key}"));
        instance.hostname = value("hostname")?.to_owned();
        instance.cores = value("cores")?.parse().map_err(|_| "holder: bad cores")?;
        instance.memory_mib = value("memory")?.parse().map_err(|_| "holder: bad memory")?;
        let (storage, size) = value("rootfs")?.split_once(' ').ok_or("holder: bad rootfs")?;
        storage.clone_into(&mut root_storage);
        root_gib = size.parse().map_err(|_| "holder: bad rootfs size")?;
        instance.root_storage = root_storage;
        instance.root_gib = root_gib;
        instance.root_volume.clear();
        instance.pool = value("pool")?.to_owned();
        instance.network = value("net0")?.to_owned();
        instance.nameserver = line(&description, "nameserver").map(str::to_owned);
        instance.running = line(&description, "running") == Some("1");
        instance.onboot = line(&description, "onboot") == Some("1");
        instance.ctid = recreate::holder_of(&description).ok_or("holder: no CTID")?;
    }
    instance.address = instance
        .network
        .split(',')
        .find_map(|part| part.strip_prefix("ip="))
        .map(|ip| ip.split('/').next().unwrap_or_default().to_owned())
        .unwrap_or_default();
    instance.mac = node::extract_hwaddr(&instance.network).unwrap_or_default();
    Ok(instance)
}

async fn fetch(pve: &impl Pve, node: &str, pool: &str, id: u32) -> Result<Instance, String> {
    let cfg = pve
        .get(&format!("/nodes/{node}/lxc/{id}/config"), &[])
        .await
        .map_err(|e| format!("CT {id} config: {e}"))?;
    let current = pve
        .get(&format!("/nodes/{node}/lxc/{id}/status/current"), &[])
        .await
        .map_err(|e| format!("CT {id} state: {e}"))?;
    let running = match current["status"].as_str() {
        Some("running") => true,
        Some("stopped") => false,
        _ => return Err(format!("CT {id}: power state unavailable")),
    };
    parse(id, node, pool, &cfg, running)
}

/// `ctid` as the holder on `node` keeps it, if that holder serves it.
async fn held(pve: &impl Pve, node: &str, pool: &str, ctid: u32) -> Result<Option<Instance>, String> {
    // A holder without a service's identity serves something else.
    Ok(fetch(pve, node, pool, recreate::HOLDER_ID)
        .await
        .ok()
        .filter(|held| held.ctid == ctid))
}

/// `ctid`, or its holder when the home waits there: the container is gone
/// or made again without it yet.
async fn located(
    pve: &impl Pve,
    node: &str,
    pool: &str,
    ctid: u32,
    exists: bool,
    holder: bool,
) -> Result<Instance, String> {
    if exists {
        let instance = fetch(pve, node, pool, ctid).await?;
        if instance.home_volume.is_empty()
            && holder
            && let Some(held) = held(pve, node, pool, ctid).await?
        {
            return Ok(held);
        }
        return Ok(instance);
    }
    if holder && let Some(held) = held(pve, node, pool, ctid).await? {
        return Ok(held);
    }
    Err(format!("CT {ctid} does not exist on {node}"))
}

/// Service container `ctid` as Proxmox shows it now, found in the cluster.
pub async fn read(pve: &impl Pve, ctid: u32) -> Result<Instance, String> {
    let found = node::container(pve, ctid).await?;
    let holder = node::container(pve, recreate::HOLDER_ID).await?;
    let (node, pool) = match (&found, &holder) {
        (Some(found), _) => (found.node.clone(), found.pool.clone()),
        (None, Some(holder)) => (holder.node.clone(), holder.pool.clone()),
        (None, None) => return Err(format!("CT {ctid} does not exist")),
    };
    let holder = holder.is_some_and(|holder| holder.node == node);
    located(pve, &node, &pool, ctid, found.is_some(), holder).await
}

/// Service container `ctid` on `node`, read from the node itself: right
/// after a change the cluster's list may not show it yet. Its pool is unknown.
pub async fn read_on(pve: &impl Pve, node: &str, ctid: u32) -> Result<Instance, String> {
    let list = pve
        .get(&format!("/nodes/{node}/lxc"), &[])
        .await
        .map_err(|e| format!("{node} containers: {e}"))?;
    let has = |id: u32| {
        list.as_array()
            .into_iter()
            .flatten()
            .any(|item| item["vmid"].as_u64() == Some(u64::from(id)))
    };
    located(pve, node, "", ctid, has(ctid), has(recreate::HOLDER_ID)).await
}

/// The names of the containers in the cluster, to keep a new one's unique.
/// Virtual machines have names of their own.
pub async fn names(pve: &impl Pve) -> Result<Vec<String>, String> {
    let resources = pve
        .get("/cluster/resources", &[("type", "vm")])
        .await
        .map_err(|e| format!("cluster resources: {e}"))?;
    Ok(resources
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "lxc")
        .filter_map(|item| item["name"].as_str().map(str::to_owned))
        .collect())
}

/// The containers of `repository`/`service` in the cluster, by CTID.
pub async fn of_service(pve: &impl Pve, repository: &str, service: &str) -> Result<Vec<Instance>, String> {
    let resources = pve
        .get("/cluster/resources", &[("type", "vm")])
        .await
        .map_err(|e| format!("cluster resources: {e}"))?;
    let mut found = Vec::new();
    for item in resources.as_array().into_iter().flatten() {
        let (Some(ctid), Some(node)) = (
            item["vmid"].as_u64().and_then(|n| u32::try_from(n).ok()),
            item["node"].as_str(),
        ) else {
            continue;
        };
        if item["type"] != "lxc" || ctid == recreate::HOLDER_ID || item["status"] == "unknown" {
            continue;
        }
        let pool = item["pool"].as_str().unwrap_or_default();
        if let Ok(instance) = fetch(pve, node, pool, ctid).await
            && instance.repository == repository
            && instance.service == service
        {
            found.push(instance);
        }
    }
    found.sort_by_key(|instance| instance.ctid);
    Ok(found)
}
