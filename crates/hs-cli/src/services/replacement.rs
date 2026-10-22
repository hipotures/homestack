//! Replace a registered service using Proxmox API only. Prepare the replacement without
//! network, home or autostart, then transfer the existing home and network.
//! The original container remains stopped with its root. No guest commands or SSH are involved.

use std::collections::BTreeSet;
use std::path::Path;

use hs_core::pve::Pve;
use hs_deploy::{config::DeployConfig, node};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::deploy::{self, Source};
use super::lifecycle::{self, Action, Choice};
use super::registry::{self, Deployment, Desired, Instance, Operation, Release, Retired};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    Pull,
    Create,
    Prepare,
    Shutdown,
    Backup,
    MoveHome,
    Disconnect,
    Connect,
    Start,
    Register,
    Complete,
}

/// Non-secret intent and evidence. Runtime environment values stay in PVE.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub id: String,
    pub commit: String,
    pub blob: String,
    pub image: String,
    pub digest: String,
    pub platform: String,
    pub template_storage: String,
    pub backup_storage: String,
    pub targets: Vec<Target>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub before: Instance,
    pub after: Instance,
    pub running: bool,
    pub onboot: bool,
    pub source_description: String,
    pub home_mount: String,
    pub network: String,
    pub step: Step,
    pub task: Option<String>,
    pub transfer_task: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub record: Deployment,
    pub journal: Journal,
    /// Exact review snapshots, including runtime values; never persisted/logged.
    pub configs: Vec<(u32, Value)>,
    pub problems: Vec<String>,
    pub lines: Vec<String>,
    pub recovery: bool,
}

fn text<'a>(cfg: &'a Value, key: &str) -> &'a str {
    cfg[key].as_str().unwrap_or_default()
}

fn volume(cfg: &Value, key: &str) -> String {
    text(cfg, key).split(',').next().unwrap_or_default().to_owned()
}

fn tags(cfg: &Value) -> Vec<String> {
    text(cfg, "tags")
        .split([';', ','])
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn description(source: &Source, journal: &Journal, target: &Target) -> String {
    format!(
        "{}\nreplacement {} from CT {} root {}",
        deploy::description(source, &journal.image, &journal.digest),
        journal.id,
        target.before.ctid,
        target.before.root_volume
    )
}

fn permit(perms: &Value, path: &str, privilege: &str) -> bool {
    // PVE values indicate propagation, not whether the listed privilege exists.
    if perms[path][privilege].as_u64().is_some_and(|value| value <= 1) {
        return true;
    }
    let mut parent = path;
    while let Some((prefix, _)) = parent.rsplit_once('/') {
        parent = if prefix.is_empty() { "/" } else { prefix };
        if perms[parent][privilege].as_u64() == Some(1) {
            return true;
        }
        if parent == "/" {
            break;
        }
    }
    false
}

async fn pool_member(pve: &impl Pve, instance: &Instance) -> Result<(), String> {
    let pool = pve
        .get(&format!("/pools/{}", instance.pool), &[])
        .await
        .map_err(|e| e.to_string())?;
    if pool["members"].as_array().is_some_and(|members| {
        members.iter().any(|m| {
            m["vmid"].as_u64() == Some(u64::from(instance.ctid)) && m["type"] == "lxc" && m["node"] == instance.node
        })
    }) {
        Ok(())
    } else {
        Err(format!("CT {} is not in the recorded node/pool", instance.ctid))
    }
}

async fn read(pve: &impl Pve, instance: &Instance) -> Result<(node::Container, Value), String> {
    let (mut found, cfg) = deploy::config_of(pve, instance.ctid)
        .await?
        .ok_or_else(|| format!("CT {} is missing", instance.ctid))?;
    if found.node != instance.node {
        return Err(format!("CT {} changed node", instance.ctid));
    }
    pool_member(pve, instance).await?;
    // Cluster inventory is cached and may omit a newly created CT's status.
    // Lifecycle decisions require the node's current state, never that cache.
    let current = pve
        .get(
            &format!("/nodes/{}/lxc/{}/status/current", instance.node, instance.ctid),
            &[],
        )
        .await
        .map_err(|error| {
            format!(
                "CT {} on {}: cannot read current power state: {error}",
                instance.ctid, instance.node
            )
        })?;
    found.status = current["status"].as_str().unwrap_or_default().to_owned();
    if !matches!(found.status.as_str(), "running" | "stopped") {
        return Err(format!(
            "CT {} on {}: current power state is unavailable",
            instance.ctid, instance.node
        ));
    }
    Ok((found, cfg))
}

async fn safe_config(pve: &impl Pve, instance: &Instance, cfg: &Value) -> Result<(), String> {
    if cfg.get("lock").is_some() || cfg["protection"].as_u64() == Some(1) || cfg["template"].as_u64() == Some(1) {
        return Err("Locked, protected or template containers require review".to_owned());
    }
    if cfg["unprivileged"].as_u64() != Some(1) {
        return Err("Replacement requires an unprivileged container".to_owned());
    }
    if cfg.as_object().is_none_or(|fields| {
        fields.keys().any(|k| {
            (k.starts_with("mp") && k != "mp0")
                || (k.starts_with("net") && k != "net0")
                || k.starts_with("unused")
                || k.starts_with("dev")
                || k == "hookscript"
        })
    }) {
        return Err("Extra disks, networks, devices or hooks require manual review".to_owned());
    }
    let allowed_lxc = [
        "lxc.init.uid",
        "lxc.init.gid",
        "lxc.init.groups",
        "lxc.init.cwd",
        "lxc.signal.halt",
    ];
    if cfg["lxc"].as_array().is_some_and(|entries| {
        entries
            .iter()
            .any(|entry| entry[0].as_str().is_none_or(|key| !allowed_lxc.contains(&key)))
    }) {
        return Err("Custom LXC configuration requires manual review".to_owned());
    }
    let base = format!("/nodes/{}/lxc/{}", instance.node, instance.ctid);
    let status = pve
        .get(&format!("{base}/status/current"), &[])
        .await
        .map_err(|e| e.to_string())?;
    if status["ha"]["managed"].as_u64() != Some(0) && status["ha"]["managed"].as_bool() != Some(false) {
        return Err("HA-managed or unknown HA state requires a separate placement plan".to_owned());
    }
    let guest = instance.ctid.to_string();
    let replication = pve
        .get(&format!("/nodes/{}/replication", instance.node), &[("guest", &guest)])
        .await
        .map_err(|e| format!("Replication inventory unavailable: {e}"))?;
    if replication.as_array().is_none_or(|jobs| !jobs.is_empty()) {
        return Err("Replication jobs require a separate placement plan".to_owned());
    }
    // Do not silently discard security configuration attached to the old CTID.
    for section in ["rules", "aliases", "ipset"] {
        let data = pve
            .get(&format!("{base}/firewall/{section}"), &[])
            .await
            .map_err(|e| format!("Firewall inventory unavailable: {e}"))?;
        if data.as_array().is_none_or(|entries| !entries.is_empty()) {
            return Err("Custom container firewall configuration requires manual review".to_owned());
        }
    }
    let firewall = pve
        .get(&format!("{base}/firewall/options"), &[])
        .await
        .map_err(|e| format!("Firewall options unavailable: {e}"))?;
    if firewall
        .as_object()
        .is_none_or(|options| options.keys().any(|key| key != "digest"))
    {
        return Err("Custom container firewall options require manual review".to_owned());
    }
    let snapshots = pve
        .get(&format!("{base}/snapshot"), &[])
        .await
        .map_err(|e| e.to_string())?;
    if snapshots
        .as_array()
        .is_none_or(|items| items.iter().any(|s| s["name"] != "current"))
    {
        return Err("Snapshots can prevent home reassignment; inspect them before replacement".to_owned());
    }
    let pending = pve
        .get(&format!("{base}/pending"), &[])
        .await
        .map_err(|e| e.to_string())?;
    if pending.as_array().is_none_or(|items| {
        items
            .iter()
            .any(|i| i.get("pending").is_some() || i["delete"].as_u64() == Some(1))
    }) {
        return Err("Pending configuration requires review".to_owned());
    }
    Ok(())
}

async fn preflight(pve: &impl Pve, cfg: &DeployConfig, instance: &Instance, backup: &str) -> Result<(), String> {
    let permissions = pve.get("/access/permissions", &[]).await.map_err(|e| e.to_string())?;
    let vm = format!("/vms/{}", instance.ctid);
    let pool = format!("/pool/{}", instance.pool);
    for privilege in [
        "VM.Audit",
        "VM.Allocate",
        "VM.PowerMgmt",
        "VM.Config.Disk",
        "VM.Config.Options",
        "VM.Config.Network",
        "VM.Config.CPU",
        "VM.Config.Memory",
    ] {
        if !permit(&permissions, &vm, privilege) && !permit(&permissions, &pool, privilege) {
            return Err(format!("Deploy token lacks {privilege} for CT {}", instance.ctid));
        }
    }
    if !permit(&permissions, &pool, "VM.Allocate") {
        return Err("Deploy token cannot create the replacement in its original pool".to_owned());
    }
    for storage in [&instance.root_storage, &instance.home_storage, &cfg.template_storage] {
        if !permit(&permissions, &format!("/storage/{storage}"), "Datastore.AllocateSpace") {
            return Err(format!("Deploy token lacks Datastore.AllocateSpace on {storage}"));
        }
    }
    if !permit(
        &permissions,
        &format!("/storage/{}", cfg.template_storage),
        "Datastore.AllocateTemplate",
    ) || !permit(&permissions, &format!("/nodes/{}", instance.node), "Sys.AccessNetwork")
    {
        return Err("Deploy token cannot pull the reviewed OCI image".to_owned());
    }
    if !backup.is_empty()
        && ((!permit(&permissions, &vm, "VM.Backup") && !permit(&permissions, &pool, "VM.Backup"))
            || !permit(&permissions, &format!("/storage/{backup}"), "Datastore.AllocateSpace"))
    {
        return Err("Selected backup requires VM.Backup and backup storage allocation".to_owned());
    }
    let storages = pve
        .get(&format!("/nodes/{}/storage", instance.node), &[("enabled", "1")])
        .await
        .map_err(|e| e.to_string())?;
    for (id, content) in [
        (&instance.root_storage, "rootdir"),
        (&instance.home_storage, "rootdir"),
        (&cfg.template_storage, "vztmpl"),
    ] {
        let entry = storages
            .as_array()
            .and_then(|items| items.iter().find(|s| s["storage"] == *id))
            .ok_or_else(|| format!("{}: storage {id} unavailable", instance.node))?;
        if entry["active"].as_u64() != Some(1) || !text(entry, "content").split(',').any(|c| c == content) {
            return Err(format!("Storage {id} is inactive or lacks {content}"));
        }
        if id == &instance.root_storage
            && entry["avail"]
                .as_u64()
                .is_none_or(|n| n < u64::from(instance.root_gib) * 1024 * 1024 * 1024)
        {
            return Err("Insufficient verified free space for the new root".to_owned());
        }
    }
    if !backup.is_empty()
        && storages.as_array().is_none_or(|items| {
            !items.iter().any(|s| {
                s["storage"] == backup
                    && s["active"].as_u64() == Some(1)
                    && text(s, "content").split(',').any(|c| c == "backup")
            })
        })
    {
        return Err("Selected backup storage is unavailable on this node".to_owned());
    }
    let bridge = instance
        .network
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .find_map(|p| p.strip_prefix("bridge="))
        .ok_or("Original network has no bridge")?;
    // vmbr bridges use the localnetwork SDN zone; other bridges are checked by PVE itself.
    if bridge.starts_with("vmbr") && !permit(&permissions, &format!("/sdn/zones/localnetwork/{bridge}"), "SDN.Use") {
        return Err(format!("Deploy token lacks SDN.Use for {bridge}"));
    }
    Ok(())
}

pub async fn plan(
    pve: &impl Pve,
    config: &DeployConfig,
    source: &Source,
    record: &Deployment,
    choice: &Choice,
) -> Plan {
    let recovery = choice.action == Some(Action::Recovery);
    let saved = record.operation.as_ref().and_then(|o| o.replacement.clone());
    let mut plan = Plan {
        record: record.clone(),
        journal: saved.unwrap_or_else(|| Journal {
            id: uuid::Uuid::new_v4().to_string(),
            commit: source.commit.clone(),
            blob: source.blob.clone(),
            image: source.image(&choice.version),
            digest: String::new(),
            platform: String::new(),
            template_storage: config.template_storage.clone(),
            backup_storage: choice.backup_storage.clone(),
            targets: Vec::new(),
        }),
        configs: Vec::new(),
        problems: Vec::new(),
        lines: Vec::new(),
        recovery,
    };
    let result = review(pve, config, source, record, choice, &mut plan).await;
    if let Err(error) = result {
        plan.problems.push(error);
    }
    for target in &plan.journal.targets {
        let before = &target.before;
        let after = &target.after;
        let current = before
            .release
            .as_ref()
            .map_or(record.image_version.as_str(), |release| release.version.as_str());
        let selected = plan
            .journal
            .image
            .rsplit_once(':')
            .map_or("unknown", |(_, version)| version);
        let state = if target.running { "running" } else { "stopped" };
        let autostart = if target.onboot { "on" } else { "off" };
        plan.lines.extend([
            format!("{} · {}", before.hostname, before.node),
            String::new(),
            "Changes".to_owned(),
            format!(
                "  Image      {current} → {selected}{}",
                if current == selected { " (reinstall)" } else { "" }
            ),
            format!("  Container  {} → {}", before.ctid, after.ctid),
            format!("  Root       new {} GiB on {}", after.root_gib, after.root_storage),
            String::new(),
            "Preserved".to_owned(),
            format!(
                "  Home       {} · {} GiB · /home/app",
                before.home_volume, before.home_gib
            ),
            format!("  Network    {} · MAC {}", before.address, before.mac),
            format!("  Name/pool  {} · {}", before.hostname, before.pool),
            format!("  Tags       {}", before.tags.join(";")),
            format!("  State      {state} → {state} · autostart {autostart}"),
            String::new(),
            format!("After success: manually delete old CT {}.", before.ctid),
            String::new(),
        ]);
    }
    if !plan.journal.backup_storage.is_empty() {
        plan.lines.push(format!(
            "Backup: stopped container and home → {}",
            plan.journal.backup_storage
        ));
    }
    if source.manifest.healthcheck.is_none() {
        plan.lines
            .push("Health check: not provided by this service.".to_owned());
    } else if plan.journal.targets.iter().any(|target| target.running) {
        plan.lines.push("Health check: automatic after update.".to_owned());
    } else {
        plan.lines
            .push("Health check: skipped because the service stays stopped.".to_owned());
    }
    plan
}

async fn review(
    pve: &impl Pve,
    config: &DeployConfig,
    source: &Source,
    record: &Deployment,
    choice: &Choice,
    plan: &mut Plan,
) -> Result<(), String> {
    if source.repository != record.repository
        || source.manifest.id != record.service
        || source.manifest.image_repository != record.image_repository
    {
        return Err("Service or image repository identity changed".to_owned());
    }
    if plan.recovery {
        if record.operation.as_ref().and_then(|o| o.replacement.as_ref()).is_none() {
            return Err("No interrupted API replacement is recorded".to_owned());
        }
        if source.commit != plan.journal.commit || source.blob != plan.journal.blob {
            return Err("Recovery requires the exact reviewed manifest revision".to_owned());
        }
        for target in &plan.journal.targets {
            if !choice.ctids.contains(&target.before.ctid) && !choice.ctids.contains(&target.after.ctid) {
                return Err("Select every instance of the interrupted replacement for recovery".to_owned());
            }
            let (_, original) = read(pve, &target.before).await?;
            plan.configs.push((target.before.ctid, original));
            if let Some((found, cfg)) = deploy::config_of(pve, target.after.ctid).await? {
                if found.node != target.after.node {
                    return Err("Replacement moved to another node".to_owned());
                }
                plan.configs.push((target.after.ctid, cfg));
            }
        }
        return Ok(());
    }
    if record.operation.is_some() {
        return Err("An unfinished operation requires Recovery first".to_owned());
    }
    if !source.manifest.multi_node
        && record
            .instances
            .iter()
            .filter(|i| i.stage != registry::Stage::Removed)
            .count()
            > 1
    {
        return Err("The manifest permits only one logical service instance".to_owned());
    }
    if choice.ctids.is_empty() || choice.ctids.iter().collect::<BTreeSet<_>>().len() != choice.ctids.len() {
        return Err("Select distinct registered instances".to_owned());
    }
    if hs_core::registry::version(&choice.version).is_none() {
        return Err("Choose a published numeric SemVer release".to_owned());
    }
    let release = hs_core::registry::resolve(&plan.journal.image).await?;
    plan.journal.digest = release.digest;
    plan.journal.platform = release.platform;
    let mut reserved = BTreeSet::new();
    for id in &choice.ctids {
        let instance = record
            .instances
            .iter()
            .find(|i| i.ctid == *id)
            .ok_or("Selected CT does not belong to this service")?;
        if !matches!(instance.stage, registry::Stage::Ready | registry::Stage::Created) {
            return Err("Only installed instances can be replaced".to_owned());
        }
        let (found, cfg) = read(pve, instance).await?;
        lifecycle::verify(source, record, instance, &cfg)?;
        safe_config(pve, instance, &cfg).await?;
        let mut before = instance.clone();
        before.release = Some(instance.release.clone().unwrap_or_else(|| Release {
            version: record.image_version.clone(),
            digest: record.image_digest.clone(),
            commit: record.commit.clone(),
            manifest_blob: record.manifest_blob.clone(),
        }));
        before.tags = tags(&cfg);
        before.network = Some(text(&cfg, "net0").to_owned());
        before.nameserver = cfg["nameserver"].as_str().map(str::to_owned);
        before.cores = cfg["cores"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or("Actual cores unavailable")?;
        before.memory_mib = cfg["memory"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or("Actual memory unavailable")?;
        before.root_gib = lifecycle::size_gib(&cfg, "rootfs")
            .filter(|v| v.fract() == 0.0 && *v <= 16384.0)
            .ok_or("Actual root capacity unavailable")? as u32;
        before.home_gib = lifecycle::size_gib(&cfg, "mp0")
            .filter(|v| v.fract() == 0.0 && *v <= 16384.0)
            .ok_or("Actual home capacity unavailable")? as u32;
        preflight(pve, config, &before, &choice.backup_storage).await?;
        if !choice.backup_storage.is_empty() && !text(&cfg, "mp0").split(',').any(|v| v == "backup=1") {
            return Err("Home must have backup=1 for the selected backup".to_owned());
        }
        let mut candidate = before.ctid.checked_add(1).ok_or("No replacement CTID available")?;
        let mut next = None;
        for _ in 0..1024 {
            if !reserved.contains(&candidate)
                && !record
                    .instances
                    .iter()
                    .any(|i| i.ctid == candidate || i.retired.iter().any(|r| r.ctid == candidate))
            {
                // PVE checks all guests, including those outside the token's pools.
                match pve.get("/cluster/nextid", &[("vmid", &candidate.to_string())]).await {
                    Ok(value) if value.as_str().and_then(|s| s.parse::<u32>().ok()) == Some(candidate) => {
                        next = Some(candidate);
                        break;
                    }
                    Err(hs_pve::PveError::Api { status: 400, .. }) => {}
                    Err(error) => return Err(format!("Cannot verify a free replacement CTID: {error}")),
                    _ => return Err("PVE returned an unexpected CTID response".to_owned()),
                }
            }
            candidate = candidate.checked_add(1).ok_or("No replacement CTID available")?;
        }
        let ctid = next.ok_or("No verified free replacement CTID; cluster visibility may be unavailable")?;
        reserved.insert(ctid);
        let mut after = before.clone();
        after.ctid = ctid;
        after.root_volume.clear();
        after.home_volume.clear();
        after.verified = None;
        after.release = Some(Release {
            version: choice.version.clone(),
            digest: plan.journal.digest.clone(),
            commit: source.commit.clone(),
            manifest_blob: source.blob.clone(),
        });
        after.desired = if found.status == "running" {
            Desired::Running
        } else {
            Desired::Stopped
        };
        plan.journal.targets.push(Target {
            before,
            after,
            running: found.status == "running",
            onboot: cfg["onboot"].as_u64() == Some(1),
            source_description: text(&cfg, "description").to_owned(),
            home_mount: text(&cfg, "mp0").to_owned(),
            network: text(&cfg, "net0").to_owned(),
            step: Step::Pull,
            task: None,
            transfer_task: None,
        });
        plan.configs.push((*id, cfg));
    }
    Ok(())
}

fn save(path: &Path, source: &Source, journal: &Journal) -> Result<(), String> {
    registry::update(path, |registry| {
        let record = registry
            .deployments
            .iter_mut()
            .find(|d| d.repository == source.repository && d.service == source.manifest.id)
            .ok_or("Service record disappeared")?;
        if record
            .operation
            .as_ref()
            .and_then(|o| o.replacement.as_ref())
            .is_some_and(|j| j.id != journal.id)
        {
            return Err("Another operation replaced the journal".to_owned());
        }
        record.operation = Some(Operation {
            kind: "replace".to_owned(),
            step: journal
                .targets
                .iter()
                .map(|t| format!("{}→{} {:?}", t.before.ctid, t.after.ctid, t.step))
                .collect::<Vec<_>>()
                .join(", "),
            started: record
                .operation
                .as_ref()
                .map_or_else(deploy::stamp, |o| o.started.clone()),
            lifecycle: None,
            replacement: Some(journal.clone()),
        });
        Ok(())
    })
}

async fn job(
    pve: &impl Pve,
    path: &Path,
    source: &Source,
    journal: &mut Journal,
    index: usize,
    endpoint: &str,
    form: &[(&str, &str)],
) -> Result<(), String> {
    if journal.targets[index].task.is_none() {
        // The intended transition is already durable before the API request.
        save(path, source, journal)?;
        let task = pve.post(endpoint, form).await.map_err(|e| e.to_string())?;
        let task = task
            .as_str()
            .ok_or("Proxmox returned no task ID; reconcile before retrying")?;
        if !task.starts_with(&format!("UPID:{}:", journal.targets[index].before.node)) {
            return Err("Task belongs to a different node".to_owned());
        }
        journal.targets[index].task = Some(task.to_owned());
        save(path, source, journal)?;
    }
    let task = journal.targets[index].task.as_ref().ok_or("Task journal disappeared")?;
    lifecycle::task(pve, &json!(task)).await
}

async fn put(pve: &impl Pve, instance: &Instance, cfg: &Value, fields: Vec<(String, String)>) -> Result<(), String> {
    let mut form: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let digest = cfg["digest"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("PVE configuration digest unavailable")?;
    form.push(("digest", digest));
    pve.put(&format!("/nodes/{}/lxc/{}/config", instance.node, instance.ctid), &form)
        .await
        .map_err(|_| {
            format!(
                "CT {} configuration update rejected; runtime values omitted",
                instance.ctid
            )
        })?;
    Ok(())
}

fn advance(journal: &mut Journal, index: usize, step: Step) {
    journal.targets[index].step = step;
    journal.targets[index].task = None;
}

fn original(target: &Target, cfg: &Value) -> Result<(), String> {
    if volume(cfg, "rootfs") != target.before.root_volume
        || text(cfg, "hostname") != target.before.hostname
        || text(cfg, "description") != target.source_description
        || tags(cfg) != target.before.tags
        || cfg["unprivileged"].as_u64() != Some(1)
    {
        return Err("Original container identity/root/preserved metadata changed; no automatic repair".to_owned());
    }
    Ok(())
}

fn replacement(source: &Source, journal: &Journal, target: &Target, cfg: &Value) -> Result<(), String> {
    if text(cfg, "description") != description(source, journal, target)
        || text(cfg, "hostname") != target.before.hostname
        || cfg["unprivileged"].as_u64() != Some(1)
        || text(cfg, "arch") != "amd64"
        || (!target.after.root_volume.is_empty() && volume(cfg, "rootfs") != target.after.root_volume)
    {
        return Err("Replacement container ownership, architecture or root changed; no automatic adoption".to_owned());
    }
    if !matches!(target.step, Step::Pull | Step::Create | Step::Prepare) && tags(cfg) != target.before.tags {
        return Err("Replacement tags changed".to_owned());
    }
    let root = volume(cfg, "rootfs");
    if !root.starts_with(&format!("{}:", target.after.root_storage))
        || !root
            .split(':')
            .nth(1)
            .is_some_and(|v| v.contains(&format!("-{}-", target.after.ctid)))
        || root == target.before.root_volume
    {
        return Err("Replacement root does not belong to the replacement container on the reviewed storage".to_owned());
    }
    Ok(())
}

/// A successful API reassignment plus these observations proves the transfer;
/// a renamed volume is never treated as a newly allocated home.
fn moved_home(target: &Target, original_config: &Value, replacement_config: &Value) -> Result<String, String> {
    if original_config.get("mp0").is_some()
        || volume(original_config, "rootfs") != target.before.root_volume
        || volume(replacement_config, "rootfs") != target.after.root_volume
    {
        return Err("Home still attached to the original container or a root changed during transfer".to_owned());
    }
    let mount = text(replacement_config, "mp0");
    let home = volume(replacement_config, "mp0");
    let options = |s: &str| s.split(',').skip(1).map(str::to_owned).collect::<BTreeSet<_>>();
    if options(mount) != options(&target.home_mount)
        || !options(mount).contains("mp=/home/app")
        || !home.starts_with(&format!("{}:", target.before.home_storage))
        || !home
            .split(':')
            .nth(1)
            .is_some_and(|v| v.contains(&format!("-{}-", target.after.ctid)))
        || home == target.after.root_volume
        || (!target.after.home_volume.is_empty() && home != target.after.home_volume)
    {
        return Err("Transferred home identity/storage/mount options differ from the journal".to_owned());
    }
    if original_config.as_object().is_none_or(|fields| {
        fields.iter().any(|(k, v)| {
            (k.starts_with("mp") || k.starts_with("unused"))
                && v.as_str().is_some_and(|v| {
                    v.split(',')
                        .next()
                        .is_some_and(|id| id == target.before.home_volume || id == home)
                })
        })
    }) {
        return Err("Original container still references the data volume".to_owned());
    }
    Ok(home)
}

async fn image_matches(journal: &Journal) -> Result<(), String> {
    let current = hs_core::registry::resolve(&journal.image).await?;
    if current.digest != journal.digest || current.platform != journal.platform {
        return Err("Image tag changed since review; refuse replacement".to_owned());
    }
    Ok(())
}

pub async fn apply(
    pve: &impl Pve,
    path: &Path,
    source: &Source,
    plan: &Plan,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<Vec<String>, String> {
    if !plan.problems.is_empty() {
        return Err("Resolve the review problems first".to_owned());
    }
    let _lock = registry::operation_lock(path)?;
    let registry = registry::load(path)?;
    if registry.deployment(&source.repository, &source.manifest.id) != Some(&plan.record) {
        return Err("Registry changed since review; review again".to_owned());
    }
    for (ctid, before) in &plan.configs {
        let (_, current) = deploy::config_of(pve, *ctid).await?.ok_or("Reviewed CT disappeared")?;
        if &current != before {
            return Err(format!("CT {ctid} changed since review"));
        }
    }
    if !plan.recovery {
        for target in &plan.journal.targets {
            let (state, _) = read(pve, &target.before).await?;
            if (state.status == "running") != target.running {
                return Err("Original power state changed since review".to_owned());
            }
            if registry
                .deployments
                .iter()
                .flat_map(|d| &d.instances)
                .any(|i| i.ctid == target.after.ctid || i.retired.iter().any(|r| r.ctid == target.after.ctid))
            {
                return Err("Replacement CTID is reserved by another registry instance".to_owned());
            }
            if registry
                .deployments
                .iter()
                .filter_map(|d| d.operation.as_ref()?.replacement.as_ref())
                .any(|j| j.targets.iter().any(|t| t.after.ctid == target.after.ctid))
            {
                return Err("Replacement CTID is reserved by an unfinished operation".to_owned());
            }
            let id = target.after.ctid.to_string();
            pve.get("/cluster/nextid", &[("vmid", &id)])
                .await
                .map_err(|_| "Replacement CTID is no longer free")?;
        }
    }
    let mut journal = plan.journal.clone();
    save(path, source, &journal)?;
    let mut lines = Vec::new();
    for index in 0..journal.targets.len() {
        if let Err(error) = run(pve, path, source, &mut journal, index, say).await {
            return Err(format!(
                "{error}\nUpdate paused. Reopen Update image to review and resume."
            ));
        }
        let target = &journal.targets[index];
        let line = format!(
            "{}: {} · CT {} → {} · home {} → {} · pool {} · tags {}",
            target.before.node,
            target.before.hostname,
            target.before.ctid,
            target.after.ctid,
            target.before.home_volume,
            target.after.home_volume,
            target.after.pool,
            target.after.tags.join(";")
        );
        say(&line);
        lines.push(line);
    }
    registry::update(path, |registry| {
        let record = registry
            .deployments
            .iter_mut()
            .find(|d| d.repository == source.repository && d.service == source.manifest.id)
            .ok_or("Service disappeared")?;
        if record.operation.as_ref().and_then(|o| o.replacement.as_ref()) != Some(&journal) {
            return Err("Operation journal changed before completion".to_owned());
        }
        record.operation = None;
        Ok(())
    })?;
    Ok(lines)
}

async fn run(
    pve: &impl Pve,
    path: &Path,
    source: &Source,
    journal: &mut Journal,
    index: usize,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<(), String> {
    loop {
        let target = journal.targets[index].clone();
        say(&format!(
            "{}: CT {} → {} · {:?}",
            target.before.node, target.before.ctid, target.after.ctid, target.step
        ));
        let base = format!("/nodes/{}/lxc/{}", target.before.node, target.before.ctid);
        let next_base = format!("/nodes/{}/lxc/{}", target.after.node, target.after.ctid);
        let (old_state, old) = read(pve, &target.before).await?;
        original(&target, &old)?;
        let filename = format!("hs-{}-{}", journal.id, target.after.ctid);
        let template = format!("{}:vztmpl/{filename}.tar", journal.template_storage);
        match target.step {
            Step::Pull => {
                image_matches(journal).await?;
                let endpoint = format!(
                    "/nodes/{}/storage/{}/oci-registry-pull",
                    target.before.node, journal.template_storage
                );
                let image = journal.image.clone();
                job(
                    pve,
                    path,
                    source,
                    journal,
                    index,
                    &endpoint,
                    &[("reference", &image), ("filename", &filename)],
                )
                .await?;
                image_matches(journal).await?;
                advance(journal, index, Step::Create);
            }
            Step::Create => {
                if target.task.is_none() {
                    if deploy::config_of(pve, target.after.ctid).await?.is_some() {
                        return Err(
                            "Replacement container already exists without a recorded create task; inspect it before retrying".to_owned(),
                        );
                    }
                    let id = target.after.ctid.to_string();
                    pve.get("/cluster/nextid", &[("vmid", &id)])
                        .await
                        .map_err(|_| "Replacement container CTID is occupied")?;
                    let spec = node::NewContainer {
                        node: &target.after.node,
                        ctid: target.after.ctid,
                        hostname: &target.after.hostname,
                        ostemplate: &template,
                        pool: &target.after.pool,
                        storage: &target.after.root_storage,
                        disk_gib: target.after.root_gib,
                        memory_mib: target.after.memory_mib,
                        cores: target.after.cores,
                        net0: None,
                        onboot: false,
                        nameserver: target.after.nameserver.clone(),
                        description: description(source, journal, &target),
                        tag: None,
                        mp0: None,
                    };
                    let upid = node::create_task(pve, &spec).await?;
                    journal.targets[index].task = Some(upid.as_str().ok_or("No create task ID")?.to_owned());
                    save(path, source, journal)?;
                }
                lifecycle::task(pve, &json!(journal.targets[index].task)).await?;
                let (state, cfg) = read(pve, &target.after).await?;
                replacement(source, journal, &target, &cfg)?;
                if state.status != "stopped"
                    || cfg.get("mp0").is_some()
                    || cfg.get("net0").is_some()
                    || cfg["onboot"].as_u64() == Some(1)
                {
                    return Err("Replacement container must be stopped without network, home or autostart".to_owned());
                }
                journal.targets[index].after.root_volume = volume(&cfg, "rootfs");
                advance(journal, index, Step::Prepare);
            }
            Step::Prepare => {
                lifecycle::verify(
                    source,
                    &registry::load(path)?
                        .deployment(&source.repository, &source.manifest.id)
                        .ok_or("Service disappeared")?
                        .clone(),
                    &target.before,
                    &old,
                )?;
                let (state, cfg) = read(pve, &target.after).await?;
                replacement(source, journal, &target, &cfg)?;
                if state.status != "stopped"
                    || cfg.get("mp0").is_some()
                    || cfg.get("net0").is_some()
                    || cfg["onboot"].as_u64() == Some(1)
                {
                    return Err("Prepared replacement container is no longer isolated".to_owned());
                }
                let previous_env = node::parse_env(text(&old, "env"));
                let env = node::merge_env(&node::parse_env(text(&cfg, "env")), &previous_env, &[]);
                let mut fields = vec![
                    ("env".to_owned(), env),
                    ("tags".to_owned(), target.before.tags.join(";")),
                    ("onboot".to_owned(), "0".to_owned()),
                ];
                for key in [
                    "hostname",
                    "cores",
                    "memory",
                    "swap",
                    "cpulimit",
                    "cpuunits",
                    "nameserver",
                    "searchdomain",
                    "startup",
                    "features",
                    "timezone",
                    "console",
                    "tty",
                    "cmode",
                ] {
                    if let Some(value) = old.get(key) {
                        fields.push((
                            key.to_owned(),
                            value.as_str().map_or_else(|| value.to_string(), str::to_owned),
                        ));
                    }
                }
                put(pve, &target.after, &cfg, fields).await?;
                let (_, prepared) = read(pve, &target.after).await?;
                if tags(&prepared) != target.before.tags
                    || text(&prepared, "env")
                        != node::merge_env(&node::parse_env(text(&cfg, "env")), &previous_env, &[])
                {
                    return Err("Replacement container did not retain original tags/runtime environment".to_owned());
                }
                safe_config(pve, &target.before, &old).await?;
                safe_config(pve, &target.after, &prepared).await?;
                advance(journal, index, Step::Shutdown);
            }
            Step::Shutdown => {
                if text(&old, "mp0") != target.home_mount || text(&old, "net0") != target.network {
                    return Err("Original container home or network changed before shutdown".to_owned());
                }
                let (state, cfg) = read(pve, &target.after).await?;
                replacement(source, journal, &target, &cfg)?;
                if state.status != "stopped"
                    || cfg.get("mp0").is_some()
                    || cfg.get("net0").is_some()
                    || cfg["onboot"].as_u64() == Some(1)
                {
                    return Err(
                        "Replacement container must remain isolated before shutting down the original container"
                            .to_owned(),
                    );
                }
                safe_config(pve, &target.after, &cfg).await?;
                safe_config(pve, &target.before, &old).await?;
                put(pve, &target.before, &old, vec![("onboot".to_owned(), "0".to_owned())]).await?;
                if old_state.status == "running" || target.task.is_some() {
                    job(
                        pve,
                        path,
                        source,
                        journal,
                        index,
                        &format!("{base}/status/shutdown"),
                        &[("timeout", "60"), ("forceStop", "0")],
                    )
                    .await?;
                }
                if read(pve, &target.before).await?.0.status != "stopped" {
                    return Err("Original container did not stop gracefully; no forced stop".to_owned());
                }
                advance(journal, index, Step::Backup);
            }
            Step::Backup => {
                if old_state.status != "stopped" || text(&old, "mp0") != target.home_mount {
                    return Err("Original container is not stopped with its original home".to_owned());
                }
                if !journal.backup_storage.is_empty() {
                    let storage = journal.backup_storage.clone();
                    job(
                        pve,
                        path,
                        source,
                        journal,
                        index,
                        &format!("/nodes/{}/vzdump", target.before.node),
                        &[
                            ("vmid", &target.before.ctid.to_string()),
                            ("storage", &storage),
                            ("mode", "stop"),
                            ("compress", "zstd"),
                        ],
                    )
                    .await?;
                }
                advance(journal, index, Step::MoveHome);
            }
            Step::MoveHome => {
                let (state, cfg) = read(pve, &target.after).await?;
                replacement(source, journal, &target, &cfg)?;
                if old_state.status != "stopped"
                    || state.status != "stopped"
                    || old["onboot"].as_u64() == Some(1)
                    || cfg["onboot"].as_u64() == Some(1)
                {
                    return Err("Both containers must remain stopped with autostart disabled".to_owned());
                }
                if target.task.is_none() {
                    if text(&old, "mp0") != target.home_mount || cfg.get("mp0").is_some() {
                        return Err("Home transfer has no recorded task and unexpected mounts; inspect storage, do not allocate another home".to_owned());
                    }
                    safe_config(pve, &target.before, &old).await?;
                    safe_config(pve, &target.after, &cfg).await?;
                }
                job(
                    pve,
                    path,
                    source,
                    journal,
                    index,
                    &format!("{base}/move_volume"),
                    &[
                        ("volume", "mp0"),
                        ("target-vmid", &target.after.ctid.to_string()),
                        ("target-volume", "mp0"),
                        ("digest", text(&old, "digest")),
                        ("target-digest", text(&cfg, "digest")),
                    ],
                )
                .await?;
                let (_, original_config) = read(pve, &target.before).await?;
                let (_, replacement_config) = read(pve, &target.after).await?;
                let home = moved_home(&target, &original_config, &replacement_config)?;
                journal.targets[index].after.home_volume = home.clone();
                journal.targets[index].transfer_task = journal.targets[index].task.clone();
                say(&format!(
                    "Verified home transfer {} → {home}",
                    target.before.home_volume
                ));
                advance(journal, index, Step::Disconnect);
            }
            Step::Disconnect | Step::Connect | Step::Start | Step::Register | Step::Complete => {
                let (state, cfg) = read(pve, &target.after).await?;
                replacement(source, journal, &target, &cfg)?;
                moved_home(&target, &old, &cfg)?;
                if tags(&cfg) != target.before.tags
                    || old_state.status != "stopped"
                    || old["onboot"].as_u64() == Some(1)
                {
                    return Err(
                        "Original container must remain retired and the replacement container must retain all tags"
                            .to_owned(),
                    );
                }
                match target.step {
                    Step::Disconnect => {
                        if state.status != "stopped" || cfg.get("net0").is_some() {
                            return Err("Replacement container started or acquired a network before cutover".to_owned());
                        }
                        if old.get("net0").is_some() {
                            if text(&old, "net0") != target.network {
                                return Err("Original container network changed".to_owned());
                            }
                            put(
                                pve,
                                &target.before,
                                &old,
                                vec![("delete".to_owned(), "net0".to_owned())],
                            )
                            .await?;
                        }
                        advance(journal, index, Step::Connect);
                    }
                    Step::Connect => {
                        if old.get("net0").is_some() || state.status != "stopped" {
                            return Err("Network cutover is not isolated".to_owned());
                        }
                        if cfg.get("net0").is_some() && text(&cfg, "net0") != target.network {
                            return Err("Replacement container has a different network".to_owned());
                        }
                        if cfg.get("net0").is_none() {
                            put(
                                pve,
                                &target.after,
                                &cfg,
                                vec![("net0".to_owned(), target.network.clone())],
                            )
                            .await?;
                        }
                        advance(journal, index, Step::Start);
                    }
                    Step::Start => {
                        if old.get("net0").is_some() || text(&cfg, "net0") != target.network {
                            return Err("Original network was not transferred exactly".to_owned());
                        }
                        let record = registry::load(path)?;
                        lifecycle::verify(
                            source,
                            record
                                .deployment(&source.repository, &source.manifest.id)
                                .ok_or("Service disappeared")?,
                            &target.after,
                            &cfg,
                        )?;
                        safe_config(pve, &target.after, &cfg).await?;
                        if target.running {
                            if state.status != "running" || target.task.is_some() {
                                job(
                                    pve,
                                    path,
                                    source,
                                    journal,
                                    index,
                                    &format!("{next_base}/status/start"),
                                    &[],
                                )
                                .await?;
                            }
                            say(&deploy::healthy(&source.manifest, &target.before.address).await?);
                            journal.targets[index].after.verified =
                                source.manifest.healthcheck.as_ref().map(|_| deploy::stamp());
                        } else if state.status != "stopped" {
                            return Err("Previously stopped service unexpectedly started".to_owned());
                        }
                        let (_, current) = read(pve, &target.after).await?;
                        put(
                            pve,
                            &target.after,
                            &current,
                            vec![("onboot".to_owned(), if target.onboot { "1" } else { "0" }.to_owned())],
                        )
                        .await?;
                        advance(journal, index, Step::Register);
                    }
                    Step::Register => {
                        if old.get("net0").is_some()
                            || text(&cfg, "net0") != target.network
                            || (state.status == "running") != target.running
                            || (cfg["onboot"].as_u64() == Some(1)) != target.onboot
                        {
                            return Err("Final power/network/autostart state differs from the reviewed plan".to_owned());
                        }
                        let mut after = target.after.clone();
                        after.stage = registry::Stage::Ready;
                        if !after.retired.iter().any(|r| r.ctid == target.before.ctid) {
                            after.retired.push(Retired {
                                ctid: target.before.ctid,
                                root_volume: target.before.root_volume.clone(),
                                release: target
                                    .before
                                    .release
                                    .clone()
                                    .ok_or("Previous image release identity missing")?,
                                home_before: target.before.home_volume.clone(),
                                home_after: target.after.home_volume.clone(),
                                transfer_task: target
                                    .transfer_task
                                    .clone()
                                    .ok_or("Home transfer task evidence missing")?,
                            });
                        }
                        registry::update(path, |registry| {
                            let record = registry
                                .deployments
                                .iter_mut()
                                .find(|d| d.repository == source.repository && d.service == source.manifest.id)
                                .ok_or("Service disappeared")?;
                            let slot = record
                                .instances
                                .iter_mut()
                                .find(|i| i.ctid == target.before.ctid || i.ctid == target.after.ctid)
                                .ok_or("Instance disappeared")?;
                            if slot.node != target.before.node
                                || slot.address != target.before.address
                                || slot.mac != target.before.mac
                            {
                                return Err("Logical instance identity changed".to_owned());
                            }
                            *slot = after.clone();
                            Ok(())
                        })?;
                        journal.targets[index].after = after;
                        advance(journal, index, Step::Complete);
                    }
                    Step::Complete => {
                        if old.get("net0").is_some()
                            || text(&cfg, "net0") != target.network
                            || (state.status == "running") != target.running
                            || (cfg["onboot"].as_u64() == Some(1)) != target.onboot
                        {
                            return Err(
                                "Completed replacement drifted; inspect it before clearing the journal".to_owned()
                            );
                        }
                        return Ok(());
                    }
                    _ => unreachable!(),
                }
            }
        }
        save(path, source, journal)?;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn target() -> Target {
        let before: Instance = serde_json::from_value(json!({
            "node":"pve3", "ctid":120, "hostname":"example", "address":"192.0.2.10",
            "mac":"02:00:00:00:00:10", "pool":"homestack-service", "tags":["homestack-app", "private"],
            "cores":1, "memory_mib":512, "root_storage":"local-lvm", "root_gib":4,
            "home_storage":"local-lvm", "home_gib":8, "root_volume":"local-lvm:vm-120-disk-0",
            "home_volume":"local-lvm:vm-120-disk-1", "stage":"ready", "desired":"running"
        }))
        .unwrap();
        let mut after = before.clone();
        after.ctid = 121;
        after.root_volume = "local-lvm:vm-121-disk-0".to_owned();
        after.home_volume.clear();
        Target {
            before,
            after,
            running: true,
            onboot: true,
            source_description: "original".to_owned(),
            home_mount: "local-lvm:vm-120-disk-1,mp=/home/app,backup=1,size=8G".to_owned(),
            network: String::new(),
            step: Step::MoveHome,
            task: None,
            transfer_task: None,
        }
    }

    #[test]
    fn home_transfer_rejects_wrong_mount_storage_root_and_remaining_reference() {
        let target = target();
        let original_config = json!({"rootfs":"local-lvm:vm-120-disk-0,size=4G"});
        let replacement_config = json!({"rootfs":"local-lvm:vm-121-disk-0,size=4G", "mp0":"local-lvm:vm-121-disk-1,mp=/home/app,backup=1,size=8G"});
        assert_eq!(
            moved_home(&target, &original_config, &replacement_config).unwrap(),
            "local-lvm:vm-121-disk-1"
        );
        for mount in [
            "other:vm-121-disk-1,mp=/home/app,backup=1,size=8G",
            "local-lvm:vm-122-disk-1,mp=/home/app,backup=1,size=8G",
            "local-lvm:vm-121-disk-0,mp=/home/app,backup=1,size=8G",
            "local-lvm:vm-121-disk-1,mp=/home/other,backup=1,size=8G",
            "local-lvm:vm-121-disk-1,mp=/home/app,backup=1,size=4G",
        ] {
            let mut wrong = replacement_config.clone();
            wrong["mp0"] = json!(mount);
            assert!(moved_home(&target, &original_config, &wrong).is_err());
        }
        for key in ["mp0", "unused0"] {
            for home in ["local-lvm:vm-120-disk-1", "local-lvm:vm-121-disk-1"] {
                let mut referenced = original_config.clone();
                referenced[key] = json!(home);
                assert!(moved_home(&target, &referenced, &replacement_config).is_err());
            }
        }
        let mut changed_root = original_config;
        changed_root["rootfs"] = json!("local-lvm:vm-120-disk-2,size=4G");
        assert!(moved_home(&target, &changed_root, &replacement_config).is_err());
    }

    #[test]
    fn recorded_home_identity_and_original_ownership_must_match() {
        let mut target = target();
        target.after.home_volume = "local-lvm:vm-121-disk-2".to_owned();
        let original_config = json!({"rootfs":"local-lvm:vm-120-disk-0,size=4G", "hostname":"example", "description":"original", "tags":"private;homestack-app", "unprivileged":1});
        let replacement_config = json!({"rootfs":"local-lvm:vm-121-disk-0,size=4G", "mp0":"local-lvm:vm-121-disk-1,mp=/home/app,backup=1,size=8G"});
        assert!(moved_home(&target, &original_config, &replacement_config).is_err());
        assert!(original(&target, &original_config).is_ok());
        for (field, value) in [
            ("rootfs", "local-lvm:vm-999-disk-0"),
            ("hostname", "foreign"),
            ("description", "foreign"),
            ("tags", "homestack-app"),
        ] {
            let mut wrong = original_config.clone();
            wrong[field] = json!(value);
            assert!(original(&target, &wrong).is_err());
        }
    }

    #[test]
    fn permissions_distinguish_presence_from_propagation() {
        let perms = json!({"/vms/120":{"VM.Config.Disk":0}, "/storage":{"Datastore.AllocateSpace":0}, "/nodes":{"Sys.AccessNetwork":1}});
        assert!(permit(&perms, "/vms/120", "VM.Config.Disk"));
        assert!(!permit(&perms, "/vms/121", "VM.Config.Disk"));
        assert!(!permit(&perms, "/storage/local-lvm", "Datastore.AllocateSpace"));
        assert!(permit(&perms, "/nodes/pve3", "Sys.AccessNetwork"));
        assert!(!permit(&perms, "/vms/120", "VM.Console"));
    }

    #[tokio::test]
    async fn interrupted_transfer_of_stopped_service_resumes_without_start_or_disk_allocation() {
        use super::super::manifest::{Manifest, StateModel};
        use hs_pve::PveError;
        use std::sync::Mutex;

        struct Api(Mutex<(Value, Value)>);
        impl Pve for Api {
            async fn get(&self, path: &str, _: &[(&str, &str)]) -> Result<Value, PveError> {
                let pair = self.0.lock().unwrap();
                Ok(match path {
                    "/cluster/resources" => json!([
                        // Cached inventory is deliberately stale/incomplete.
                        {"type":"lxc", "vmid":120, "node":"pve3", "status":"running"},
                        {"type":"lxc", "vmid":121, "node":"pve3"}
                    ]),
                    "/pools/homestack-service" => json!({"members":[
                        {"type":"lxc", "vmid":120, "node":"pve3"},
                        {"type":"lxc", "vmid":121, "node":"pve3"}
                    ]}),
                    "/nodes/pve3/lxc/120/config" => pair.0.clone(),
                    "/nodes/pve3/lxc/121/config" => pair.1.clone(),
                    p if p.contains("/tasks/") && p.ends_with("/status") => {
                        json!({"status":"stopped", "exitstatus":"OK"})
                    }
                    p if p.ends_with("/status/current") => json!({"status":"stopped", "ha":{"managed":0}}),
                    p if p.ends_with("/snapshot") => json!([{"name":"current"}]),
                    p if p.ends_with("/firewall/options") => json!({}),
                    p if p.ends_with("/pending")
                        || p.ends_with("/replication")
                        || p.contains("/firewall/")
                        || p.ends_with("/log") =>
                    {
                        json!([])
                    }
                    _ => panic!("Unexpected read: {path}"),
                })
            }
            async fn post(&self, path: &str, _: &[(&str, &str)]) -> Result<Value, PveError> {
                panic!("Recovery must not recreate disks, repeat the move or start the stopped service: {path}")
            }
            async fn delete(&self, path: &str, _: &[(&str, &str)]) -> Result<Value, PveError> {
                panic!("Recovery must not destroy anything: {path}")
            }
            async fn put(&self, path: &str, fields: &[(&str, &str)]) -> Result<Value, PveError> {
                let mut pair = self.0.lock().unwrap();
                let cfg = match path {
                    "/nodes/pve3/lxc/120/config" => &mut pair.0,
                    "/nodes/pve3/lxc/121/config" => &mut pair.1,
                    _ => panic!("Unexpected mutation: {path}"),
                };
                assert_eq!(
                    fields.iter().find(|(k, _)| *k == "digest").unwrap().1,
                    text(cfg, "digest")
                );
                for (key, value) in fields {
                    match *key {
                        "delete" => {
                            assert_eq!(*value, "net0");
                            cfg.as_object_mut().unwrap().remove("net0");
                        }
                        "net0" => cfg["net0"] = json!(value),
                        "onboot" => cfg["onboot"] = json!(value.parse::<u32>().unwrap()),
                        "digest" => {}
                        _ => panic!("Unexpected configuration change: {key}"),
                    }
                }
                Ok(Value::Null)
            }
        }

        let source = Source {
            repository: "owner/repo".to_owned(),
            branch: "main".to_owned(),
            commit: "commit".to_owned(),
            path: ".homestack/example.yaml".to_owned(),
            blob: "blob".to_owned(),
            manifest: Manifest {
                id: "example".to_owned(),
                name: "Example".to_owned(),
                image_repository: "ghcr.io/example/service".to_owned(),
                ports: vec![],
                healthcheck: None,
                directories: vec!["data".to_owned()],
                mappings: vec![],
                multi_node: false,
                state_model: StateModel::Local,
            },
        };
        let mut target = target();
        target.running = false;
        target.after.desired = Desired::Stopped;
        target.before.release = Some(Release {
            version: "1.0.0".to_owned(),
            digest: "old-digest".to_owned(),
            commit: "old-commit".to_owned(),
            manifest_blob: "old-blob".to_owned(),
        });
        target.after.release = Some(Release {
            version: "1.0.1".to_owned(),
            digest: "new-digest".to_owned(),
            commit: source.commit.clone(),
            manifest_blob: source.blob.clone(),
        });
        target.network = "name=eth0,bridge=vmbr0,ip=192.0.2.10/24,hwaddr=02:00:00:00:00:10,type=veth".to_owned();
        target.task = Some("UPID:pve3:00000001:00000001:00000001:move_volume:120-0:operator@pve:".to_owned());
        let mut journal = Journal {
            id: "operation".to_owned(),
            commit: source.commit.clone(),
            blob: source.blob.clone(),
            image: source.image("1.0.1"),
            digest: "new-digest".to_owned(),
            platform: "platform-digest".to_owned(),
            template_storage: "local".to_owned(),
            backup_storage: String::new(),
            targets: vec![target.clone()],
        };
        let api = Api(Mutex::new((
            json!({"rootfs":"local-lvm:vm-120-disk-0,size=4G", "hostname":"example", "description":"original", "tags":"homestack-app;private", "unprivileged":1, "onboot":0, "net0":target.network, "digest":"original_config"}),
            json!({"rootfs":"local-lvm:vm-121-disk-0,size=4G", "mp0":"local-lvm:vm-121-disk-1,mp=/home/app,backup=1,size=8G", "hostname":"example", "description":description(&source,&journal,&target), "tags":"homestack-app;private", "unprivileged":1, "arch":"amd64", "onboot":0, "digest":"replacement_config"}),
        )));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("services.toml");
        let record = Deployment {
            repository: source.repository.clone(),
            service: source.manifest.id.clone(),
            git_ref: source.branch.clone(),
            commit: source.commit.clone(),
            manifest_path: source.path.clone(),
            manifest_blob: source.blob.clone(),
            image_repository: source.manifest.image_repository.clone(),
            image_version: "1.0.0".to_owned(),
            image_digest: "old-digest".to_owned(),
            environment: vec![],
            instances: vec![target.before.clone()],
            operation: None,
        };
        registry::update(&path, |r| {
            r.deployments.push(record);
            Ok(())
        })
        .unwrap();
        save(&path, &source, &journal).unwrap();
        run(&api, &path, &source, &mut journal, 0, &|_| {}).await.unwrap();
        let registry = registry::load(&path).unwrap();
        let active = &registry.deployments[0].instances[0];
        assert_eq!(active.ctid, 121);
        assert_eq!(active.desired, Desired::Stopped);
        assert!(active.verified.is_none());
        assert_eq!(active.home_volume, "local-lvm:vm-121-disk-1");
        assert_eq!(active.retired[0].root_volume, "local-lvm:vm-120-disk-0");
        assert!(active.retired[0].transfer_task.starts_with("UPID:pve3:"));
        let pair = api.0.lock().unwrap();
        assert!(pair.0.get("mp0").is_none() && pair.0.get("net0").is_none());
        assert_eq!(pair.0["onboot"], 0);
        assert_eq!(pair.1["onboot"], 1);
        assert_eq!(pair.1["net0"], target.network);
    }
}
