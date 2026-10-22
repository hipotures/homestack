//! Maintenance of deployed service containers through the Proxmox API. The
//! settings show what the containers have; what the operator changes is what
//! is done: a disk grows, the environment changes, or the containers are
//! removed together with their homes.

use hs_core::pve::Pve;
use hs_core::tasks::{NoObserver, TaskWait, wait_task};
use hs_deploy::node;
use serde_json::Value;

use super::deploy;
use super::instance::{self, Instance};
use super::manifest::Manifest;

/// The entries the environment setting shows: neither `PATH` nor anything
/// named like a secret.
#[must_use]
pub fn shown_environment(instance: &Instance) -> String {
    instance
        .environment
        .iter()
        .filter(|(key, _)| key != "PATH" && !deploy::secret_name(key))
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The settings as shown and as the operator left them.
#[derive(Debug, Clone, Default)]
pub struct Choice {
    pub ctids: Vec<u32>,
    pub root_gib: String,
    pub home_gib: String,
    pub environment: String,
    /// What the environment setting showed.
    pub shown_environment: String,
    /// The selected containers' names, typed to remove them.
    pub remove: String,
}

/// The reviewed changes.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub targets: Vec<Instance>,
    pub remove: bool,
    pub root_gib: Option<u32>,
    pub home_gib: Option<u32>,
    pub set: Vec<(String, String)>,
    pub unset: Vec<String>,
    pub lines: Vec<String>,
    pub problems: Vec<String>,
}

fn size(text: &str, what: &str) -> Result<Option<u32>, String> {
    let text = text.trim().trim_end_matches(['G', 'g']);
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<u32>()
        .ok()
        .filter(|n| (1..=16_384).contains(n))
        .map(Some)
        .ok_or_else(|| format!("{what} is a whole number of GiB from 1 to 16384"))
}

/// The names that confirm removing `targets`.
#[must_use]
pub fn names(targets: &[Instance]) -> String {
    targets
        .iter()
        .map(|target| target.hostname.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

pub async fn plan(pve: &impl Pve, repository: &str, service: &str, choice: &Choice) -> Plan {
    let mut plan = Plan::default();
    if choice.ctids.is_empty() {
        plan.problems.push("Select at least one instance".to_owned());
    }
    for ctid in &choice.ctids {
        match instance::read(pve, *ctid).await {
            Ok(target) if target.repository == repository && target.service == service && !target.held => {
                plan.targets.push(target);
            }
            Ok(_) => plan.problems.push(format!(
                "CT {ctid} is not an instance of this service ready for maintenance"
            )),
            Err(error) => plan.problems.push(error),
        }
    }
    if !choice.remove.trim().is_empty() {
        let wanted = names(&plan.targets);
        if choice.remove.trim() != wanted {
            plan.problems.push(format!(
                "To remove them, type the names of the selected containers: {wanted}"
            ));
        }
        plan.remove = true;
        for target in &plan.targets {
            plan.lines.push(format!(
                "CT {} {} on {} · {}: removed with root {} and home {} ({} GiB); its data is gone",
                target.ctid,
                target.hostname,
                target.node,
                target.address,
                target.root_volume,
                target.home_volume,
                target.home_gib
            ));
        }
        return plan;
    }
    match (size(&choice.root_gib, "Root"), size(&choice.home_gib, "Home")) {
        (Ok(root), Ok(home)) => {
            plan.root_gib = root;
            plan.home_gib = home;
        }
        (root, home) => plan.problems.extend([root.err(), home.err()].into_iter().flatten()),
    }
    if choice.environment.trim() != choice.shown_environment.trim() {
        match deploy::environment(&choice.environment) {
            Ok(entries) => {
                // An entry taken out of the shown ones is removed.
                let kept: Vec<&str> = entries.iter().map(|(key, _)| key.as_str()).collect();
                plan.unset = deploy::environment(&choice.shown_environment)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(key, _)| key)
                    .filter(|key| !kept.contains(&key.as_str()))
                    .collect();
                plan.set = entries;
            }
            Err(error) => plan.problems.push(error),
        }
    }
    for target in &plan.targets {
        let mut changes = Vec::new();
        for (what, now, wanted) in [
            ("Root", target.root_gib, plan.root_gib),
            ("Home", target.home_gib, plan.home_gib),
        ] {
            match wanted {
                Some(wanted) if wanted < now => plan
                    .problems
                    .push(format!("CT {}: {what} is {now} GiB; a disk only grows", target.ctid)),
                Some(wanted) if wanted > now => {
                    changes.push(format!(
                        "  {what} {now} GiB → {wanted} GiB, the filesystem grows online"
                    ));
                }
                _ => {}
            }
        }
        let now = |key: &str| {
            target
                .environment
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        let mut environment = false;
        for (key, value) in &plan.set {
            if now(key).as_deref() != Some(value.as_str()) {
                environment = true;
                changes.push(match now(key) {
                    Some(old) => format!("  {key}: {old} → {value}"),
                    None => format!("  {key}: new, {value}"),
                });
            }
        }
        for key in &plan.unset {
            if now(key).is_some() {
                environment = true;
                changes.push(format!("  {key}: removed"));
            }
        }
        if environment && target.running {
            changes.push("  restarts to read its environment; /home/app is untouched".to_owned());
        }
        if !changes.is_empty() {
            plan.lines
                .push(format!("CT {} {} on {}", target.ctid, target.hostname, target.node));
            plan.lines.extend(changes);
        }
    }
    if plan.lines.is_empty() && plan.problems.is_empty() {
        plan.problems
            .push("Nothing changes: change a size or the environment, or type the names to remove".to_owned());
    }
    plan
}

async fn task(pve: &impl Pve, upid: &Value, what: &str) -> Result<(), String> {
    // A configuration change may finish without a task.
    let Some(upid) = upid.as_str() else { return Ok(()) };
    let result = wait_task(
        pve,
        upid,
        &TaskWait::up_to(std::time::Duration::from_secs(900)),
        &NoObserver,
    )
    .await
    .map_err(|e| format!("{what}: {e}"))?;
    if result.ok() {
        Ok(())
    } else {
        Err(format!("{what}: {}", result.exitstatus))
    }
}

pub async fn apply(
    pve: &impl Pve,
    manifest: &Manifest,
    plan: &Plan,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<Vec<String>, String> {
    if !plan.problems.is_empty() {
        return Err("Resolve the review problems first".to_owned());
    }
    let mut lines = Vec::new();
    for target in &plan.targets {
        let (ctid, node) = (target.ctid, target.node.as_str());
        let now = instance::read_on(pve, node, ctid).await?;
        if now.mac != target.mac || now.home_volume != target.home_volume {
            return Err(format!("CT {ctid} changed since review; review again"));
        }
        if plan.remove {
            if now.running {
                say(&format!("CT {ctid} · stopping"));
                node::power(pve, node, ctid, "stop").await?;
            }
            say(&format!("CT {ctid} · removing with its home"));
            let upid = pve
                .delete(
                    &format!("/nodes/{node}/lxc/{ctid}"),
                    &[("purge", "1"), ("destroy-unreferenced-disks", "1")],
                )
                .await
                .map_err(|e| format!("remove CT {ctid}: {e}"))?;
            task(pve, &upid, &format!("remove CT {ctid}")).await?;
            lines.push(format!(
                "CT {ctid} {} removed with root {} and home {}",
                target.hostname, target.root_volume, target.home_volume
            ));
            continue;
        }
        for (disk, now_gib, wanted) in [
            ("rootfs", now.root_gib, plan.root_gib),
            ("mp0", now.home_gib, plan.home_gib),
        ] {
            let Some(wanted) = wanted.filter(|wanted| *wanted > now_gib) else {
                continue;
            };
            say(&format!("CT {ctid} · {disk} → {wanted} GiB"));
            let size = format!("{wanted}G");
            let upid = pve
                .put(
                    &format!("/nodes/{node}/lxc/{ctid}/resize"),
                    &[("disk", disk), ("size", size.as_str())],
                )
                .await
                .map_err(|e| format!("resize CT {ctid} {disk}: {e}"))?;
            task(pve, &upid, &format!("resize CT {ctid} {disk}")).await?;
        }
        if !plan.set.is_empty() || !plan.unset.is_empty() {
            let unset: Vec<&str> = plan.unset.iter().map(String::as_str).collect();
            say(&format!("CT {ctid} · environment"));
            node::set_env(pve, node, ctid, &plan.set, &unset).await?;
        }
        let after = instance::read_on(pve, node, ctid).await?;
        let restarted = after.environment != now.environment && now.running;
        if restarted {
            say(&format!("CT {ctid} · restarting"));
            node::power(pve, node, ctid, "reboot").await?;
            say(&deploy::healthy(manifest, &target.address).await?);
        }
        lines.push(format!(
            "CT {ctid} {}: root {} GiB, home {} GiB{}",
            target.hostname,
            after.root_gib,
            after.home_gib,
            if restarted {
                ", environment set, restarted, healthy"
            } else if after.environment != now.environment {
                ", environment set"
            } else {
                ""
            }
        ));
    }
    Ok(lines)
}
