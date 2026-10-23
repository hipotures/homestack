//! Maintenance of deployed service containers through the Proxmox API. The
//! settings show what the containers have; what the operator changes is what
//! is done: a disk grows, the environment changes, or the containers are
//! removed together with their homes.

use hs_core::pve::Pve;
use hs_core::tasks::{NoObserver, TaskWait, wait_task};
use hs_deploy::node;
use serde_json::Value;

use super::deploy::{self, SiteReady, Source};
use super::instance::{self, Instance};

/// The managed runtime selector of initialized API and Site instances.
const MANAGED: &str = "HOMESTACK_MANAGED_HOME";

/// Whether applying `plan` changes an environment, restarting running containers.
pub fn restarts(plan: &Plan) -> bool {
    !plan.remove && (!plan.set.is_empty() || !plan.unset.is_empty())
}

/// Editable environment entries, excluding secrets and API bootstrap settings.
#[must_use]
pub fn shown_environment(source: &Source, instance: &Instance) -> String {
    instance
        .environment
        .iter()
        .filter(|(key, _)| key != "PATH" && key != MANAGED && !deploy::secret_name(key))
        .filter(|(key, _)| {
            !source.is_api()
                || !matches!(
                    key.as_str(),
                    "HOMESTACK_INSTANCE"
                        | "HOMESTACK_PVE_ENDPOINTS"
                        | "HOMESTACK_PVE_POOL"
                        | "HOMESTACK_PVE_GOLD_POOL"
                        | "HOMESTACK_MANAGED_HOME"
                        | "HOMESTACK_TLS_CERT_B64"
                )
        })
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The settings as shown and as the operator left them.
#[derive(Debug, Clone, Default)]
pub struct Choice {
    pub ctids: Vec<u32>,
    pub cores: String,
    pub memory_mib: String,
    pub root_mib: String,
    pub home_mib: String,
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
    pub cores: Option<u32>,
    pub memory_mib: Option<u32>,
    pub root_mib: Option<u32>,
    pub home_mib: Option<u32>,
    pub set: Vec<(String, String)>,
    pub unset: Vec<String>,
    pub lines: Vec<String>,
    pub problems: Vec<String>,
}

/// A size typed as `10M` or `2G` (a bare number is GiB), in MiB; empty: unchanged.
fn size(text: &str, what: &str) -> Result<Option<u32>, String> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    node::parse_size(text)
        .filter(|mib| *mib <= 16_384 * 1024)
        .map(Some)
        .ok_or_else(|| format!("{what} is a whole number followed by M or G, up to 16384G"))
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
            let home = if target.home_volume.is_empty() {
                String::new()
            } else {
                format!(
                    " and home {} ({})",
                    target.home_volume,
                    node::show_size(target.home_mib)
                )
            };
            plan.lines.push(format!(
                "CT {} {} on {} · {}: remove with root {}{home}",
                target.ctid, target.hostname, target.node, target.address, target.root_volume,
            ));
        }
        return plan;
    }
    match (size(&choice.root_mib, "Root"), size(&choice.home_mib, "Home")) {
        (Ok(root), Ok(home)) => {
            plan.root_mib = root;
            plan.home_mib = home;
        }
        (root, home) => plan.problems.extend([root.err(), home.err()].into_iter().flatten()),
    }
    for (text, what, range, slot) in [
        (&choice.cores, "Cores", 1..=64, &mut plan.cores),
        (&choice.memory_mib, "Memory (MiB)", 64..=1_048_576, &mut plan.memory_mib),
    ] {
        match text.trim() {
            "" => {}
            text => match text.parse::<u32>().ok().filter(|value| range.contains(value)) {
                Some(value) => *slot = Some(value),
                None => plan
                    .problems
                    .push(format!("{what} is a whole number {}–{}", range.start(), range.end())),
            },
        }
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
                let value =
                    |list: &[(String, String)]| list.iter().find(|(key, _)| key == MANAGED).map(|(_, v)| v.clone());
                if value(&entries) != value(&deploy::environment(&choice.shown_environment).unwrap_or_default()) {
                    plan.problems
                        .push(format!("{MANAGED} is set by initialization; it cannot be changed here"));
                }
                plan.set = entries;
            }
            Err(error) => plan.problems.push(error),
        }
    }
    for target in &plan.targets {
        let mut changes = Vec::new();
        for (what, now, wanted) in [
            ("Cores", target.cores, plan.cores),
            ("Memory", target.memory_mib, plan.memory_mib),
        ] {
            if let Some(wanted) = wanted.filter(|wanted| *wanted != now) {
                let unit = if what == "Memory" { " MiB" } else { "" };
                changes.push(format!("  {what} {now}{unit} → {wanted}{unit}"));
            }
        }
        for (what, now, wanted) in [
            ("Root", target.root_mib, plan.root_mib),
            ("Home", target.home_mib, plan.home_mib),
        ] {
            match wanted {
                Some(wanted) if wanted < now => plan.problems.push(format!(
                    "CT {}: {what} is {}; a disk only grows",
                    target.ctid,
                    node::show_size(now)
                )),
                Some(wanted) if wanted > now => {
                    changes.push(format!(
                        "  {what} {} → {}, the filesystem grows online",
                        node::show_size(now),
                        node::show_size(wanted)
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
    source: &Source,
    plan: &Plan,
    site: Option<&dyn SiteReady>,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<Vec<String>, String> {
    if !plan.problems.is_empty() {
        return Err("Resolve the review problems first".to_owned());
    }
    if source.is_site() && restarts(plan) {
        let site = site.ok_or("Restarting a Site requires the verified Pangolin context")?;
        for target in plan.targets.iter().filter(|target| target.running) {
            tokio::task::block_in_place(|| site.identity(target.ctid))?;
        }
    }
    if source.is_site() && plan.remove && site.is_none() {
        return Err("Removing a Site requires the verified Pangolin context".to_owned());
    }
    let mut lines = Vec::new();
    for target in &plan.targets {
        let (ctid, node) = (target.ctid, target.node.as_str());
        let now = instance::read_on(pve, node, ctid).await?;
        if now.mac != target.mac || now.home_volume != target.home_volume {
            return Err(format!("CT {ctid} changed since review; review again"));
        }
        if plan.remove {
            // Its Site first: a failure leaves the container listed, so removal can be repeated.
            if let Some(site) = site.filter(|_| source.is_site()) {
                say(&tokio::task::block_in_place(|| site.remove(ctid, say))?);
            }
            if now.running {
                say(&format!("CT {ctid} · stopping"));
                node::power(pve, node, ctid, "stop").await?;
            }
            say(&format!("CT {ctid} · removing"));
            let upid = pve
                .delete(
                    &format!("/nodes/{node}/lxc/{ctid}"),
                    &[("purge", "1"), ("destroy-unreferenced-disks", "1")],
                )
                .await
                .map_err(|e| format!("remove CT {ctid}: {e}"))?;
            task(pve, &upid, &format!("remove CT {ctid}")).await?;
            let home = if now.home_volume.is_empty() {
                String::new()
            } else {
                format!(" and home {}", now.home_volume)
            };
            lines.push(format!(
                "CT {ctid} {} removed with root {}{home}",
                now.hostname, now.root_volume
            ));
            continue;
        }
        for (disk, now_mib, wanted) in [
            ("rootfs", now.root_mib, plan.root_mib),
            ("mp0", now.home_mib, plan.home_mib),
        ] {
            let Some(wanted) = wanted.filter(|wanted| *wanted > now_mib) else {
                continue;
            };
            let size = node::show_size(wanted);
            say(&format!("CT {ctid} · {disk} → {size}"));
            let upid = pve
                .put(
                    &format!("/nodes/{node}/lxc/{ctid}/resize"),
                    &[("disk", disk), ("size", size.as_str())],
                )
                .await
                .map_err(|e| format!("resize CT {ctid} {disk}: {e}"))?;
            task(pve, &upid, &format!("resize CT {ctid} {disk}")).await?;
        }
        let cores = plan
            .cores
            .filter(|cores| *cores != now.cores)
            .map(|cores| cores.to_string());
        let memory = plan
            .memory_mib
            .filter(|memory| *memory != now.memory_mib)
            .map(|memory| memory.to_string());
        let mut limits: Vec<(&str, &str)> = Vec::new();
        if let Some(cores) = &cores {
            limits.push(("cores", cores));
        }
        if let Some(memory) = &memory {
            limits.push(("memory", memory));
        }
        if !limits.is_empty() {
            say(&format!("CT {ctid} · cores and memory"));
            let upid = pve
                .put(&format!("/nodes/{node}/lxc/{ctid}/config"), &limits)
                .await
                .map_err(|e| format!("configure CT {ctid}: {e}"))?;
            task(pve, &upid, &format!("configure CT {ctid}")).await?;
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
            say(&deploy::ready(source, site, ctid, &target.address, say).await?);
        }
        lines.push(format!(
            "CT {ctid} {}: root {}, home {}{}",
            target.hostname,
            node::show_size(after.root_mib),
            node::show_size(after.home_mib),
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
