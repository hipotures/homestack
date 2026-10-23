//! Update a deployed service's image under the same CTID, through the
//! Proxmox API only. The home volume waits in a temporary holder container
//! while the container is made again (`hs_deploy::recreate`). Nothing is
//! journaled on this desktop: every run starts from what Proxmox shows, so an
//! interrupted update is finished by updating again.

use hs_core::pve::Pve;
use hs_deploy::{config::DeployConfig, node, recreate};
use serde_json::Value;

use super::deploy::{self, SiteReady, Source};
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
    pub image: String,
    pub digest: String,
    pub platform: String,
    pub template_storage: String,
    pub backup_storage: String,
    pub targets: Vec<Instance>,
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
    site: Option<&dyn SiteReady>,
) -> Plan {
    let mut plan = Plan {
        image: source.image(version),
        digest: String::new(),
        platform: String::new(),
        template_storage: config.template_storage.clone(),
        backup_storage: backup.to_owned(),
        targets: Vec::new(),
        lines: Vec::new(),
        problems: Vec::new(),
    };
    if let Err(error) = review(pve, config, source, ctids, version, site, &mut plan).await {
        plan.problems.push(error);
    }
    for target in &plan.targets {
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
            format!("  Root       new {} GiB on {}", target.root_gib, target.root_storage),
            String::new(),
            "Preserved".to_owned(),
            format!("  Container  CT {}", target.ctid),
            format!(
                "  Home       {} · {} GiB · /home/app",
                target.home_volume, target.home_gib
            ),
            format!("  Network    {} · MAC {}", target.address, target.mac),
            format!("  Name/pool  {} · {}", target.hostname, target.pool),
            format!(
                "  State      {state} → {} · autostart {}",
                if target.running || target.onboot {
                    "running"
                } else {
                    "stopped"
                },
                if target.onboot { "on" } else { "off" }
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
        _ if source.is_site() => "Readiness: the container runs and its own Pangolin Site is online.".to_owned(),
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
    site: Option<&dyn SiteReady>,
    plan: &mut Plan,
) -> Result<(), String> {
    if ctids.is_empty() {
        return Err("Select the containers to update".to_owned());
    }
    if hs_core::registry::version(version).is_none() {
        return Err("Choose a published numeric SemVer release".to_owned());
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
        if target.home_volume.is_empty() {
            return Err(format!("CT {ctid} has no home volume (mp0)"));
        }
        if source.is_site() {
            let site = site.ok_or("Site updates require the verified Pangolin context")?;
            plan.lines.push(tokio::task::block_in_place(|| site.identity(*ctid))?);
        }
        if !target.held {
            let cfg = pve
                .get(&format!("/nodes/{}/lxc/{ctid}/config", target.node), &[])
                .await
                .map_err(|e| format!("CT {ctid} config: {e}"))?;
            safe_config(pve, &target, &cfg).await?;
            recreate::mounts(&cfg)?;
            if !plan.backup_storage.is_empty() && !text(&cfg, "mp0").split(',').any(|v| v == "backup=1") {
                return Err("Home must have backup=1 for the selected backup".to_owned());
            }
        }
        preflight(pve, config, &target, &plan.backup_storage).await?;
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
    site: Option<&dyn SiteReady>,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<Vec<String>, String> {
    if !plan.problems.is_empty() {
        return Err("Resolve the review problems first".to_owned());
    }
    let mut lines = Vec::new();
    for target in &plan.targets {
        if source.is_site() {
            let site = site.ok_or("Site updates require the verified Pangolin context")?;
            tokio::task::block_in_place(|| site.identity(target.ctid))?;
        }
        let line = one(pve, source, plan, target, site, say).await.map_err(|error| {
            format!(
                "{error}\nUpdate paused. Update the image again to finish it; it continues from what Proxmox shows."
            )
        })?;
        say(&line);
        lines.push(line);
    }
    Ok(lines)
}

/// Distinguish a reviewed reinstall from a root already made by that operation.
/// The temporary holder carries the operation through interrupted replacement.
const REPLACEMENT_OPERATION: &str = "replacement-operation ";

async fn replacement_operation(pve: &impl Pve, node: &str, ctid: u32) -> Result<String, String> {
    let containers = pve
        .get(&format!("/nodes/{node}/lxc"), &[])
        .await
        .map_err(|e| format!("Replacement inventory: {e}"))?;
    let containers = containers.as_array().ok_or("Replacement inventory is unavailable")?;
    if containers
        .iter()
        .any(|entry| entry["vmid"].as_u64() == Some(u64::from(recreate::HOLDER_ID)))
    {
        let holder = pve
            .get(&format!("/nodes/{node}/lxc/{}/config", recreate::HOLDER_ID), &[])
            .await
            .map_err(|e| format!("Replacement holder: {e}"))?;
        let description = text(&holder, "description");
        if recreate::holder_of(description) != Some(ctid) {
            return Err("The shared replacement holder is busy; finish its update first".to_owned());
        }
        if let Some(operation) = description
            .lines()
            .find_map(|line| line.strip_prefix(REPLACEMENT_OPERATION))
        {
            return uuid::Uuid::parse_str(operation)
                .map(|id| id.to_string())
                .map_err(|_| "Replacement holder has an invalid operation identity".to_owned());
        }
    }
    Ok(uuid::Uuid::new_v4().to_string())
}

async fn one(
    pve: &impl Pve,
    source: &Source,
    plan: &Plan,
    target: &Instance,
    site: Option<&dyn SiteReady>,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<String, String> {
    let (ctid, node) = (target.ctid, target.node.as_str());
    let now = instance::read_on(pve, node, ctid).await?;
    if deploy::initializing(&now.description) {
        return Err(format!(
            "CT {ctid}: initialization is unfinished; image replacement is refused"
        ));
    }
    if now.mac != target.mac || !node::same_net(&now.network, &target.network) {
        return Err(format!("CT {ctid} changed since review; review again"));
    }
    let operation = replacement_operation(pve, node, ctid).await?;
    let description = format!(
        "{}\n{REPLACEMENT_OPERATION}{operation}",
        deploy::description(source, &plan.image, &plan.digest)
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
    let untouched = !now.held && now.description.trim_end() != description.trim_end();
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
        node,
        ctid,
        hostname: &target.hostname,
        ostemplate: &ostemplate,
        pool: &target.pool,
        storage: &target.root_storage,
        disk_gib: target.root_gib,
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
    let options = |mount: &str| mount.split_once(',').map(|(_, rest)| rest.to_owned());
    if new.held
        || new.description.trim_end() != description.trim_end()
        || new.hostname != target.hostname
        || new.mac != target.mac
        || !node::same_net(&new.network, &target.network)
        || options(&new.home_mount) != options(&target.home_mount)
        || !new.home_volume.contains(&format!("-{ctid}-"))
    {
        return Err(format!(
            "CT {ctid}: the new container's home, network or identity differs from the old one"
        ));
    }
    // Autostart only now: a holder or a homeless container must never start
    // on its own.
    if new.onboot != target.onboot {
        let cfg = pve
            .get(&format!("/nodes/{node}/lxc/{ctid}/config"), &[])
            .await
            .map_err(|e| format!("CT {ctid} config: {e}"))?;
        pve.put(
            &format!("/nodes/{node}/lxc/{ctid}/config"),
            &[
                ("onboot", if target.onboot { "1" } else { "0" }),
                ("digest", text(&cfg, "digest")),
            ],
        )
        .await
        .map_err(|e| format!("CT {ctid} autostart: {e}"))?;
    }
    say(&format!("{node}: CT {ctid} · Start"));
    let start = target.running || target.onboot;
    if start {
        if !new.running {
            node::power(pve, node, ctid, "start").await?;
        }
        say(&deploy::ready(source, site, ctid, &target.address, say).await?);
    }
    node::remove_old_images(pve, node, &plan.template_storage, &format!("hs-{ctid}-"), "", &|line| {
        say(line)
    })
    .await;
    Ok(format!(
        "{node}: {} · CT {ctid} · {} · home {}{}",
        target.hostname,
        plan.image,
        new.home_volume,
        if start { "" } else { " · stopped" }
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;

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
