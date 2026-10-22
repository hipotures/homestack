//! Maintenance of deployed service containers through the Proxmox API:
//! changing the environment, growing a disk, and removing an instance
//! together with its home.

use hs_core::pve::Pve;
use hs_core::tasks::{NoObserver, TaskWait, wait_task};
use hs_deploy::node;
use serde_json::Value;

use super::deploy;
use super::instance::{self, Instance};
use super::manifest::Manifest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Environment,
    Resize,
    Remove,
}

impl Action {
    pub const ALL: [Self; 3] = [Self::Environment, Self::Resize, Self::Remove];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Environment => "Environment",
            Self::Resize => "Resize",
            Self::Remove => "Remove with its home",
        }
    }
}

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

/// What the operator chose; numbers as typed.
#[derive(Debug, Clone)]
pub struct Choice {
    pub action: Action,
    pub ctids: Vec<u32>,
    pub root_gib: String,
    pub home_gib: String,
    /// Environment: `KEY=value` to set, `KEY=` to remove.
    pub environment: String,
    /// Remove: the selected containers' names, as typed.
    pub confirmation: String,
}

/// The reviewed operation.
#[derive(Debug, Clone)]
pub struct Plan {
    pub action: Action,
    pub targets: Vec<Instance>,
    pub root_gib: Option<u32>,
    pub home_gib: Option<u32>,
    pub set: Vec<(String, String)>,
    pub remove: Vec<String>,
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

/// The names `choice` must confirm a removal with.
#[must_use]
pub fn names(targets: &[Instance]) -> String {
    targets
        .iter()
        .map(|target| target.hostname.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

pub async fn plan(pve: &impl Pve, repository: &str, service: &str, choice: &Choice) -> Plan {
    let mut plan = Plan {
        action: choice.action,
        targets: Vec::new(),
        root_gib: None,
        home_gib: None,
        set: Vec::new(),
        remove: Vec::new(),
        lines: Vec::new(),
        problems: Vec::new(),
    };
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
    match choice.action {
        Action::Environment => {
            match deploy::environment(&choice.environment) {
                Ok(entries) => {
                    for (key, value) in entries {
                        if value.is_empty() {
                            plan.remove.push(key);
                        } else {
                            plan.set.push((key, value));
                        }
                    }
                }
                Err(error) => plan.problems.push(error),
            }
            for target in &plan.targets {
                plan.lines
                    .push(format!("CT {} {} on {}", target.ctid, target.hostname, target.node));
                let now = |key: &str| {
                    target
                        .environment
                        .iter()
                        .find(|(k, _)| k == key)
                        .map(|(_, v)| v.clone())
                };
                let mut changed = false;
                for (key, value) in &plan.set {
                    if now(key).as_deref() != Some(value.as_str()) {
                        changed = true;
                        plan.lines.push(match now(key) {
                            Some(old) => format!("  {key}: {old} → {value}"),
                            None => format!("  {key}: new, {value}"),
                        });
                    }
                }
                for key in &plan.remove {
                    if now(key).is_some() {
                        changed = true;
                        plan.lines.push(format!("  {key}: removed"));
                    }
                }
                if !changed {
                    plan.lines.push("  unchanged".to_owned());
                } else if target.running {
                    plan.lines.push(
                        "  restarts to take the change, then its health is checked; /home/app is untouched".to_owned(),
                    );
                }
            }
        }
        Action::Resize => {
            match (size(&choice.root_gib, "Root"), size(&choice.home_gib, "Home")) {
                (Ok(root), Ok(home)) => {
                    plan.root_gib = root;
                    plan.home_gib = home;
                }
                (root, home) => plan.problems.extend([root.err(), home.err()].into_iter().flatten()),
            }
            if plan.root_gib.is_none() && plan.home_gib.is_none() && plan.problems.is_empty() {
                plan.problems
                    .push("Give the new size of the root or the home".to_owned());
            }
            for target in &plan.targets {
                plan.lines
                    .push(format!("CT {} {} on {}", target.ctid, target.hostname, target.node));
                for (what, now, wanted) in [
                    ("Root", target.root_gib, plan.root_gib),
                    ("Home", target.home_gib, plan.home_gib),
                ] {
                    let Some(wanted) = wanted else { continue };
                    if wanted <= now {
                        plan.problems
                            .push(format!("CT {}: {what} is {now} GiB; a disk only grows", target.ctid));
                    }
                    plan.lines
                        .push(format!("  {what} {now} GiB → {wanted} GiB, filesystem grows online"));
                }
            }
        }
        Action::Remove => {
            for target in &plan.targets {
                plan.lines.push(format!(
                    "CT {} {} on {} · {}",
                    target.ctid, target.hostname, target.node, target.address
                ));
                plan.lines.push(format!(
                    "  removed with root {} and home {} ({} GiB): its data is gone",
                    target.root_volume, target.home_volume, target.home_gib
                ));
            }
            let wanted = names(&plan.targets);
            if !plan.targets.is_empty() && choice.confirmation.trim() != wanted {
                plan.problems
                    .push(format!("Type the names of the containers to remove: {wanted}"));
            }
        }
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
        match plan.action {
            Action::Environment => {
                let remove: Vec<&str> = plan.remove.iter().map(String::as_str).collect();
                say(&format!("CT {ctid} · environment"));
                node::set_env(pve, node, ctid, &plan.set, &remove).await?;
                let after = instance::read_on(pve, node, ctid).await?;
                if after.environment == now.environment {
                    lines.push(format!("CT {ctid} {}: environment unchanged", target.hostname));
                    continue;
                }
                if now.running {
                    say(&format!("CT {ctid} · restarting"));
                    node::power(pve, node, ctid, "reboot").await?;
                    say(&deploy::healthy(manifest, &target.address).await?);
                }
                lines.push(format!(
                    "CT {ctid} {}: environment set{}",
                    target.hostname,
                    if now.running { ", restarted, healthy" } else { "" }
                ));
            }
            Action::Resize => {
                for (disk, wanted) in [("rootfs", plan.root_gib), ("mp0", plan.home_gib)] {
                    let Some(wanted) = wanted else { continue };
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
                let after = instance::read_on(pve, node, ctid).await?;
                lines.push(format!(
                    "CT {ctid} {}: root {} GiB, home {} GiB",
                    target.hostname, after.root_gib, after.home_gib
                ));
            }
            Action::Remove => {
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
            }
        }
    }
    Ok(lines)
}
