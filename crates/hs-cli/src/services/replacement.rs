//! Update a registered service's image under the same CTID, through the
//! Proxmox API only. The home volume waits in a temporary holder container
//! while the container is made again (`hs_deploy::recreate`). Each step is
//! journaled; an interrupted update resumes from what Proxmox shows.

use std::collections::BTreeSet;
use std::path::Path;

use hs_core::pve::Pve;
use hs_deploy::{config::DeployConfig, node, recreate};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::deploy::{self, Source};
use super::lifecycle::{self, Action, Choice};
use super::registry::{self, Deployment, Desired, Instance, Operation, Release};

/// Configuration the new container keeps from the old one, besides what
/// the registry records.
const KEEP: &[&str] = &[
    "env",
    "swap",
    "cpulimit",
    "cpuunits",
    "searchdomain",
    "startup",
    "features",
    "timezone",
    "console",
    "tty",
    "cmode",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    Pull,
    Backup,
    Replace,
    Configure,
    Start,
    Register,
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
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The update's description; its `update` line tells the new container from the old one.
fn description(source: &Source, journal: &Journal) -> String {
    format!(
        "{}\nupdate {}",
        deploy::description(source, &journal.image, &journal.digest),
        journal.id
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
            format!("{} · {} · CT {}", before.hostname, before.node, before.ctid),
            String::new(),
            "Changes".to_owned(),
            format!(
                "  Image      {current} → {selected}{}",
                if current == selected { " (reinstall)" } else { "" }
            ),
            format!("  Root       new {} GiB on {}", before.root_gib, before.root_storage),
            String::new(),
            "Preserved".to_owned(),
            format!("  Container  CT {}", before.ctid),
            format!(
                "  Home       {} · {} GiB · /home/app",
                before.home_volume, before.home_gib
            ),
            format!("  Network    {} · MAC {}", before.address, before.mac),
            format!("  Name/pool  {} · {}", before.hostname, before.pool),
            format!("  Tags       {}", before.tags.join(";")),
            format!("  State      {state} → {state} · autostart {autostart}"),
            String::new(),
        ]);
        if plan.recovery {
            plan.lines.push(format!("Resume from step {:?}.", target.step));
            plan.lines.push(String::new());
        }
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
            return Err("No interrupted image update is recorded".to_owned());
        }
        if source.commit != plan.journal.commit || source.blob != plan.journal.blob {
            return Err("Resuming requires the exact reviewed manifest revision".to_owned());
        }
        if plan
            .journal
            .targets
            .iter()
            .any(|target| !choice.ctids.contains(&target.before.ctid))
        {
            return Err("Select every instance of the interrupted update to resume it".to_owned());
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
    for id in &choice.ctids {
        let instance = record
            .instances
            .iter()
            .find(|i| i.ctid == *id)
            .ok_or("Selected CT does not belong to this service")?;
        if !matches!(instance.stage, registry::Stage::Ready | registry::Stage::Created) {
            return Err("Only installed instances can be updated".to_owned());
        }
        let (found, cfg) = read(pve, instance).await?;
        lifecycle::verify(source, record, instance, &cfg)?;
        safe_config(pve, instance, &cfg).await?;
        recreate::mounts(&cfg)?;
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
        let mut after = before.clone();
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
                .map(|t| format!("CT {} {:?}", t.before.ctid, t.step))
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
        || text(cfg, "mp0") != target.home_mount
        || text(cfg, "net0") != target.network
    {
        return Err("Original container changed since review; no automatic repair".to_owned());
    }
    Ok(())
}

/// The new container: this update's description, its own root on the
/// reviewed storage and the original home under the same mount options.
fn replaced(source: &Source, journal: &Journal, target: &Target, cfg: &Value) -> Result<(String, String), String> {
    if text(cfg, "description").trim_end() != description(source, journal).trim_end() {
        return Err(format!("CT {}: not made by this update", target.before.ctid));
    }
    if text(cfg, "hostname") != target.before.hostname || cfg["unprivileged"].as_u64() != Some(1) {
        return Err(format!("CT {}: hostname or privilege changed", target.before.ctid));
    }
    let root = volume(cfg, "rootfs");
    if !root.starts_with(&format!("{}:", target.before.root_storage))
        || !root.contains(&format!("-{}-", target.before.ctid))
    {
        return Err(format!(
            "CT {}: root is not a new volume of its own",
            target.before.ctid
        ));
    }
    let options = |mount: &str| mount.split_once(',').map(|(_, rest)| rest.to_owned());
    let home = volume(cfg, "mp0");
    if options(text(cfg, "mp0")) != options(&target.home_mount)
        || !home.starts_with(&format!("{}:", target.before.home_storage))
        || !home.contains(&format!("-{}-", target.before.ctid))
        || home == root
    {
        return Err(format!(
            "CT {}: home is missing or its storage/mount options differ",
            target.before.ctid
        ));
    }
    if !node::same_net(text(cfg, "net0"), &target.network)
        || node::extract_hwaddr(text(cfg, "net0")) != node::extract_hwaddr(&target.network)
    {
        return Err(format!("CT {}: network differs from the original", target.before.ctid));
    }
    Ok((root, home))
}

async fn image_matches(journal: &Journal) -> Result<(), String> {
    let current = hs_core::registry::resolve(&journal.image).await?;
    if current.digest != journal.digest || current.platform != journal.platform {
        return Err("Image tag changed since review; refuse the update".to_owned());
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
            "{}: {} · CT {} · {} · home {}",
            target.before.node, target.before.hostname, target.before.ctid, journal.image, target.after.home_volume
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
        let instance = &target.before;
        say(&format!("{}: CT {} · {:?}", instance.node, instance.ctid, target.step));
        let base = format!("/nodes/{}/lxc/{}", instance.node, instance.ctid);
        match target.step {
            Step::Pull => {
                let (_, old) = read(pve, instance).await?;
                original(&target, &old)?;
                image_matches(journal).await?;
                let endpoint = format!(
                    "/nodes/{}/storage/{}/oci-registry-pull",
                    instance.node, journal.template_storage
                );
                let image = journal.image.clone();
                let filename = format!("hs-{}-{}", journal.id, instance.ctid);
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
                advance(journal, index, Step::Backup);
            }
            Step::Backup => {
                if !journal.backup_storage.is_empty() {
                    if target.task.is_none() {
                        let (_, old) = read(pve, instance).await?;
                        original(&target, &old)?;
                    }
                    let storage = journal.backup_storage.clone();
                    job(
                        pve,
                        path,
                        source,
                        journal,
                        index,
                        &format!("/nodes/{}/vzdump", instance.node),
                        &[
                            ("vmid", &instance.ctid.to_string()),
                            ("storage", &storage),
                            ("mode", "stop"),
                            ("compress", "zstd"),
                        ],
                    )
                    .await?;
                }
                advance(journal, index, Step::Replace);
            }
            Step::Replace => {
                if let Some((_, cfg)) = deploy::config_of(pve, instance.ctid).await?
                    && text(&cfg, "description") == target.source_description
                {
                    original(&target, &cfg)?;
                    safe_config(pve, instance, &cfg).await?;
                }
                let template = format!(
                    "{}:vztmpl/hs-{}-{}.tar",
                    journal.template_storage, journal.id, instance.ctid
                );
                let spec = node::NewContainer {
                    node: &instance.node,
                    ctid: instance.ctid,
                    hostname: &instance.hostname,
                    ostemplate: &template,
                    pool: &instance.pool,
                    storage: &instance.root_storage,
                    disk_gib: instance.root_gib,
                    memory_mib: instance.memory_mib,
                    cores: instance.cores,
                    net0: Some(target.network.clone()),
                    onboot: false,
                    nameserver: instance.nameserver.clone(),
                    description: description(source, journal),
                    tag: None,
                    mp0: None,
                };
                recreate::replace(
                    pve,
                    &recreate::Replace {
                        spec,
                        graceful: true,
                        keep: KEEP,
                    },
                    &|line| say(line),
                )
                .await?;
                let (_, cfg) = read(pve, instance).await?;
                let (root, home) = replaced(source, journal, &target, &cfg)?;
                say(&format!("Home {} → {home}", instance.home_volume));
                journal.targets[index].after.root_volume = root;
                journal.targets[index].after.home_volume = home;
                advance(journal, index, Step::Configure);
            }
            Step::Configure => {
                let (_, cfg) = read(pve, instance).await?;
                replaced(source, journal, &target, &cfg)?;
                if tags(&cfg) != instance.tags {
                    put(pve, instance, &cfg, vec![("tags".to_owned(), instance.tags.join(";"))]).await?;
                }
                let (_, cfg) = read(pve, instance).await?;
                let record = registry::load(path)?;
                lifecycle::verify(
                    source,
                    record
                        .deployment(&source.repository, &source.manifest.id)
                        .ok_or("Service disappeared")?,
                    &target.after,
                    &cfg,
                )?;
                if tags(&cfg) != instance.tags {
                    return Err("The new container did not keep the original tags".to_owned());
                }
                advance(journal, index, Step::Start);
            }
            Step::Start => {
                let (state, cfg) = read(pve, instance).await?;
                replaced(source, journal, &target, &cfg)?;
                if target.running {
                    if state.status != "running" || target.task.is_some() {
                        job(pve, path, source, journal, index, &format!("{base}/status/start"), &[]).await?;
                    }
                    say(&deploy::healthy(&source.manifest, &instance.address).await?);
                    journal.targets[index].after.verified =
                        source.manifest.healthcheck.as_ref().map(|_| deploy::stamp());
                } else if state.status != "stopped" {
                    return Err("Previously stopped service unexpectedly started".to_owned());
                }
                let (_, current) = read(pve, instance).await?;
                put(
                    pve,
                    instance,
                    &current,
                    vec![("onboot".to_owned(), if target.onboot { "1" } else { "0" }.to_owned())],
                )
                .await?;
                advance(journal, index, Step::Register);
            }
            Step::Register => {
                let (state, cfg) = read(pve, instance).await?;
                replaced(source, journal, &target, &cfg)?;
                if (state.status == "running") != target.running || (cfg["onboot"].as_u64() == Some(1)) != target.onboot
                {
                    return Err("Final power/autostart state differs from the reviewed plan".to_owned());
                }
                let mut after = target.after.clone();
                after.stage = registry::Stage::Ready;
                registry::update(path, |registry| {
                    let slot = registry
                        .deployments
                        .iter_mut()
                        .find(|d| d.repository == source.repository && d.service == source.manifest.id)
                        .ok_or("Service disappeared")?
                        .instances
                        .iter_mut()
                        .find(|i| i.ctid == instance.ctid)
                        .ok_or("Instance disappeared")?;
                    if slot.node != instance.node || slot.address != instance.address || slot.mac != instance.mac {
                        return Err("Logical instance identity changed".to_owned());
                    }
                    *slot = after.clone();
                    Ok(())
                })?;
                journal.targets[index].after = after;
                save(path, source, journal)?;
                return Ok(());
            }
        }
        save(path, source, journal)?;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn permissions_distinguish_presence_from_propagation() {
        let perms = json!({"/vms/120":{"VM.Config.Disk":0}, "/storage":{"Datastore.AllocateSpace":0}, "/nodes":{"Sys.AccessNetwork":1}});
        assert!(permit(&perms, "/vms/120", "VM.Config.Disk"));
        assert!(!permit(&perms, "/vms/121", "VM.Config.Disk"));
        assert!(!permit(&perms, "/storage/local-lvm", "Datastore.AllocateSpace"));
        assert!(permit(&perms, "/nodes/pve3", "Sys.AccessNetwork"));
        assert!(!permit(&perms, "/vms/120", "VM.Console"));
    }
}
