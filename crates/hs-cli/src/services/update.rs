//! Update a deployed service's image under the same CTID, through the
//! Proxmox API only. Existing persistent volumes and kept settings wait in a
//! temporary holder while the container is made again (`hs_deploy::recreate`).
//! Stateless API instances are initialized again from desktop configuration. Nothing is
//! journaled on this desktop: every run starts from what Proxmox shows, so an
//! interrupted update is finished by updating again.

use hs_core::{CoreError, pve::Pve};
use hs_deploy::{
    config::{DeployConfig, InstanceSpec},
    deploy::ApiBootstrap,
    node, recreate,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::deploy::{self, Source};
use super::instance::{self, Instance};

/// Configuration the new container keeps from the old one.
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

/// The reviewed update.
#[derive(Debug, Clone)]
pub struct Plan {
    selection_root: std::path::PathBuf,
    pub image: String,
    pub digest: String,
    pub platform: String,
    pub template_storage: String,
    pub backup_storage: String,
    api_config: Option<DeployConfig>,
    pub targets: Vec<Instance>,
    reviewed: std::collections::BTreeMap<u32, Reviewed>,
    pub lines: Vec<String>,
    pub problems: Vec<String>,
}

fn text<'a>(cfg: &'a Value, key: &str) -> &'a str {
    cfg[key].as_str().unwrap_or_default()
}

pub(super) fn permit(perms: &Value, path: &str, privilege: &str) -> bool {
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

/// What would keep the old container's settings from carrying over.
async fn safe_config(pve: &impl Pve, instance: &Instance, cfg: &Value) -> Result<bool, CoreError> {
    if cfg.get("lock").is_some()
        || cfg.get("protection").is_some_and(|v| v.as_u64() != Some(0))
        || cfg.get("template").is_some_and(|v| v.as_u64() != Some(0))
    {
        return Err(CoreError::Refused(
            "Locked, protected or template containers require review".to_owned(),
        ));
    }
    if cfg["unprivileged"].as_u64() != Some(1) {
        return Err(CoreError::Refused(
            "Replacement requires an unprivileged container".to_owned(),
        ));
    }
    let extra: Vec<&str> = cfg
        .as_object()
        .ok_or_else(|| CoreError::Refused("Container configuration is unreadable".to_owned()))?
        .keys()
        .map(String::as_str)
        .filter(|k| {
            (k.starts_with("mp") && *k != "mp0")
                || (k.starts_with("net") && *k != "net0")
                || k.starts_with("unused")
                || k.starts_with("dev")
                || *k == "hookscript"
        })
        .collect();
    if !extra.is_empty() {
        return Err(CoreError::Refused(format!(
            "Extra disks, networks, devices or hooks require manual review: {}",
            extra.join(", ")
        )));
    }
    let allowed_lxc = [
        "lxc.init.uid",
        "lxc.init.gid",
        "lxc.init.groups",
        "lxc.init.cwd",
        "lxc.signal.halt",
    ];
    if cfg.get("lxc").is_some_and(|lxc| {
        lxc.as_array().is_none_or(|entries| {
            entries
                .iter()
                .any(|entry| entry[0].as_str().is_none_or(|key| !allowed_lxc.contains(&key)))
        })
    }) {
        return Err(CoreError::Refused(
            "Custom LXC configuration requires manual review".to_owned(),
        ));
    }
    let base = format!("/nodes/{}/lxc/{}", instance.node, instance.ctid);
    let status = pve.get(&format!("{base}/status/current"), &[]).await?;
    if status["ha"]["managed"].as_u64() != Some(0) && status["ha"]["managed"].as_bool() != Some(false) {
        return Err(CoreError::Refused(
            "HA-managed or unknown HA state requires a separate placement plan".to_owned(),
        ));
    }
    let guest = instance.ctid.to_string();
    let replication = pve
        .get(&format!("/nodes/{}/replication", instance.node), &[("guest", &guest)])
        .await?;
    if replication.as_array().is_none_or(|jobs| !jobs.is_empty()) {
        return Err(CoreError::Refused(
            "Replication jobs require a separate placement plan".to_owned(),
        ));
    }
    // Do not silently discard security configuration attached to the old CTID.
    for section in ["rules", "aliases", "ipset"] {
        let data = pve.get(&format!("{base}/firewall/{section}"), &[]).await?;
        if data.as_array().is_none_or(|entries| !entries.is_empty()) {
            return Err(CoreError::Refused(
                "Custom container firewall configuration requires manual review".to_owned(),
            ));
        }
    }
    let firewall = pve.get(&format!("{base}/firewall/options"), &[]).await?;
    if firewall
        .as_object()
        .is_none_or(|options| options.keys().any(|key| key != "digest"))
    {
        return Err(CoreError::Refused(
            "Custom container firewall options require manual review".to_owned(),
        ));
    }
    let snapshots = pve.get(&format!("{base}/snapshot"), &[]).await?;
    if snapshots
        .as_array()
        .is_none_or(|items| items.iter().any(|s| s["name"] != "current"))
    {
        return Err(CoreError::Refused(
            "Snapshots can prevent home reassignment; inspect them before replacement".to_owned(),
        ));
    }
    let pending = pve.get(&format!("{base}/pending"), &[]).await?;
    if pending.as_array().is_none_or(|items| {
        items
            .iter()
            .any(|i| i.get("pending").is_some() || i["delete"].as_u64() == Some(1))
    }) {
        return Err(CoreError::Refused("Pending configuration requires review".to_owned()));
    }
    match status["status"].as_str() {
        Some("running") => Ok(true),
        Some("stopped") => Ok(false),
        _ => Err(CoreError::Refused("Container power state unavailable".to_owned())),
    }
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
    for storage in [&instance.root_storage, &instance.home_storage, &cfg.template_storage]
        .into_iter()
        .filter(|storage| !storage.is_empty())
    {
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
    ]
    .into_iter()
    .filter(|(id, _)| !id.is_empty())
    {
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
                .is_none_or(|n| n < u64::from(instance.root_mib) * 1024 * 1024)
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
        .split(',')
        .find_map(|p| p.strip_prefix("bridge="))
        .ok_or("Original network has no bridge")?;
    // vmbr bridges use the localnetwork SDN zone; other bridges are checked by PVE itself.
    if bridge.starts_with("vmbr") && !permit(&permissions, &format!("/sdn/zones/localnetwork/{bridge}"), "SDN.Use") {
        return Err(format!("Deploy token lacks SDN.Use for {bridge}"));
    }
    Ok(())
}

/// Review updating `ctids` to `version`; nothing changes.
pub async fn plan(
    pve: &impl Pve,
    config: &DeployConfig,
    source: &Source,
    ctids: &[u32],
    version: &str,
    backup: &str,
    selection_root: &std::path::Path,
) -> Plan {
    let mut plan = Plan {
        selection_root: selection_root.to_owned(),
        image: source.image(version),
        digest: String::new(),
        platform: String::new(),
        template_storage: config.template_storage.clone(),
        backup_storage: backup.to_owned(),
        api_config: source.is_api().then(|| config.clone()),
        targets: Vec::new(),
        reviewed: std::collections::BTreeMap::new(),
        lines: Vec::new(),
        problems: Vec::new(),
    };
    if let Err(error) = review(pve, config, source, ctids, version, &mut plan).await {
        plan.problems.push(error);
    }
    if source.manifest.schema3.is_some() {
        plan.lines.push("Image replacement discards all rootfs-local application data, including any unsent exporter queue. Ordinary CT restarts preserve it.".into());
        for target in &plan.targets {
            match application_environment(&plan.selection_root, source, target) {
                Ok(environment) => {
                    let changed: Vec<&str> = environment
                        .iter()
                        .filter(|entry| !target.environment.contains(entry))
                        .map(|(key, _)| key.as_str())
                        .collect();
                    if !changed.is_empty() {
                        plan.lines.push(format!(
                            "CT {}: sets {} from Application configuration",
                            target.ctid,
                            changed.join(", ")
                        ));
                    }
                }
                Err(error) => plan.problems.push(error),
            }
        }
    }
    for target in &plan.targets {
        let (was_running, was_onboot) = match plan
            .reviewed
            .get(&target.ctid)
            .map(|state| state.power)
            .map_or_else(|| original_power(target), Ok)
        {
            Ok(state) => state,
            Err(error) => {
                plan.problems.push(error);
                continue;
            }
        };
        let current = target.image.rsplit_once(':').map_or("unknown", |(_, v)| v);
        let state = if target.running { "running" } else { "stopped" };
        plan.lines.extend([
            format!("{} · {} · CT {}", target.hostname, target.node, target.ctid),
            String::new(),
            "Changes".to_owned(),
            format!(
                "  Image      {current} → {version}{}",
                if current == version { " (reinstall)" } else { "" }
            ),
            format!(
                "  Root       new {} on {}",
                node::show_size(target.root_mib),
                target.root_storage
            ),
            String::new(),
            "Preserved".to_owned(),
            format!("  Container  CT {}", target.ctid),
            if target.home_volume.is_empty() {
                "  Volumes    none".to_owned()
            } else {
                format!(
                    "  Home       {} · {} · /home/app",
                    target.home_volume,
                    node::show_size(target.home_mib)
                )
            },
            format!("  Network    {} · MAC {}", target.address, target.mac),
            format!("  Name/pool  {} · {}", target.hostname, target.pool),
            format!(
                "  State      {state} → {} · autostart {}",
                if was_running || was_onboot {
                    "running"
                } else {
                    "stopped"
                },
                if was_onboot { "on" } else { "off" }
            ),
            String::new(),
        ]);
        if target.held {
            plan.lines.push(format!(
                "An earlier update stopped while CT {} was gone; its home waits in CT {}.",
                target.ctid,
                recreate::HOLDER_ID
            ));
            plan.lines.push(String::new());
        }
    }
    if !plan.backup_storage.is_empty() {
        plan.lines
            .push(format!("Backup: stopped container and home → {}", plan.backup_storage));
    }
    plan.lines.push(match &source.manifest.healthcheck {
        None => "Health check: not provided by this service.".to_owned(),
        Some(_) => "Health check: automatic after update.".to_owned(),
    });
    plan
}

async fn review(
    pve: &impl Pve,
    config: &DeployConfig,
    source: &Source,
    ctids: &[u32],
    version: &str,
    plan: &mut Plan,
) -> Result<(), String> {
    if ctids.is_empty() {
        return Err("Select the containers to update".to_owned());
    }
    if hs_core::registry::version(version).is_none() {
        return Err("Choose a published numeric SemVer release".to_owned());
    }
    if let Some(problem) = source.image_blocker(version) {
        return Err(problem.to_owned());
    }
    let release = hs_core::registry::resolve(&plan.image).await?;
    plan.digest = release.digest;
    plan.platform = release.platform;
    for ctid in ctids {
        let target = instance::read(pve, *ctid).await?;
        if deploy::initializing(&target.description) {
            return Err(format!(
                "CT {ctid}: finish initialization by reviewing deployment with this CTID before updating its image"
            ));
        }
        if target.repository != source.repository || target.service != source.manifest.id {
            return Err(format!(
                "CT {ctid} runs {} of {}, not {}",
                target.service, target.repository, source.manifest.id
            ));
        }
        if target.home_volume.is_empty() && source.manifest.persistence.is_some() {
            return Err(format!(
                "CT {ctid}: the service requires its persistent home volume (mp0)"
            ));
        }
        if target.home_volume.is_empty()
            && let Some(config) = &plan.api_config
        {
            let bootstrap = ApiBootstrap::load(config)?;
            if target
                .searchdomain
                .as_deref()
                .is_some_and(|domain| domain != config.domain)
            {
                return Err(format!(
                    "CT {ctid}: API domain differs from the deployment certificate domain"
                ));
            }
            if config.pve.ca_file.is_some() {
                return Err("API initialization requires system-trusted PVE certificates".to_owned());
            }
            bootstrap.validate_instance(&api_spec(&target)?)?;
        }
        if !target.held {
            let cfg = pve
                .get(&format!("/nodes/{}/lxc/{ctid}/config", target.node), &[])
                .await
                .map_err(|e| format!("CT {ctid} config: {e}"))?;
            if !plan.backup_storage.is_empty()
                && !target.home_volume.is_empty()
                && !text(&cfg, "mp0").split(',').any(|v| v == "backup=1")
            {
                return Err("Home must have backup=1 for the selected backup".to_owned());
            }
        }
        preflight(pve, config, &target, &plan.backup_storage).await?;
        let reviewed = reviewed_state(pve, &target, &plan.image, &plan.digest)
            .await
            .map_err(safety_message)?;
        plan.reviewed.insert(target.ctid, reviewed);
        plan.targets.push(target);
    }
    Ok(())
}

async fn image_matches(plan: &Plan) -> Result<(), String> {
    let current = hs_core::registry::resolve(&plan.image).await?;
    if current.digest != plan.digest || current.platform != plan.platform {
        return Err("Image tag changed since review; refuse the update".to_owned());
    }
    Ok(())
}

async fn task(pve: &impl Pve, upid: &Value, what: &str) -> Result<(), String> {
    let upid = upid.as_str().ok_or_else(|| format!("{what}: Proxmox gave no task"))?;
    let result = hs_core::tasks::wait_task(
        pve,
        upid,
        &hs_core::tasks::TaskWait::up_to(std::time::Duration::from_secs(3600)),
        &hs_core::tasks::NoObserver,
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
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<Vec<String>, String> {
    if !plan.problems.is_empty() {
        return Err("Resolve the review problems first".to_owned());
    }
    if source.manifest.schema3.is_some() {
        for target in &plan.targets {
            application_environment(&plan.selection_root, source, target)?;
        }
    }
    let mut lines = Vec::new();
    for target in &plan.targets {
        let bootstrap = if target.home_volume.is_empty() {
            plan.api_config.as_ref().map(ApiBootstrap::load).transpose()?
        } else {
            None
        };
        if let Some(bootstrap) = &bootstrap {
            bootstrap.validate_instance(&api_spec(target)?)?;
        }
        let done = one(pve, source, plan, target, bootstrap.as_ref(), say)
            .await
            .map_err(|error| {
                format!(
                    "{error}\nUpdate paused. Update the image again to finish it; it continues from what Proxmox shows."
                )
            })?;
        for line in &done {
            say(line);
        }
        lines.extend(done);
    }
    Ok(lines)
}

/// The environment the saved application selections and HomeStack's own
/// settings give a schema-3 container.
fn application_environment(
    root: &std::path::Path,
    source: &Source,
    target: &Instance,
) -> Result<Vec<(String, String)>, String> {
    let hostname = super::selections::base_name(&target.hostname);
    let saved = super::selections::load(
        root,
        source,
        hostname,
        target.searchdomain.as_deref().unwrap_or_default(),
        true,
    )?
    .ok_or("Private application selections are unavailable")?;
    let contract = source.manifest.schema3.as_ref().ok_or("Expected schema 3")?;
    let mut values = saved.values;
    contract.with_homestack(&mut values, &DeployConfig::load(&root.join("deploy.toml"))?)?;
    contract.resolve(&values)
}

/// Distinguish a reviewed reinstall from a root already made by that operation.
/// The temporary holder carries the operation through interrupted replacement.
const REPLACEMENT_OPERATION: &str = "replacement-operation ";
const REPLACEMENT_RUNNING: &str = "replacement-running ";
const REPLACEMENT_ONBOOT: &str = "replacement-onboot ";

/// Intended power settings survive a stopped replacement and a process restart.
fn original_power(target: &Instance) -> Result<(bool, bool), String> {
    let flag = |prefix: &str, current: bool| match target.description.lines().find_map(|line| line.strip_prefix(prefix))
    {
        None => Ok(current),
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(_) => Err(format!("CT {}: invalid replacement power metadata", target.ctid)),
    };
    Ok((
        flag(REPLACEMENT_RUNNING, target.running)?,
        flag(REPLACEMENT_ONBOOT, target.onboot)?,
    ))
}

/// PVE can reorder mount options and report the allocated size after moving a
/// volume. Verify the mount settings and refuse a smaller persistent home.
fn same_home(new: &Instance, target: &Instance) -> bool {
    let options = |mount: &str| -> std::collections::BTreeMap<String, String> {
        mount
            .split(',')
            .skip(1)
            .filter_map(|part| part.split_once('='))
            .filter(|(key, _)| *key != "size")
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect()
    };
    new.home_volume.is_empty() == target.home_volume.is_empty()
        && new.home_storage == target.home_storage
        && new.home_mib >= target.home_mib
        && options(&new.home_mount) == options(&target.home_mount)
        && (new.home_volume.is_empty() || new.home_volume.contains(&format!("-{}-", target.ctid)))
}

async fn restore_autostart(pve: &impl Pve, node: &str, ctid: u32, enabled: bool) -> Result<(), String> {
    let path = format!("/nodes/{node}/lxc/{ctid}/config");
    let cfg = pve
        .get(&path, &[])
        .await
        .map_err(|e| format!("CT {ctid} config: {e}"))?;
    if (cfg["onboot"].as_u64() == Some(1)) != enabled {
        pve.put(
            &path,
            &[
                ("onboot", if enabled { "1" } else { "0" }),
                ("digest", text(&cfg, "digest")),
            ],
        )
        .await
        .map_err(|e| format!("CT {ctid} autostart: {e}"))?;
    }
    Ok(())
}

/// All configuration is bound, not just fields the current reconstruction reads.
/// Keep only a digest in the plan: configuration can contain application secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reviewed {
    fingerprint: [u8; 32],
    recovery_operation: Option<String>,
    power: (bool, bool),
}

fn configuration_identity(mut cfg: Value) -> Result<Value, String> {
    let fields = cfg.as_object_mut().ok_or("Container configuration is unreadable")?;
    fields.remove("digest");
    for (key, value) in fields {
        let Some(text) = value.as_str() else { continue };
        let separator = match key.as_str() {
            "env" => '\0',
            "tags" => ';',
            "rootfs" | "mp0" | "net0" | "features" => ',',
            _ => continue,
        };
        let mut parts: Vec<_> = text.split(separator).collect();
        // A volume's leading value has meaning. Duplicate option/env keys
        // may be order-sensitive, so do not normalize their order.
        let offset = usize::from(matches!(key.as_str(), "rootfs" | "mp0"));
        if let Some(options) = parts.get_mut(offset..) {
            let keys: std::collections::BTreeSet<_> = options.iter().map(|part| part.split('=').next()).collect();
            if keys.len() == options.len() {
                options.sort_unstable();
            }
        }
        *value = Value::String(parts.join(&separator.to_string()));
    }
    Ok(cfg)
}

fn same_instance(left: &Instance, right: &Instance) -> Result<bool, String> {
    let normalize = |instance: &Instance| -> Result<Instance, String> {
        let mut instance = instance.clone();
        let cfg = configuration_identity(json!({"net0": instance.network, "mp0": instance.home_mount,
            "env": node::merge_env(&instance.environment, &[], &[])}))?;
        instance.network = text(&cfg, "net0").to_owned();
        instance.home_mount = text(&cfg, "mp0").to_owned();
        instance.environment = node::parse_env(text(&cfg, "env"));
        Ok(instance)
    };
    Ok(normalize(left)? == normalize(right)?)
}

fn operation(description: &str) -> Result<Option<String>, String> {
    let values: Vec<_> = description
        .lines()
        .filter_map(|line| line.strip_prefix(REPLACEMENT_OPERATION))
        .collect();
    match values.as_slice() {
        [] if description
            .lines()
            .any(|line| line.starts_with(REPLACEMENT_RUNNING) || line.starts_with(REPLACEMENT_ONBOOT)) =>
        {
            Err("Replacement power intent has no operation identity".to_owned())
        }
        [] => Ok(None),
        [value] => uuid::Uuid::parse_str(value)
            .map(|id| Some(id.to_string()))
            .map_err(|_| "Invalid replacement operation identity".to_owned()),
        _ => Err("Ambiguous replacement operation identity".to_owned()),
    }
}

/// CLI presentation is the boundary. Infrastructure details never become a
/// guest/configuration refusal string while the safety checks are running.
fn safety_message(error: CoreError) -> String {
    if error.pve_error().is_some() {
        "Proxmox could not verify replacement safety; no replacement was started. Review again.".to_owned()
    } else {
        error.to_string()
    }
}

/// Review recovery as its own exact state, including both the real target and
/// holder. No arbitrary configuration drift is exempted by a recovery flag.
async fn reviewed_state(pve: &impl Pve, target: &Instance, image: &str, digest: &str) -> Result<Reviewed, CoreError> {
    let resources = pve.get("/cluster/resources", &[("type", "vm")]).await?;
    let resources = resources
        .as_array()
        .ok_or_else(|| CoreError::Refused("Cluster inventory unavailable".to_owned()))?;
    for id in [target.ctid, recreate::HOLDER_ID] {
        let matches: Vec<_> = resources
            .iter()
            .filter(|r| r["vmid"].as_u64() == Some(u64::from(id)))
            .collect();
        if matches.len() > 1
            || matches
                .iter()
                .any(|r| r["type"] != "lxc" || r["node"] != target.node || r["pool"] != target.pool)
        {
            return Err(CoreError::Refused(
                "Replacement location/pool changed; review again".to_owned(),
            ));
        }
        if id == target.ctid && matches.is_empty() && !target.held {
            return Err(CoreError::Refused(
                "Replacement target disappeared; review again".to_owned(),
            ));
        }
    }
    let list = pve.get(&format!("/nodes/{}/lxc", target.node), &[]).await?;
    let list = list
        .as_array()
        .ok_or_else(|| CoreError::Refused("Container inventory unavailable".to_owned()))?;
    if !target.held
        && !list
            .iter()
            .any(|item| item["vmid"].as_u64() == Some(u64::from(target.ctid)))
    {
        return Err(CoreError::Refused(
            "Target missing from node inventory; review again".to_owned(),
        ));
    }
    let mut states = Vec::new();
    let mut recovery_operation = None;
    let mut power = original_power(target).map_err(CoreError::Refused)?;
    for id in [target.ctid, recreate::HOLDER_ID] {
        if !list.iter().any(|item| item["vmid"].as_u64() == Some(u64::from(id))) {
            continue;
        }
        let cfg = pve.get(&format!("/nodes/{}/lxc/{id}/config", target.node), &[]).await?;
        let mut subject = target.clone();
        subject.ctid = id;
        let running = safe_config(pve, &subject, &cfg).await?;
        recreate::mounts(&cfg).map_err(CoreError::Refused)?;
        let description = text(&cfg, "description");
        let op = operation(description).map_err(CoreError::Refused)?;
        if id == recreate::HOLDER_ID {
            if running || recreate::holder_of(description) != Some(target.ctid) || op.is_none() {
                return Err(CoreError::Refused(
                    "Replacement holder ownership/operation is unproven; inspect it before updating".to_owned(),
                ));
            }
            if recovery_operation.is_some() && recovery_operation != op {
                return Err(CoreError::Refused(
                    "Target and holder belong to different replacement operations".to_owned(),
                ));
            }
            // Parsing verifies the holder's saved reconstruction and service identity.
            let held = instance::parse(id, &target.node, &target.pool, &cfg, false).map_err(CoreError::Refused)?;
            if target.held && !same_instance(&held, target).map_err(CoreError::Refused)? {
                return Err(CoreError::Refused(
                    "Holder changed during review; review again".to_owned(),
                ));
            }
            if held.repository != target.repository || held.service != target.service {
                return Err(CoreError::Refused(
                    "Replacement holder service ownership changed".to_owned(),
                ));
            }
            if held.hostname != target.hostname
                || !node::same_net(&held.network, &target.network)
                || held.mac != target.mac
                || held.cores != target.cores
                || held.memory_mib != target.memory_mib
                || held.root_storage != target.root_storage
                || held.root_mib != target.root_mib
                || held.nameserver != target.nameserver
                || held.searchdomain != target.searchdomain
                || held.pool != target.pool
            {
                return Err(CoreError::Refused(
                    "Replacement holder reconstruction differs from the reviewed target".to_owned(),
                ));
            }
            power = original_power(&held).map_err(CoreError::Refused)?;
            recovery_operation = op;
        } else {
            let parsed = instance::parse(id, &target.node, &target.pool, &cfg, running).map_err(CoreError::Refused)?;
            if !target.held && !same_instance(&parsed, target).map_err(CoreError::Refused)? {
                return Err(CoreError::Refused(
                    "Target configuration changed during review; review again".to_owned(),
                ));
            }
            if op.is_some() {
                for prefix in [REPLACEMENT_RUNNING, REPLACEMENT_ONBOOT] {
                    if description.lines().filter(|line| line.starts_with(prefix)).count() != 1 {
                        return Err(CoreError::Refused(
                            "Replacement power intent is missing or ambiguous".to_owned(),
                        ));
                    }
                }
                if parsed.image != image || parsed.digest != digest {
                    return Err(CoreError::Refused(
                        "Finish the pending replacement release before selecting another image".to_owned(),
                    ));
                }
                recovery_operation = op;
            }
        }
        let root = text(&cfg, "rootfs").split(',').next().unwrap_or_default();
        let home = text(&cfg, "mp0").split(',').next().unwrap_or_default();
        if root.is_empty() || root == home || root.starts_with('/') || !root.contains(':') {
            return Err(CoreError::Refused("Unproven root/home volume separation".to_owned()));
        }
        if !root.contains(&format!("-{id}-")) || (!home.is_empty() && !home.contains(&format!("-{id}-"))) {
            return Err(CoreError::Refused("Root/home volume ownership is unproven".to_owned()));
        }
        states.push(
            json!({"id": id, "running": running, "config": configuration_identity(cfg).map_err(CoreError::Refused)?}),
        );
    }
    if target.held && recovery_operation.is_none() {
        return Err(CoreError::Refused(
            "Replacement holder operation is unavailable".to_owned(),
        ));
    }
    let bytes = serde_json::to_vec(&json!({
        "node": target.node, "pool": target.pool, "running": target.running,
        "states": states,
    }))
    .map_err(|_| CoreError::Refused("Cannot bind replacement configuration".to_owned()))?;
    Ok(Reviewed {
        fingerprint: Sha256::digest(bytes).into(),
        recovery_operation,
        power,
    })
}

fn api_spec(target: &Instance) -> Result<InstanceSpec, String> {
    Ok(InstanceSpec {
        name: target.hostname.clone(),
        node: target.node.clone(),
        ctid: target.ctid,
        address: target.address.parse().map_err(|_| "Invalid API instance address")?,
        storage: target.root_storage.clone(),
    })
}

async fn one(
    pve: &impl Pve,
    source: &Source,
    plan: &Plan,
    target: &Instance,
    bootstrap: Option<&ApiBootstrap>,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<Vec<String>, String> {
    let (ctid, node) = (target.ctid, target.node.as_str());
    if deploy::initializing(&target.description) {
        return Err(format!(
            "CT {ctid}: initialization is unfinished; image replacement is refused"
        ));
    }
    let reviewed = plan
        .reviewed
        .get(&ctid)
        .ok_or("No reviewed replacement configuration")?;
    let current = reviewed_state(pve, target, &plan.image, &plan.digest)
        .await
        .map_err(safety_message)?;
    if current != *reviewed {
        return Err(format!("CT {ctid} changed since review; review again"));
    }
    // Every read/authorization check precedes the first environment or PVE write.
    if source.manifest.schema3.is_some() {
        let environment = application_environment(&plan.selection_root, source, target)?;
        if !target.held && environment.iter().any(|entry| !target.environment.contains(entry)) {
            say(&format!("{node}: CT {ctid} · Application configuration"));
            deploy::set_application_environment(pve, node, ctid, &environment, &[]).await?;
        }
    }
    let (was_running, was_onboot) = reviewed.power;
    let operation = reviewed
        .recovery_operation
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let completed_description = deploy::description(source, &plan.image, &plan.digest);
    let description = format!(
        "{completed_description}\n{REPLACEMENT_OPERATION}{operation}\n{REPLACEMENT_RUNNING}{}\n{REPLACEMENT_ONBOOT}{}",
        u8::from(was_running),
        u8::from(was_onboot)
    );
    // Pull the release once per digest; the file is removed after the update.
    let short = plan.digest.trim_start_matches("sha256:").get(..12).unwrap_or_default();
    let file = format!("hs-{ctid}-{short}");
    let ostemplate = format!("{}:vztmpl/{file}.tar", plan.template_storage);
    let content = pve
        .get(
            &format!("/nodes/{node}/storage/{}/content", plan.template_storage),
            &[("content", "vztmpl")],
        )
        .await
        .map_err(|e| format!("{node} {}: {e}", plan.template_storage))?;
    if !content
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| item["volid"].as_str() == Some(ostemplate.as_str()))
    {
        say(&format!("{node}: CT {ctid} · Pull"));
        image_matches(plan).await?;
        let upid = pve
            .post(
                &format!("/nodes/{node}/storage/{}/oci-registry-pull", plan.template_storage),
                &[("reference", plan.image.as_str()), ("filename", file.as_str())],
            )
            .await
            .map_err(|e| format!("pull on {node}: {e}"))?;
        task(pve, &upid, &format!("pull on {node}")).await?;
        image_matches(plan).await?;
    }
    let untouched = !target.held && target.description.trim_end() != description.trim_end();
    if untouched && !plan.backup_storage.is_empty() {
        say(&format!("{node}: CT {ctid} · Backup"));
        let id = ctid.to_string();
        let upid = pve
            .post(
                &format!("/nodes/{node}/vzdump"),
                &[
                    ("vmid", id.as_str()),
                    ("storage", plan.backup_storage.as_str()),
                    ("mode", "stop"),
                    ("compress", "zstd"),
                ],
            )
            .await
            .map_err(|e| format!("backup CT {ctid}: {e}"))?;
        task(pve, &upid, &format!("backup CT {ctid}")).await?;
    }
    say(&format!("{node}: CT {ctid} · Replace"));
    let previous = instance::note(target);
    let note = format!(
        "{}\n{REPLACEMENT_OPERATION}{operation}",
        previous
            .lines()
            .filter(|line| !line.starts_with(REPLACEMENT_OPERATION))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let spec = node::NewContainer {
        searchdomain: None,
        swap_mib: 0,
        node,
        ctid,
        hostname: &target.hostname,
        ostemplate: &ostemplate,
        pool: &target.pool,
        storage: &target.root_storage,
        disk_mib: target.root_mib,
        memory_mib: target.memory_mib,
        cores: target.cores,
        net0: Some(target.network.clone()),
        onboot: false,
        nameserver: target.nameserver.clone(),
        description: description.clone(),
        tag: Some(node::SERVER_TAG),
        mp0: None,
    };
    recreate::replace(
        pve,
        &recreate::Replace {
            spec,
            graceful: true,
            keep: KEEP,
            note: &note,
        },
        &|line| say(line),
    )
    .await?;
    let new = instance::read_on(pve, node, ctid).await?;
    let differences: Vec<&str> = [
        (new.held, "persistent volumes still in holder"),
        (
            new.description.trim_end() != description.trim_end(),
            "replacement identity",
        ),
        (new.hostname != target.hostname, "hostname"),
        (new.mac != target.mac, "MAC address"),
        (!node::same_net(&new.network, &target.network), "network configuration"),
        (
            !same_home(&new, target),
            "home storage, mount options, ownership or size",
        ),
    ]
    .into_iter()
    .filter_map(|(different, field)| different.then_some(field))
    .collect();
    if !differences.is_empty() {
        return Err(format!(
            "CT {ctid}: replacement verification failed: {}",
            differences.join(", ")
        ));
    }
    // Once persistent volumes and identity are verified, a health-check failure
    // must not silently disable the original autostart setting.
    if bootstrap.is_none() {
        restore_autostart(pve, node, ctid, was_onboot).await?;
    }
    deploy::set_heartbeat(pve, node, ctid, &source.manifest).await?;
    say(&format!("{node}: CT {ctid} · Start"));
    let start = was_running || was_onboot;
    let readiness;
    if let Some(bootstrap) = bootstrap {
        say(&format!("{node}: CT {ctid} · Initialize API"));
        bootstrap
            .initialize(pve, &api_spec(target)?, false, &|line| say(line))
            .await?;
        readiness = "HTTPS /health/live: response OK; uploaded configuration version verified".to_owned();
        if !start {
            node::power(pve, node, ctid, "stop").await?;
        }
    } else if start {
        if !new.running {
            node::power(pve, node, ctid, "start").await?;
        }
        readiness = deploy::healthy(&source.manifest, &target.address).await?;
    } else {
        readiness = "Health check skipped: container left stopped".to_owned();
    }
    restore_autostart(pve, node, ctid, was_onboot).await?;
    // Only successful initialization and readiness complete the operation.
    // Until then its intended power settings remain available for the next run.
    let path = format!("/nodes/{node}/lxc/{ctid}/config");
    let cfg = pve
        .get(&path, &[])
        .await
        .map_err(|e| format!("CT {ctid} config: {e}"))?;
    pve.put(
        &path,
        &[
            ("description", completed_description.as_str()),
            ("digest", text(&cfg, "digest")),
        ],
    )
    .await
    .map_err(|e| format!("CT {ctid}: cannot complete replacement metadata: {e}"))?;
    node::remove_old_images(pve, node, &plan.template_storage, &format!("hs-{ctid}-"), "", &|line| {
        say(line)
    })
    .await;
    Ok(vec![
        format!(
            "{node}: {} · CT {ctid} · {} · home {}{}",
            target.hostname,
            plan.image,
            if new.home_volume.is_empty() {
                "none"
            } else {
                &new.home_volume
            },
            if start { "" } else { " · stopped" }
        ),
        format!("{node}: CT {ctid} · {readiness}"),
    ])
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::*;

    fn instance_with_home(home: &str, description: &str) -> Instance {
        instance::parse(120, "pve3", "services", &json!({
            "hostname": "app", "rootfs": "local-lvm:vm-120-disk-0,size=128M",
            "mp0": home, "onboot": 0,
                "net0": "name=eth0,bridge=vmbr0,ip=192.0.2.10/24,hwaddr=02:00:00:00:00:10",
            "description": format!("HomeStack service app\nrepository owner/app\nimage registry/app:1.0.0\ndigest sha256:abc\nmade by homestack\n{description}"),
        }), false).unwrap()
    }

    #[test]
    fn persistent_home_verification_accepts_allocation_rounding_but_rejects_loss() {
        let original = instance_with_home("local-lvm:vm-120-disk-1,mp=/home/app,backup=1,size=10M", "");
        let actual = instance_with_home("local-lvm:vm-120-disk-1,size=12M,backup=1,mp=/home/app", "");
        assert!(same_home(&actual, &original));
        for mount in [
            "local-lvm:vm-120-disk-1,mp=/home/app,backup=1,size=8M",
            "local-lvm:vm-120-disk-1,mp=/other,backup=1,size=12M",
            "other:vm-120-disk-1,mp=/home/app,backup=1,size=12M",
            "local-lvm:vm-999-disk-1,mp=/home/app,backup=1,size=12M",
            "local-lvm:vm-120-disk-1,mp=/home/app,backup=0,size=12M",
            "",
        ] {
            assert!(!same_home(&instance_with_home(mount, ""), &original), "{mount}");
        }
        let stateless = instance_with_home("", "");
        assert!(same_home(&stateless, &stateless));
        assert!(!same_home(&actual, &stateless));
    }

    #[test]
    fn interrupted_replacement_keeps_intended_power_through_holder_recovery() {
        let pending = instance_with_home("", "replacement-running 1\nreplacement-onboot 1");
        assert!(!pending.running && !pending.onboot);
        assert_eq!(original_power(&pending).unwrap(), (true, true));
        let note = instance::note(&pending);
        let recovered = instance::parse(
            recreate::HOLDER_ID,
            "pve3",
            "services",
            &json!({
                "description": format!("HomeStack holder for CT 120\n{note}"),
                "rootfs": "local-lvm:vm-999999-disk-0,size=1G",
            }),
            false,
        )
        .unwrap();
        assert!(recovered.held);
        assert_eq!(recovered.ctid, 120);
        assert_eq!(original_power(&recovered).unwrap(), (true, true));
        let stopped = instance_with_home("", "replacement-running 0\nreplacement-onboot 0");
        assert_eq!(original_power(&stopped).unwrap(), (false, false));
        let invalid = instance_with_home("", "replacement-onboot invalid");
        assert!(original_power(&invalid).is_err());
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
}

#[cfg(test)]
#[path = "update_safety_tests.rs"]
mod safety_tests;
