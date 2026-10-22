//! Manual lifecycle for common-registry services. A reviewed plan captures
//! identity and live configuration; an operation journal precedes mutations.
//! Legacy API/Site records never enter this module.
use std::path::Path;
use std::time::Duration;

use hs_core::pve::Pve;
use hs_core::tasks::{NoObserver, TaskWait, wait_task};
use hs_deploy::{config::DeployConfig, node};
use hs_install::Root;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::deploy::{self, Source};
use super::registry::{self, Deployment, Desired, Instance, Operation, Release, Stage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Update,
    Resize,
    Uninstall,
    DeleteData,
    Recovery,
    Reconnect,
}
impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Self::Update => "Update",
            Self::Resize => "Resize",
            Self::Uninstall => "Uninstall (retain home)",
            Self::DeleteData => "Delete data permanently",
            Self::Recovery => "Recovery / reconcile",
            Self::Reconnect => "Reconnect retained home",
        }
    }
}

/// Only non-secret operation intent is persisted. Configuration values and
/// credentials remain in Proxmox or the existing protected credential stores.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub action: Action,
    pub commit: String,
    pub blob: String,
    pub image: String,
    pub digest: String,
    pub platform: String,
    pub backup_storage: String,
    pub root_gib: u32,
    pub home_gib: u32,
    pub targets: Vec<Target>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub ctid: u32,
    pub was_running: bool,
    pub root_before: String,
    pub step: String,
    pub backup_task: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Choice {
    pub action: Option<Action>,
    pub ctids: Vec<u32>,
    pub version: String,
    pub root_gib: String,
    pub home_gib: String,
    pub backup_storage: String,
    pub environment: String,
    /// Exact confirmation for the separately selected delete-data action.
    pub confirmation: String,
}

#[derive(Debug, Clone)]
pub struct Observed {
    pub instance: Instance,
    pub config: Value,
    pub running: bool,
}
#[derive(Debug, Clone)]
pub struct Plan {
    pub record: Deployment,
    pub journal: Journal,
    pub observed: Vec<Observed>,
    pub environment: Vec<(String, String)>,
    pub problems: Vec<String>,
    pub lines: Vec<String>,
    pub replacement: Option<super::replacement::Plan>,
}

fn text<'a>(config: &'a Value, key: &str) -> &'a str {
    config[key].as_str().unwrap_or_default()
}
fn vol(config: &Value, key: &str) -> String {
    text(config, key).split(',').next().unwrap_or_default().to_owned()
}
fn has_line(config: &Value, wanted: &str) -> bool {
    text(config, "description").lines().any(|line| line.trim() == wanted)
}

/// Fail closed on ownership, disk, address, MAC, image or runtime drift.
/// Pool membership alone never authorizes a mutation.
pub fn verify(source: &Source, record: &Deployment, instance: &Instance, config: &Value) -> Result<(), String> {
    let (root, home, mac) = deploy::verify(instance, config)?;
    if root != instance.root_volume || home != instance.home_volume || !mac.eq_ignore_ascii_case(&instance.mac) {
        return Err("root/home volume or MAC differs from the registry".to_owned());
    }
    if !deploy::made_by(source, config) || text(config, "hostname") != instance.hostname {
        return Err("service ownership or hostname differs".to_owned());
    }
    let ip = text(config, "net0").split(',').find_map(|p| p.strip_prefix("ip="));
    if ip.and_then(|p| p.split('/').next()) != Some(instance.address.as_str()) {
        return Err("IP address differs from the registry".to_owned());
    }
    for tag in &instance.tags {
        if !text(config, "tags").split([';', ',']).any(|t| t == tag) {
            return Err(format!("ownership tag {tag} is missing"));
        }
    }
    let (version, digest) = instance.release.as_ref().map_or(
        (record.image_version.as_str(), record.image_digest.as_str()),
        |release| (release.version.as_str(), release.digest.as_str()),
    );
    if !has_line(config, &format!("image {}:{version}", record.image_repository))
        || !has_line(config, &format!("digest {digest}"))
    {
        return Err("image metadata differs from the registry".to_owned());
    }
    if config.get("lock").is_some() || config["protection"].as_u64() == Some(1) {
        return Err("container is locked or protected".to_owned());
    }
    if config
        .as_object()
        .is_none_or(|fields| fields.keys().any(|k| k.starts_with("mp") && k != "mp0"))
    {
        return Err("additional mount points require manual review".to_owned());
    }
    if text(config, "mp0").split(',').any(|p| p == "ro=1" || p == "shared=1") {
        return Err("read-only or shared home requires manual review".to_owned());
    }
    Ok(())
}

pub(super) async fn task(pve: &impl Pve, upid: &Value) -> Result<(), String> {
    let id = upid.as_str().ok_or("Proxmox returned no task ID")?;
    let result = wait_task(pve, id, &TaskWait::up_to(Duration::from_secs(1800)), &NoObserver)
        .await
        .map_err(|e| e.to_string())?;
    if result.ok() { Ok(()) } else { Err(result.exitstatus) }
}

async fn volume_exists(pve: &impl Pve, instance: &Instance) -> Result<bool, String> {
    let content = pve
        .get(
            &format!("/nodes/{}/storage/{}/content", instance.node, instance.home_storage),
            &[],
        )
        .await
        .map_err(|e| format!("home inventory unavailable: {e}"))?;
    let entries = content.as_array().ok_or("home inventory is not a list")?;
    Ok(entries
        .iter()
        .any(|v| v["volid"].as_str() == Some(instance.home_volume.as_str())))
}

async fn permissions(pve: &impl Pve, instance: &Instance, action: Action) -> Result<(), String> {
    let list = pve.get("/access/permissions", &[]).await.map_err(|e| e.to_string())?;
    let vm = format!("/vms/{}", instance.ctid);
    let pool = format!("/pool/{}", instance.pool);
    let storage = format!("/storage/{}", instance.root_storage);
    let required: &[&str] = match action {
        Action::Resize => &["VM.Config.Disk"],
        Action::Uninstall => &["VM.Allocate", "VM.PowerMgmt"],
        Action::Update => &["VM.PowerMgmt", "VM.Config.Disk", "VM.Config.Options"],
        Action::Reconnect => &["VM.Allocate", "VM.Config.Disk", "VM.PowerMgmt"],
        _ => &["VM.Audit"],
    };
    for permission in required {
        if ![vm.as_str(), pool.as_str(), "/vms", "/"]
            .iter()
            .any(|path| list[*path][*permission].as_u64() == Some(1))
        {
            return Err(format!("deploy token lacks {permission} for CT {}", instance.ctid));
        }
    }
    if matches!(action, Action::Update | Action::Resize | Action::Reconnect)
        && ![storage.as_str(), "/storage", "/"]
            .iter()
            .any(|path| list[*path]["Datastore.AllocateSpace"].as_u64() == Some(1))
    {
        return Err(format!(
            "deploy token lacks Datastore.AllocateSpace on {}",
            instance.root_storage
        ));
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
    let action = choice.action.unwrap_or(Action::Recovery);
    let mut result = Plan {
        record: record.clone(),
        journal: Journal {
            action,
            commit: source.commit.clone(),
            blob: source.blob.clone(),
            image: source.image(&choice.version),
            digest: String::new(),
            platform: String::new(),
            backup_storage: choice.backup_storage.clone(),
            root_gib: 0,
            home_gib: 0,
            targets: Vec::new(),
        },
        observed: Vec::new(),
        environment: Vec::new(),
        problems: Vec::new(),
        lines: Vec::new(),
        replacement: None,
    };
    if action == Action::Update
        || (action == Action::Recovery && record.operation.as_ref().is_some_and(|o| o.replacement.is_some()))
    {
        let replacement = super::replacement::plan(pve, config, source, record, choice).await;
        result.problems.clone_from(&replacement.problems);
        result.lines.clone_from(&replacement.lines);
        result.replacement = Some(replacement);
        return result;
    }
    if choice.action.is_none() {
        result.problems.push("Choose an operation".to_owned());
    }
    if source.repository != record.repository
        || source.manifest.id != record.service
        || source.manifest.image_repository != record.image_repository
    {
        result
            .problems
            .push("Source identity/image repository changed; inspect before continuing".to_owned());
    }
    if record.operation.is_some() && action != Action::Recovery {
        result
            .problems
            .push("An unfinished operation requires Recovery first".to_owned());
    }
    if choice.ctids.is_empty() {
        result
            .problems
            .push("Select at least one registered instance".to_owned());
    }
    let mut ids = std::collections::BTreeSet::new();
    for id in &choice.ctids {
        if !ids.insert(*id) {
            result.problems.push(format!("CT {id} selected twice"));
        }
        let Some(instance) = record.instances.iter().find(|i| i.ctid == *id) else {
            result
                .problems
                .push(format!("CT {id} is not registered to this service"));
            continue;
        };
        let checked = inspect(pve, source, record, instance, action).await;
        match checked {
            Ok(observed) => {
                result.lines.push(format!(
                    "{} · CT {id} · {} · {} · MAC {}",
                    instance.node, instance.hostname, instance.address, instance.mac
                ));
                result.lines.push(format!(
                    "  root {} · home {} at /home/app",
                    instance.root_volume, instance.home_volume
                ));
                result.lines.push(format!(
                    "  state {} · measured home usage unknown",
                    if observed.running { "running" } else { "stopped" }
                ));
                result.journal.targets.push(Target {
                    ctid: *id,
                    was_running: observed.running,
                    root_before: instance.root_volume.clone(),
                    step: "reviewed".to_owned(),
                    backup_task: None,
                });
                result.observed.push(observed);
            }
            Err(error) => result.problems.push(format!("CT {id}: {error}")),
        }
    }
    if action == Action::Reconnect {
        if hs_core::registry::version(&choice.version).is_none() {
            result
                .problems
                .push("Choose a published numeric SemVer image".to_owned());
        } else {
            match hs_core::registry::resolve(&result.journal.image).await {
                Ok(release) => {
                    result.journal.digest = release.digest;
                    result.journal.platform = release.platform;
                }
                Err(error) => result.problems.push(error),
            }
        }
        if !source.manifest.multi_node && record.instances.iter().filter(|i| i.stage != Stage::Removed).count() > 1 {
            result
                .problems
                .push("The manifest permits only one independent instance".to_owned());
        }
    }
    if action == Action::Resize {
        let parse = |value: &str| value.parse::<u32>().ok().filter(|v| *v > 0 && *v <= 16384);
        match (parse(&choice.root_gib), parse(&choice.home_gib)) {
            (Some(root), Some(home)) => {
                result.journal.root_gib = root;
                result.journal.home_gib = home;
                for o in &result.observed {
                    if !o.running {
                        result.problems.push(format!(
                            "CT {} must be running to verify filesystem growth",
                            o.instance.ctid
                        ));
                    }
                    let root_now = size_gib(&o.config, "rootfs");
                    let home_now = size_gib(&o.config, "mp0");
                    if root_now.is_none_or(|n| f64::from(root) < n) || home_now.is_none_or(|n| f64::from(home) < n) {
                        result.problems.push(format!(
                            "CT {}: cannot shrink, or actual capacity is unknown",
                            o.instance.ctid
                        ));
                    }
                }
                result.lines.push(format!(
                    "Grow only: root {root} GiB, home {home} GiB. Backing volume identities stay unchanged."
                ));
            }
            _ => result
                .problems
                .push("Give whole positive root and home sizes in GiB".to_owned()),
        }
    }
    if action == Action::DeleteData {
        if choice.confirmation != format!("DELETE {}/{}", record.repository, record.service) {
            result.problems.push(format!(
                "Type DELETE {}/{} to authorize permanent data deletion",
                record.repository, record.service
            ));
        }
        result
            .lines
            .push("PERMANENT: remove only the selected retained home volumes. This cannot be undone.".to_owned());
    }
    if action == Action::Uninstall {
        result.lines.push("Stop and remove selected CTs and their disposable roots. Retain exact home volumes and registry associations. No purge, no unused-volume sweep.".to_owned());
        for o in &result.observed {
            if o.config
                .as_object()
                .is_some_and(|fields| fields.keys().any(|k| k.starts_with("unused")))
            {
                result.problems.push(format!(
                    "CT {}: retained unused volumes require review before removal",
                    o.instance.ctid
                ));
            }
        }
    }
    if action == Action::Reconnect {
        match deploy::environment(&choice.environment) {
            Ok(env) => {
                for name in &record.environment {
                    if !env.iter().any(|(key, value)| key == name && !value.is_empty()) {
                        result.problems.push(format!(
                            "Supply required non-secret environment {name}; values are not saved in the registry"
                        ));
                    }
                }
                result.environment = env;
            }
            Err(error) => result.problems.push(error),
        }
        result.lines.push("Recreate only explicitly selected absent containers; attach the recorded home without allocating or formatting another home. Start and verify health.".to_owned());
    }
    if action == Action::Recovery {
        result.lines.push("Reconcile only recorded identities. Never adopt unknown CTs or allocate a replacement home. Resume verified health/power state; ambiguous intermediate states stop for review.".to_owned());
        if let Some(operation) = &record.operation {
            result
                .lines
                .push(format!("Unfinished {}: {}", operation.kind, operation.step));
            if let Some(journal) = &operation.lifecycle {
                let pending: Vec<u32> = journal.targets.iter().map(|t| t.ctid).collect();
                if pending.iter().any(|id| !choice.ctids.contains(id)) {
                    result
                        .problems
                        .push("Select all instances of the unfinished operation for reconciliation".to_owned());
                }
            }
        }
    }
    result
        .lines
        .push(format!("Then: {}", deploy::health_text(&source.manifest)));
    result
}

pub fn size_gib(config: &Value, key: &str) -> Option<f64> {
    let size = text(config, key).split(',').find_map(|v| v.strip_prefix("size="))?;
    let (number, factor) = match size.as_bytes().last()? {
        b'T' => (&size[..size.len() - 1], 1024.0),
        b'G' => (&size[..size.len() - 1], 1.0),
        b'M' => (&size[..size.len() - 1], 1.0 / 1024.0),
        _ => (size, 1.0 / (1024.0 * 1024.0 * 1024.0)),
    };
    let value = number.parse::<f64>().ok()? * factor;
    (value.is_finite() && value > 0.0).then_some(value)
}

async fn inspect(
    pve: &impl Pve,
    source: &Source,
    record: &Deployment,
    instance: &Instance,
    action: Action,
) -> Result<Observed, String> {
    permissions(pve, instance, action).await?;
    if instance.home_volume.is_empty() || instance.root_volume.is_empty() || instance.mac.is_empty() {
        return Err("registry lacks verified disk/MAC identities; no automatic adoption".to_owned());
    }
    let exists = volume_exists(pve, instance).await?;
    if !exists && !(action == Action::Recovery && instance.stage == Stage::Removed) {
        return Err("recorded home volume is missing; no replacement will be allocated".to_owned());
    }
    let found = deploy::config_of(pve, instance.ctid).await?;
    if matches!(action, Action::DeleteData | Action::Reconnect) {
        if instance.stage != Stage::Retained || found.is_some() {
            return Err("only retained homes with absent CTs qualify; a visible CT is never overwritten".to_owned());
        }
        let id = instance.ctid.to_string();
        pve.get("/cluster/nextid", &[("vmid", &id)])
            .await
            .map_err(|_| "CTID is occupied or cluster visibility unavailable")?;
        return Ok(Observed {
            instance: instance.clone(),
            config: Value::Null,
            running: false,
        });
    }
    let Some((found, config)) = found else {
        if action == Action::Recovery {
            return Ok(Observed {
                instance: instance.clone(),
                config: Value::Null,
                running: false,
            });
        }
        return Err("container is missing; use Recovery".to_owned());
    };
    if found.node != instance.node {
        return Err(format!("node changed to {}", found.node));
    }
    if !matches!(found.status.as_str(), "running" | "stopped") {
        return Err("unknown power state".to_owned());
    }
    let membership = pve
        .get(&format!("/pools/{}", instance.pool), &[])
        .await
        .map_err(|e| format!("pool ownership unavailable: {e}"))?;
    if membership["members"].as_array().is_none_or(|members| {
        !members.iter().any(|m| {
            m["vmid"].as_u64() == Some(u64::from(instance.ctid)) && m["type"] == "lxc" && m["node"] == instance.node
        })
    }) {
        return Err("container no longer belongs to the recorded pool".to_owned());
    }
    // Recovery is allowed to inspect drift, but apply still verifies before
    // repairing any specific, journal-proven transition.
    if action != Action::Recovery {
        verify(source, record, instance, &config)?;
    }
    let pending = pve
        .get(&format!("/nodes/{}/lxc/{}/pending", instance.node, instance.ctid), &[])
        .await
        .map_err(|e| e.to_string())?;
    if pending.as_array().is_none_or(|items| {
        items
            .iter()
            .any(|item| item.get("pending").is_some() || item["delete"].as_u64() == Some(1))
    }) {
        return Err("pending configuration needs review".to_owned());
    }
    Ok(Observed {
        instance: instance.clone(),
        config,
        running: found.status == "running",
    })
}

fn save(path: &Path, source: &Source, f: impl FnOnce(&mut Deployment) -> Result<(), String>) -> Result<(), String> {
    registry::update(path, |r| {
        f(r.deployments
            .iter_mut()
            .find(|d| d.repository == source.repository && d.service == source.manifest.id)
            .ok_or("deployment disappeared")?)
    })
}
fn checkpoint(path: &Path, source: &Source, id: u32, step: &str) -> Result<(), String> {
    save(path, source, |record| {
        let operation = record.operation.as_mut().ok_or("operation disappeared")?;
        operation.step = format!("CT {id}: {step}");
        if let Some(journal) = &mut operation.lifecycle {
            journal
                .targets
                .iter_mut()
                .find(|t| t.ctid == id)
                .ok_or("target disappeared")?
                .step = step.to_owned();
        }
        Ok(())
    })
}

fn root_call(root: &mut dyn Root, request: &Value) -> Result<String, String> {
    let command = format!(
        "perl -e {}",
        hs_install::shell::quote(include_str!("lifecycle-root.pl"))
    );
    let output = root
        .run_input(&command, &request.to_string())
        .map_err(|e| e.to_string())?;
    if output.code != 0 {
        return Err(format!("service storage operation refused: {}", output.detail()));
    }
    Ok(output.stdout)
}

/// Apply only the exact reviewed record and configurations. The worker holds
/// a profile operation lock for the entire run (including SSH and task waits).
pub async fn apply(
    pve: &impl Pve,
    path: &Path,
    config: &DeployConfig,
    source: &Source,
    plan: &Plan,
    roots: &mut std::collections::BTreeMap<String, Box<dyn Root>>,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<Vec<String>, String> {
    if let Some(replacement) = &plan.replacement {
        return super::replacement::apply(pve, path, source, replacement, say).await;
    }
    if plan.journal.action == Action::Update || plan.record.operation.as_ref().is_some_and(|o| o.replacement.is_some())
    {
        return Err("An API replacement plan is required".to_owned());
    }
    if !plan.problems.is_empty() {
        return Err("Review the plan's problems first".to_owned());
    }
    let lock = registry::operation_lock(path)?;
    let current = registry::load(path)?;
    if current.deployment(&source.repository, &source.manifest.id) != Some(&plan.record) {
        return Err("Registry changed since review; review again".to_owned());
    }
    for old in &plan.observed {
        let now = inspect(pve, source, &plan.record, &old.instance, plan.journal.action).await?;
        if now.config != old.config || now.running != old.running {
            return Err(format!("CT {} changed since review", old.instance.ctid));
        }
        if plan.journal.action != Action::Recovery || roots.contains_key(&old.instance.node) {
            let root = roots
                .get_mut(&old.instance.node)
                .ok_or("verified root SSH access required for each selected node")?;
            let access = hs_install::probe(root.as_mut()).map_err(|e| e.to_string())?;
            if access.host != old.instance.node {
                return Err("root SSH leads to a different node".to_owned());
            }
        }
    }
    if plan.journal.action != Action::Recovery {
        save(path, source, |record| {
            record.operation = Some(Operation {
                kind: plan.journal.action.label().to_owned(),
                step: "review accepted".to_owned(),
                started: deploy::stamp(),
                lifecycle: Some(plan.journal.clone()),
                replacement: None,
            });
            Ok(())
        })?;
    }
    let mut lines = Vec::new();
    for old in &plan.observed {
        let result = apply_instance(pve, path, config, source, plan, old, roots, say).await;
        match result {
            Ok(line) => {
                say(&line);
                lines.push(line);
            }
            Err(error) => {
                lines.push(format!("CT {}: {error}", old.instance.ctid));
                say(&lines.join("\n"));
                return Err(format!(
                    "{error}. Partial progress remains journaled; use Recovery. No cleanup was attempted."
                ));
            }
        }
    }
    save(path, source, |record| {
        record.operation = None;
        Ok(())
    })?;
    drop(lock);
    Ok(lines)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn apply_instance(
    pve: &impl Pve,
    path: &Path,
    config: &DeployConfig,
    source: &Source,
    plan: &Plan,
    observed: &Observed,
    roots: &mut std::collections::BTreeMap<String, Box<dyn Root>>,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<String, String> {
    let mut instance = observed.instance.clone();
    let id = instance.ctid;
    let base = format!("/nodes/{}/lxc/{id}", instance.node);
    let action = plan.journal.action;
    if action == Action::Recovery {
        return recover(pve, path, source, &plan.record, &instance, roots, say).await;
    }
    if action == Action::Resize {
        if !observed.running {
            return Err(
                "Resize requires a running container to verify its mounted filesystem and observed capacity".to_owned(),
            );
        }
        let root = roots
            .get_mut(&instance.node)
            .ok_or("verified root SSH required for filesystem verification")?;
        say(&root_call(
            root.as_mut(),
            &json!({"action":"verify-runtime", "ctid":id, "node":instance.node, "home":instance.home_volume, "mp0":text(&observed.config,"mp0")}),
        )?);
        for (disk, target) in [("rootfs", plan.journal.root_gib), ("mp0", plan.journal.home_gib)] {
            let (_, cfg) = deploy::config_of(pve, id).await?.ok_or("CT disappeared")?;
            verify(source, &plan.record, &instance, &cfg)?;
            let actual = size_gib(&cfg, disk).ok_or("measured capacity unknown")?;
            if f64::from(target) > actual {
                checkpoint(path, source, id, &format!("grow {disk} to {target} GiB"))?;
                let upid = pve
                    .put(
                        &format!("{base}/resize"),
                        &[
                            ("disk", disk),
                            ("size", &format!("{target}G")),
                            ("digest", text(&cfg, "digest")),
                        ],
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                // Some Proxmox versions return a task, others return null.
                if !upid.is_null() {
                    task(pve, &upid).await?;
                }
                let (_, cfg) = deploy::config_of(pve, id).await?.ok_or("CT disappeared")?;
                verify(source, &plan.record, &instance, &cfg)?;
                if size_gib(&cfg, disk).is_none_or(|size| size < f64::from(target)) {
                    return Err("growth has not been observed; inspect backing storage/filesystem".to_owned());
                }
            }
        }
        let (_, cfg) = deploy::config_of(pve, id).await?.ok_or("CT disappeared")?;
        let measured = root_call(
            root.as_mut(),
            &json!({"action":"verify-runtime", "ctid":id, "node":instance.node, "home":instance.home_volume, "mp0":text(&cfg,"mp0")}),
        )?;
        say(&measured);
        // Filesystem metadata consumes space: require observable growth over
        // the previous capacity rather than equating df capacity with LV size.
        if filesystem_kib(&measured, "/").is_none() || filesystem_kib(&measured, "/home/app").is_none() {
            return Err(
                "Guest filesystem capacities could not be measured; inspect growth before reconciling".to_owned(),
            );
        }
        instance.root_gib = plan.journal.root_gib;
        instance.home_gib = plan.journal.home_gib;
    } else {
        let root = roots.get_mut(&instance.node).ok_or("root SSH access missing")?;
        if action == Action::DeleteData {
            checkpoint(path, source, id, "delete exact retained home")?;
            root_call(
                root.as_mut(),
                &json!({"action":"delete-data", "ctid":id, "node":instance.node, "home":instance.home_volume}),
            )?;
            if volume_exists(pve, &instance).await? {
                return Err("home still appears in storage inventory".to_owned());
            }
            instance.stage = Stage::Removed;
        } else if action == Action::Reconnect {
            let net = instance
                .network
                .clone()
                .ok_or("retained network configuration missing; manual review required")?;
            let template = node::ensure_image(
                pve,
                &instance.node,
                &config.template_storage,
                &plan.journal.image,
                &|line| say(line),
            )
            .await?;
            // Verify the archive against the reviewed platform digest before
            // creating the replacement root; no home is attached during unpack.
            root_call(
                root.as_mut(),
                &json!({"action":"verify-image", "ctid":id, "node":instance.node, "home":instance.home_volume, "archive":template, "platform":plan.journal.platform}),
            )?;
            root_call(
                root.as_mut(),
                &json!({"action":"assert-home-free", "ctid":id, "node":instance.node, "home":instance.home_volume}),
            )?;
            checkpoint(path, source, id, "create root for retained home")?;
            node::create(
                pve,
                &node::NewContainer {
                    node: &instance.node,
                    ctid: id,
                    hostname: &instance.hostname,
                    ostemplate: &template,
                    pool: &instance.pool,
                    storage: &instance.root_storage,
                    disk_gib: instance.root_gib,
                    memory_mib: instance.memory_mib,
                    cores: instance.cores,
                    net0: Some(net),
                    onboot: true,
                    nameserver: instance.nameserver.clone(),
                    description: deploy::description(source, &plan.journal.image, &plan.journal.digest),
                    tag: None,
                    mp0: None,
                },
                &|line| say(line),
            )
            .await?;
            let (_, cfg) = deploy::config_of(pve, id).await?.ok_or("new CT is not visible")?;
            instance.root_volume = vol(&cfg, "rootfs");
            root_call(
                root.as_mut(),
                &json!({"action":"assert-home-free", "ctid":id, "node":instance.node, "home":instance.home_volume}),
            )?;
            checkpoint(path, source, id, "attach retained home before startup")?;
            pve.put(
                &format!("{base}/config"),
                &[
                    ("mp0", &format!("{},mp=/home/app,backup=1", instance.home_volume)),
                    ("tags", &instance.tags.join(";")),
                    ("cmode", "shell"),
                    ("digest", text(&cfg, "digest")),
                ],
            )
            .await
            .map_err(|e| e.to_string())?;
            node::set_env(pve, &instance.node, id, &plan.environment, &[]).await?;
            instance.release = Some(release(source, plan));
            instance.stage = Stage::Created;
            persist_instance(path, source, &instance)?;
            let mut expected = plan.record.clone();
            *expected
                .instances
                .iter_mut()
                .find(|i| i.ctid == id)
                .ok_or("instance disappeared")? = instance.clone();
            let (_, cfg) = deploy::config_of(pve, id).await?.ok_or("new CT disappeared")?;
            verify(source, &expected, &instance, &cfg)?;
            checkpoint(path, source, id, "start reconnected instance")?;
            node::power(pve, &instance.node, id, "start").await?;
            say(&deploy::healthy(&source.manifest, &instance.address).await?);
            instance.stage = Stage::Ready;
            instance.desired = Desired::Running;
            instance.verified = Some(deploy::stamp());
        } else {
            if action != Action::Uninstall {
                return Err("Replacement must use the API replacement engine".to_owned());
            }
            let (_, before) = deploy::config_of(pve, id).await?.ok_or("CT disappeared")?;
            verify(source, &plan.record, &instance, &before)?;
            instance.network = Some(text(&before, "net0").to_owned());
            instance.nameserver = before["nameserver"].as_str().map(str::to_owned);
            persist_instance(path, source, &instance)?;
            checkpoint(path, source, id, "stop")?;
            if observed.running {
                node::power(pve, &instance.node, id, "stop").await?;
            }
            let (_, stopped) = deploy::config_of(pve, id).await?.ok_or("CT disappeared")?;
            verify(source, &plan.record, &instance, &stopped)?;
            checkpoint(path, source, id, "detach exact home before CT removal")?;
            say(&root_call(
                root.as_mut(),
                &json!({
                    "action":"uninstall", "ctid":id, "node":instance.node,
                    "home":instance.home_volume, "digest":text(&stopped,"digest"),
                    "mp0":text(&stopped,"mp0"), "rootfs":text(&stopped,"rootfs")
                }),
            )?);
            let (_, detached) = deploy::config_of(pve, id)
                .await?
                .ok_or("CT disappeared before removal")?;
            if detached.get("mp0").is_some()
                || vol(&detached, "rootfs") != instance.root_volume
                || text(&detached, "net0") != text(&stopped, "net0")
                || !deploy::made_by(source, &detached)
            {
                return Err("Configuration drift after home retention; refusing CT removal".to_owned());
            }
            checkpoint(path, source, id, "remove CT; no purge or unreferenced disks")?;
            let upid = pve
                .delete(&base, &[("purge", "0"), ("destroy-unreferenced-disks", "0")])
                .await
                .map_err(|e| e.to_string())?;
            task(pve, &upid).await?;
            if !volume_exists(pve, &instance).await? {
                return Err("Retained home is no longer visible".to_owned());
            }
            instance.stage = Stage::Retained;
            instance.desired = Desired::Stopped;
        }
    }
    persist_instance(path, source, &instance)?;
    checkpoint(path, source, id, "complete")?;
    Ok(format!(
        "{}: CT {id} · {} complete · home {}",
        instance.node,
        action.label(),
        instance.home_volume
    ))
}
fn release(source: &Source, plan: &Plan) -> Release {
    Release {
        version: plan.journal.image.rsplit_once(':').map_or("", |(_, t)| t).to_owned(),
        digest: plan.journal.digest.clone(),
        commit: source.commit.clone(),
        manifest_blob: source.blob.clone(),
    }
}
fn persist_instance(path: &Path, source: &Source, instance: &Instance) -> Result<(), String> {
    save(path, source, |record| {
        *record
            .instances
            .iter_mut()
            .find(|i| i.ctid == instance.ctid)
            .ok_or("instance disappeared")? = instance.clone();
        Ok(())
    })
}

async fn recover(
    pve: &impl Pve,
    path: &Path,
    source: &Source,
    record: &Deployment,
    instance: &Instance,
    roots: &mut std::collections::BTreeMap<String, Box<dyn Root>>,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<String, String> {
    let mut instance = instance.clone();
    let id = instance.ctid;
    let journal = record.operation.as_ref().and_then(|o| o.lifecycle.as_ref());
    let target = journal.and_then(|j| j.targets.iter().find(|t| t.ctid == id));
    let found = deploy::config_of(pve, id).await?;
    if let Some((found, mut cfg)) = found {
        if found.node != instance.node {
            return Err("wrong node; no recovery mutation permitted".to_owned());
        }
        if cfg.get("mp0").is_none() {
            if !journal.is_some_and(|j| j.action == Action::Uninstall)
                || !target.is_some_and(|t| {
                    matches!(
                        t.step.as_str(),
                        "detach exact home before CT removal" | "remove CT; no purge or unreferenced disks"
                    )
                })
                || found.status != "stopped"
            {
                return Err(
                    "Missing home mount has no resumable uninstall journal; no automatic attachment".to_owned(),
                );
            }
            let mount = format!("{},mp=/home/app,backup=1", instance.home_volume);
            let mut expected = cfg.clone();
            expected["mp0"] = json!(mount);
            verify(source, record, &instance, &expected)?;
            let root = roots
                .get_mut(&instance.node)
                .ok_or("Verified root SSH is required to reconcile detached home")?;
            root_call(
                root.as_mut(),
                &json!({"action":"assert-home-free","ctid":id,"node":instance.node,"home":instance.home_volume}),
            )?;
            pve.put(
                &format!("/nodes/{}/lxc/{id}/config", instance.node),
                &[("mp0", mount.as_str()), ("digest", text(&cfg, "digest"))],
            )
            .await
            .map_err(|e| e.to_string())?;
            cfg = deploy::config_of(pve, id)
                .await?
                .ok_or("CT disappeared during reconciliation")?
                .1;
        }
        verify(source, record, &instance, &cfg)?;
        if let Some(journal) = journal
            && journal.action == Action::Resize
        {
            instance.root_gib = size_gib(&cfg, "rootfs")
                .filter(|n| n.fract() == 0.0)
                .ok_or("actual root size unknown")? as u32;
            instance.home_gib = size_gib(&cfg, "mp0")
                .filter(|n| n.fract() == 0.0)
                .ok_or("actual home size unknown")? as u32;
        }
        let running = target.map_or(instance.desired == Desired::Running, |t| t.was_running);
        if running && found.status != "running" {
            node::power(pve, &instance.node, id, "start").await?;
        }
        if !running && found.status == "running" {
            node::power(pve, &instance.node, id, "stop").await?;
        }
        if running {
            say(&deploy::healthy(&source.manifest, &instance.address).await?);
            instance.verified = Some(deploy::stamp());
        }
        instance.stage = Stage::Ready;
        instance.desired = if running { Desired::Running } else { Desired::Stopped };
    } else if volume_exists(pve, &instance).await? {
        if journal.is_some_and(|j| j.action == Action::Uninstall) || instance.stage == Stage::Retained {
            instance.stage = Stage::Retained;
            instance.desired = Desired::Stopped;
        } else {
            return Err("CT missing, home retained; no automatic recreation or adoption".to_owned());
        }
    } else {
        return Err("home missing or delete-data interrupted; explicit storage review required".to_owned());
    }
    persist_instance(path, source, &instance)?;
    Ok(format!(
        "CT {id}: reconciled verified identities; home {}",
        instance.home_volume
    ))
}

fn filesystem_kib(output: &str, mount: &str) -> Option<u64> {
    output.lines().find_map(|line| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        (parts.len() >= 6 && parts.last().copied() == Some(mount))
            .then(|| parts[1].parse().ok())
            .flatten()
    })
}
