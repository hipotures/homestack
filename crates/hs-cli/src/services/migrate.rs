//! Move a service container to another node through the Proxmox API: a
//! restart migration of the stopped or running container with its root and
//! home onto one storage of the same type. The CTID, address and MAC stay.

use std::time::Duration;

use hs_core::pve::Pve;
use hs_core::tasks::{NoObserver, TaskWait, wait_task};
use serde_json::Value;

use super::deploy::{self, Source};
use super::instance::{self, Instance};
use super::update::permit;

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

/// The enabled storages of `node`.
async fn storages(pve: &impl Pve, node: &str) -> Result<Vec<Value>, String> {
    let list = pve
        .get(&format!("/nodes/{node}/storage"), &[("enabled", "1")])
        .await
        .map_err(|e| format!("{node} storages: {e}"))?;
    Ok(list.as_array().cloned().unwrap_or_default())
}

fn movable(instance: &Instance) -> Result<(), String> {
    if instance.held {
        return Err(format!(
            "CT {} is in the middle of an image update; finish it first",
            instance.ctid
        ));
    }
    Ok(())
}

pub async fn plan(pve: &impl Pve, target: &Target) -> Result<Plan, String> {
    let instance = instance::read(pve, target.ctid).await?;
    let mut problems = Vec::new();
    if let Err(problem) = movable(&instance) {
        problems.push(problem);
    }
    let mut lines = vec![format!(
        "CT {} {}: {} → {}",
        target.ctid, instance.hostname, instance.node, target.node
    )];
    if instance.node == target.node {
        problems.push(format!("CT {} is already on {}", target.ctid, target.node));
        return Ok(Plan { lines, problems });
    }
    let source = storages(pve, &instance.node).await?;
    let wanted = storages(pve, &target.node).await?;
    let kind = |list: &[Value], id: &str| {
        list.iter()
            .find(|s| s["storage"] == id)
            .map(|s| text(s, "type").to_owned())
    };
    match wanted.iter().find(|s| s["storage"] == target.storage.as_str()) {
        None => problems.push(format!("{}: storage {} unavailable", target.node, target.storage)),
        Some(storage) => {
            let to = text(storage, "type");
            for volume in [&instance.root_volume, &instance.home_volume] {
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
    lines.push(format!(
        "Root {} ({} GiB) → {}",
        instance.root_volume, instance.root_gib, target.storage
    ));
    lines.push(format!(
        "Home {} ({} GiB) → {}",
        instance.home_volume, instance.home_gib, target.storage
    ));
    lines.push(if instance.running {
        "Restart migration: the service stops, its disks move, it starts again and its health is checked.".to_owned()
    } else {
        "The stopped container moves; it stays stopped.".to_owned()
    });
    lines.push(format!(
        "CTID {}, address {} and MAC {} stay.",
        target.ctid, instance.address, instance.mac
    ));
    Ok(Plan { lines, problems })
}

pub async fn apply(
    pve: &impl Pve,
    source: &Source,
    target: &Target,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<Vec<String>, String> {
    let ctid = target.ctid;
    let before = instance::read(pve, ctid).await?;
    movable(&before)?;
    if before.node != target.node {
        let mut form = vec![("target", target.node.as_str()), ("target-storage", target.storage.as_str())];
        if before.running {
            form.push(("restart", "1"));
        }
        say(&format!("CT {ctid} · migrating {} → {}", before.node, target.node));
        let upid = pve
            .post(&format!("/nodes/{}/lxc/{ctid}/migrate", before.node), &form)
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
    let after = instance::read_on(pve, &target.node, ctid).await?;
    if after.mac != before.mac
        || after.description != before.description
        || after.home_volume.is_empty()
        || storage_of(&after.home_volume) != target.storage
    {
        return Err(format!(
            "CT {ctid} on {} differs from the container that moved",
            target.node
        ));
    }
    say(&format!(
        "CT {ctid} · root {} · home {}",
        after.root_volume, after.home_volume
    ));
    if after.running {
        say(&format!("CT {ctid} · health"));
        say(&deploy::healthy(&source.manifest, &after.address).await?);
    }
    Ok(vec![format!(
        "CT {ctid} {} is on {} · root {} · home {}{}",
        after.hostname,
        target.node,
        after.root_volume,
        after.home_volume,
        if after.running { " · running, healthy" } else { " · stopped" }
    )])
}
