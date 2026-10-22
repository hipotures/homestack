//! Move a registered service container to another node through the Proxmox
//! API: a restart migration of the stopped or running container with its root
//! and home onto one storage of the same type. The CTID, address and MAC stay.
//! A container already on the target node (an interrupted migration, or one
//! moved in Proxmox) is only recorded there.

use std::path::Path;
use std::time::Duration;

use hs_core::pve::Pve;
use hs_core::tasks::{NoObserver, TaskWait, wait_task};
use serde_json::Value;

use super::deploy::{self, Source};
use super::registry::{self, Instance};
use super::replacement::permit;

/// Copying the disks takes as long as their used data.
const MIGRATE_LIMIT: Duration = Duration::from_secs(3600);

/// What the form chose.
#[derive(Debug, Clone)]
pub struct Target {
    pub ctid: u32,
    pub node: String,
    pub storage: String,
}

/// The reviewed move.
pub struct Plan {
    pub lines: Vec<String>,
    pub problems: Vec<String>,
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}

fn storage_of(volume: &str) -> &str {
    volume.split_once(':').map_or("", |(storage, _)| storage)
}

/// The enabled storages of `node` by ID.
async fn storages(pve: &impl Pve, node: &str) -> Result<Vec<Value>, String> {
    let list = pve
        .get(&format!("/nodes/{node}/storage"), &[("enabled", "1")])
        .await
        .map_err(|e| format!("{node} storages: {e}"))?;
    Ok(list.as_array().cloned().unwrap_or_default())
}

/// The node the container is on now, read from each node itself.
async fn where_is(pve: &impl Pve, nodes: &[&str], ctid: u32) -> Result<Option<String>, String> {
    for node in nodes {
        let list = pve
            .get(&format!("/nodes/{node}/lxc"), &[])
            .await
            .map_err(|e| format!("{node} containers: {e}"))?;
        if list
            .as_array()
            .into_iter()
            .flatten()
            .any(|item| item["vmid"].as_u64() == Some(u64::from(ctid)))
        {
            return Ok(Some((*node).to_owned()));
        }
    }
    Ok(None)
}

/// The container's configuration must still be the registered service's.
fn same_service(instance: &Instance, cfg: &Value) -> Result<(), String> {
    if !text(cfg, "description").starts_with("HomeStack service ")
        || text(cfg, "hostname") != instance.hostname
        || hs_deploy::node::extract_hwaddr(text(cfg, "net0")).as_deref() != Some(instance.mac.as_str())
    {
        return Err(format!(
            "CT {} is no longer the registered {} (description, hostname or MAC differ)",
            instance.ctid, instance.hostname
        ));
    }
    let keys = hs_deploy::recreate::mounts(cfg)?;
    if keys != ["mp0"] {
        return Err(format!("CT {}: only a single home volume (mp0) can move", instance.ctid));
    }
    Ok(())
}

pub async fn plan(pve: &impl Pve, path: &Path, target: &Target) -> Result<Plan, String> {
    let registry = registry::load(path)?;
    let (record, instance) = registry
        .instance(target.ctid)
        .ok_or_else(|| format!("CT {} is not a registered service", target.ctid))?;
    let mut problems = Vec::new();
    if record.operation.is_some() {
        problems.push("Finish the service's interrupted operation first".to_owned());
    }
    let nodes = [instance.node.as_str(), target.node.as_str()];
    let now = where_is(pve, &nodes, target.ctid)
        .await?
        .ok_or_else(|| format!("CT {} is on neither {} nor {}", target.ctid, instance.node, target.node))?;
    let cfg = pve
        .get(&format!("/nodes/{now}/lxc/{}/config", target.ctid), &[])
        .await
        .map_err(|e| format!("CT {} config: {e}", target.ctid))?;
    if let Err(problem) = same_service(instance, &cfg) {
        problems.push(problem);
    }
    if cfg.get("lock").is_some() {
        problems.push(format!("CT {} is locked; wait for its task", target.ctid));
    }
    let root = text(&cfg, "rootfs").split(',').next().unwrap_or_default().to_owned();
    let home = text(&cfg, "mp0").split(',').next().unwrap_or_default().to_owned();
    let mut lines = vec![format!(
        "CT {} {}: {} → {}",
        target.ctid, instance.hostname, instance.node, target.node
    )];
    if now == target.node {
        lines.push(format!(
            "It is already on {}; only the registry records it there.",
            target.node
        ));
        lines.push(format!("Root {root} · home {home}"));
        return Ok(Plan { lines, problems });
    }
    let source = storages(pve, &now).await?;
    let wanted = storages(pve, &target.node).await?;
    let kind = |list: &[Value], id: &str| {
        list.iter()
            .find(|s| s["storage"] == id)
            .map(|s| text(s, "type").to_owned())
    };
    let chosen = wanted.iter().find(|s| s["storage"] == target.storage.as_str());
    match chosen {
        None => problems.push(format!("{}: storage {} unavailable", target.node, target.storage)),
        Some(storage) => {
            let to = text(storage, "type");
            for volume in [&root, &home] {
                let from = kind(&source, storage_of(volume)).unwrap_or_default();
                if from != to {
                    problems.push(format!(
                        "{volume} is on {from}; Proxmox moves it only to the same type, not {to}"
                    ));
                }
            }
            if storage["active"].as_u64() != Some(1) || !text(storage, "content").split(',').any(|c| c == "rootdir") {
                problems.push(format!("Storage {} is inactive or lacks rootdir", target.storage));
            }
            let need = u64::from(instance.root_gib + instance.home_gib) * 1024 * 1024 * 1024;
            if storage["avail"].as_u64().is_none_or(|avail| avail < need) {
                problems.push(format!(
                    "Storage {} has less than {} GiB free",
                    target.storage,
                    instance.root_gib + instance.home_gib
                ));
            }
        }
    }
    let permissions = pve.get("/access/permissions", &[]).await.map_err(|e| e.to_string())?;
    if !permit(&permissions, &format!("/vms/{}", target.ctid), "VM.Migrate")
        && !permit(&permissions, &format!("/pool/{}", instance.pool), "VM.Migrate")
    {
        problems.push(
            "The deploy token lacks VM.Migrate: as root run pveum role modify HomeStackDeploy with VM.Migrate added, \
             or run the installer again"
                .to_owned(),
        );
    }
    let running = pve
        .get(&format!("/nodes/{now}/lxc/{}/status/current", target.ctid), &[])
        .await
        .map_err(|e| format!("CT {} state: {e}", target.ctid))?["status"]
        == "running";
    lines.push(format!("Root {root} ({} GiB) → {}", instance.root_gib, target.storage));
    lines.push(format!("Home {home} ({} GiB) → {}", instance.home_gib, target.storage));
    lines.push(if running {
        "Restart migration: the service stops, its disks move, it starts again and its health is checked.".to_owned()
    } else {
        "The stopped container moves; it stays stopped.".to_owned()
    });
    lines.push(format!("CTID {}, address {} and MAC {} stay.", target.ctid, instance.address, instance.mac));
    Ok(Plan { lines, problems })
}

pub async fn apply(
    pve: &impl Pve,
    path: &Path,
    source: &Source,
    target: &Target,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<Vec<String>, String> {
    let _lock = registry::operation_lock(path)?;
    let registry = registry::load(path)?;
    let (_, instance) = registry
        .instance(target.ctid)
        .ok_or_else(|| format!("CT {} is not a registered service", target.ctid))?;
    let instance = instance.clone();
    let ctid = target.ctid;
    let nodes = [instance.node.as_str(), target.node.as_str()];
    let now = where_is(pve, &nodes, ctid)
        .await?
        .ok_or_else(|| format!("CT {ctid} is on neither {} nor {}", instance.node, target.node))?;
    if now != target.node {
        let cfg = pve
            .get(&format!("/nodes/{now}/lxc/{ctid}/config"), &[])
            .await
            .map_err(|e| format!("CT {ctid} config: {e}"))?;
        same_service(&instance, &cfg)?;
        let running = pve
            .get(&format!("/nodes/{now}/lxc/{ctid}/status/current"), &[])
            .await
            .map_err(|e| format!("CT {ctid} state: {e}"))?["status"]
            == "running";
        let mut form = vec![("target", target.node.as_str()), ("target-storage", target.storage.as_str())];
        if running {
            form.push(("restart", "1"));
        }
        say(&format!("CT {ctid} · migrating {now} → {}", target.node));
        let upid = pve
            .post(&format!("/nodes/{now}/lxc/{ctid}/migrate"), &form)
            .await
            .map_err(|e| format!("migrate CT {ctid}: {e}"))?;
        let upid = upid.as_str().ok_or("Proxmox gave no migration task")?;
        let result = wait_task(pve, upid, &TaskWait::up_to(MIGRATE_LIMIT), &NoObserver)
            .await
            .map_err(|e| format!("migrate CT {ctid}: {e}"))?;
        if !result.ok() {
            return Err(format!("migrate CT {ctid}: {}", result.exitstatus));
        }
    }
    if where_is(pve, &[target.node.as_str()], ctid).await?.is_none() {
        return Err(format!("CT {ctid} is not on {} after the migration", target.node));
    }
    let base = format!("/nodes/{}/lxc/{ctid}", target.node);
    let cfg = pve
        .get(&format!("{base}/config"), &[])
        .await
        .map_err(|e| format!("CT {ctid} config: {e}"))?;
    same_service(&instance, &cfg)?;
    let root = text(&cfg, "rootfs").split(',').next().unwrap_or_default().to_owned();
    let home = text(&cfg, "mp0").split(',').next().unwrap_or_default().to_owned();
    say(&format!("CT {ctid} · root {root} · home {home}"));
    let running = pve
        .get(&format!("{base}/status/current"), &[])
        .await
        .map_err(|e| format!("CT {ctid} state: {e}"))?["status"]
        == "running";
    let mut verified = instance.verified.clone();
    if running {
        say(&format!("CT {ctid} · health"));
        say(&deploy::healthy(&source.manifest, &instance.address).await?);
        verified = source.manifest.healthcheck.as_ref().map(|_| deploy::stamp());
    }
    registry::update(path, |registry| {
        let slot = registry
            .deployments
            .iter_mut()
            .flat_map(|d| d.instances.iter_mut())
            .find(|i| i.ctid == ctid)
            .ok_or("Instance disappeared")?;
        slot.node.clone_from(&target.node);
        slot.root_storage = storage_of(&root).to_owned();
        slot.home_storage = storage_of(&home).to_owned();
        slot.root_volume.clone_from(&root);
        slot.home_volume.clone_from(&home);
        slot.verified.clone_from(&verified);
        Ok(())
    })?;
    Ok(vec![format!(
        "CT {ctid} {} is on {} · root {root} · home {home}{}",
        instance.hostname,
        target.node,
        if running { " · running, healthy" } else { " · stopped" }
    )])
}
