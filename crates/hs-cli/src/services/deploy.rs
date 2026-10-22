//! The first deployment of a manifest-defined service, through the deploy
//! token only: one unprivileged container per chosen node, made from a
//! published release of the manifest's image, each with its own new
//! Proxmox volume at `/home/app` (`mp0`) before its first start.
//!
//! Nothing changes before the operator applies a reviewed [`Plan`]. The
//! registry records each instance before its container is made and after
//! each step, so an interrupted run is resumed from what it recorded, and
//! a container is reused only when its description and tag prove that this
//! service made it. Nothing here stops, replaces or removes a container or
//! a volume.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::path::Path;
use std::time::{Duration, Instant};

use hs_core::pve::Pve;
use hs_core::registry::{Published, Resolved};
use hs_deploy::config::DeployConfig;
use hs_deploy::node::{self, NewContainer};
use serde_json::Value;

use super::manifest::{Healthcheck, MOUNT, Manifest};
use super::registry::{self, Deployment, Desired, Instance, Operation, Stage};

/// How long a started instance may take to pass its health check.
const HEALTH_LIMIT: Duration = Duration::from_secs(180);
const HEALTH_RETRY: Duration = Duration::from_secs(2);
/// How many published releases are offered.
const RELEASES: usize = 10;

/// The manifest revision a deployment is made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub repository: String,
    pub branch: String,
    pub commit: String,
    pub path: String,
    pub blob: String,
    pub manifest: Manifest,
}

impl Source {
    /// `REGISTRY/PATH:VERSION` of the manifest's image.
    #[must_use]
    pub fn image(&self, version: &str) -> String {
        format!("{}:{version}", self.manifest.image_repository)
    }
}

/// A storage on a node that the deploy token may make volumes on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStorage {
    pub node: String,
    pub storage: String,
    /// Free and total bytes, as Proxmox reports them.
    pub available: u64,
    pub total: u64,
}

/// What the cluster and the registry offer.
#[derive(Debug, Clone)]
pub struct Cluster {
    pub version: String,
    /// The online nodes.
    pub nodes: Vec<String>,
    pub storages: Vec<NodeStorage>,
    /// The nodes whose template storage takes images.
    pub templates: BTreeSet<String>,
    /// Published numeric releases, newest first.
    pub releases: Vec<Published>,
}

/// What the operator chose; numbers as typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Choice {
    pub version: String,
    pub nodes: Vec<String>,
    /// `NODE:STORAGE` entries; each node uses its own.
    pub root_storages: Vec<String>,
    pub home_storages: Vec<String>,
    /// One CTID per chosen node, in the nodes' order.
    pub ctids: String,
    pub cores: String,
    pub memory_mib: String,
    pub root_gib: String,
    pub home_gib: String,
    /// `KEY=value` entries separated by spaces.
    pub environment: String,
}

/// What applying would do; nothing is changed by making it.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub image: String,
    pub resolved: Option<Resolved>,
    pub instances: Vec<Instance>,
    /// The nodes the image is pulled on.
    pub pulls: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub bridge: String,
    pub cidr: u8,
    pub gateway: String,
    pub nameserver: Option<String>,
    pub template_storage: String,
    /// The instances an unfinished run left, reused.
    pub resumed: Vec<u32>,
    pub problems: Vec<String>,
}

fn stamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// The hostname of the instance on `node`: the service ID, with the node
/// when the service may run on several.
#[must_use]
pub fn hostname(manifest: &Manifest, node: &str) -> String {
    if manifest.multi_node {
        format!("{}-{node}", manifest.id)
    } else {
        manifest.id.clone()
    }
}

fn description(source: &Source, image: &str, digest: &str) -> String {
    format!(
        "HomeStack service {}\nrepository {}\nmanifest {} {}\ncommit {}\nimage {image}\ndigest {digest}\nmade by homestack",
        source.manifest.id, source.repository, source.path, source.blob, source.commit
    )
}

/// Whether container configuration `config` was made by this service.
fn made_by(source: &Source, config: &Value) -> bool {
    let text = config["description"].as_str().unwrap_or_default();
    let has = |wanted: &str| text.lines().any(|line| line.trim() == wanted);
    has(&format!("HomeStack service {}", source.manifest.id))
        && has(&format!("repository {}", source.repository))
        && has("made by homestack")
}

/// The online nodes, the storages the deploy token may use and the
/// published releases. Read only.
pub async fn survey(config: &DeployConfig, pve: &impl Pve, source: &Source) -> Result<Cluster, String> {
    let version = pve
        .get("/version", &[])
        .await
        .map_err(|e| format!("Proxmox version: {e}"))?["version"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let listed = pve.get("/nodes", &[]).await.map_err(|e| format!("nodes: {e}"))?;
    let mut nodes: Vec<String> = listed
        .as_array()
        .into_iter()
        .flatten()
        .filter(|node| node["status"] == "online")
        .filter_map(|node| node["node"].as_str().map(str::to_owned))
        .collect();
    nodes.sort();
    let permissions = pve
        .get("/access/permissions", &[])
        .await
        .map_err(|e| format!("token permissions: {e}"))?;
    let may_allocate = |storage: &str| {
        [format!("/storage/{storage}"), "/storage".to_owned(), "/".to_owned()]
            .iter()
            .any(|path| permissions[path.as_str()]["Datastore.AllocateSpace"].as_u64() == Some(1))
    };
    let mut storages = Vec::new();
    let mut templates = BTreeSet::new();
    for node in &nodes {
        let listed = pve
            .get(&format!("/nodes/{node}/storage"), &[("enabled", "1")])
            .await
            .map_err(|e| format!("{node} storages: {e}"))?;
        for item in listed.as_array().into_iter().flatten() {
            let Some(id) = item["storage"].as_str() else { continue };
            let content: Vec<&str> = item["content"].as_str().unwrap_or_default().split(',').collect();
            if item["active"].as_u64() == Some(0) {
                continue;
            }
            if content.contains(&"vztmpl") && id == config.template_storage {
                templates.insert(node.clone());
            }
            if content.contains(&"rootdir") && may_allocate(id) {
                storages.push(NodeStorage {
                    node: node.clone(),
                    storage: id.to_owned(),
                    available: item["avail"].as_u64().unwrap_or(0),
                    total: item["total"].as_u64().unwrap_or(0),
                });
            }
        }
    }
    let releases = hs_core::registry::recent(&source.image("latest"), RELEASES, &[]).await?;
    Ok(Cluster {
        version,
        nodes,
        storages,
        templates,
        releases,
    })
}

fn number(text: &str, what: &str, range: std::ops::RangeInclusive<u32>) -> Result<u32, String> {
    text.trim()
        .parse::<u32>()
        .ok()
        .filter(|n| range.contains(n))
        .ok_or_else(|| {
            format!(
                "{what} must be a whole number from {} to {}",
                range.start(),
                range.end()
            )
        })
}

/// Names that look like secrets: the container configuration is readable
/// in Proxmox, and HomeStack has no secret transport for services.
fn secret_name(key: &str) -> bool {
    ["TOKEN", "SECRET", "PASSWORD", "PASSWD", "KEY", "CREDENTIAL"]
        .iter()
        .any(|word| key.contains(word))
}

/// `KEY=value` entries separated by spaces.
pub fn environment(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut entries: Vec<(String, String)> = Vec::new();
    for entry in text.split_whitespace() {
        let (key, value) = entry
            .split_once('=')
            .ok_or_else(|| format!("{entry}: an environment entry is KEY=value"))?;
        let valid = key.bytes().next().is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
            && key
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
        if !valid {
            return Err(format!("{key}: a variable name is A-Z, 0-9 and _"));
        }
        if secret_name(key) {
            return Err(format!(
                "{key} looks like a secret; HomeStack cannot pass secrets to a service yet, and the container \
                 configuration is readable in Proxmox"
            ));
        }
        if entries.iter().any(|(known, _)| known == key) {
            return Err(format!("{key} is given twice"));
        }
        entries.push((key.to_owned(), value.to_owned()));
    }
    Ok(entries)
}

/// Whether `ctid` is free in the whole cluster.
async fn free(pve: &impl Pve, ctid: u32) -> Result<bool, String> {
    let id = ctid.to_string();
    match pve.get("/cluster/nextid", &[("vmid", id.as_str())]).await {
        Ok(_) => Ok(true),
        // Proxmox refuses a CTID in use with a 400 and no further detail.
        Err(hs_pve::PveError::Api { status: 400, .. }) => Ok(false),
        Err(error) => Err(format!("CT {ctid}: cannot check whether it is free: {error}")),
    }
}

/// Container `ctid` and its configuration when the token sees it.
async fn config_of(pve: &impl Pve, ctid: u32) -> Result<Option<(node::Container, Value)>, String> {
    let Some(found) = node::container(pve, ctid).await? else {
        return Ok(None);
    };
    let config = pve
        .get(&format!("/nodes/{}/lxc/{ctid}/config", found.node), &[])
        .await
        .map_err(|e| format!("CT {ctid} config: {e}"))?;
    Ok(Some((found, config)))
}

/// `count` free CTIDs from the start of the API servers' block (102001
/// for servers at 102001-102003), each with an address in the network.
pub async fn automatic(config: &DeployConfig, pve: &impl Pve, count: usize) -> Result<Vec<u32>, String> {
    let first = config
        .instances
        .iter()
        .map(|instance| instance.ctid)
        .min()
        .map_or(1001, |ctid| ctid / 1000 * 1000 + 1);
    let mut found = Vec::new();
    for ctid in first..first + 999 {
        if found.len() == count {
            break;
        }
        if address(config, ctid).is_some() && free(pve, ctid).await? {
            found.push(ctid);
        }
    }
    if found.len() < count {
        return Err(format!("No free CTID from {first}; give them in CTID"));
    }
    Ok(found)
}

/// The address of `ctid` in the deployment's network.
fn address(config: &DeployConfig, ctid: u32) -> Option<Ipv4Addr> {
    let network = hs_model::config::Network {
        prefix: String::new(),
        cidr: config.cidr,
        gateway: config.gateway.to_string(),
        dns: Vec::new(),
    };
    hs_model::address::address_of(ctid, &network)
}

/// Plan `choice`; `record` is the registry's record of this service.
#[allow(clippy::too_many_lines)]
pub async fn plan(
    config: &DeployConfig,
    pve: &impl Pve,
    source: &Source,
    cluster: &Cluster,
    choice: &Choice,
    record: Option<&Deployment>,
) -> Plan {
    let manifest = &source.manifest;
    let mut plan = Plan {
        image: source.image(&choice.version),
        bridge: config.bridge.clone(),
        cidr: config.cidr,
        gateway: config.gateway.to_string(),
        nameserver: config.nameserver.map(|ip| ip.to_string()),
        template_storage: config.template_storage.clone(),
        ..Plan::default()
    };
    let problems = &mut plan.problems;
    if let Some(record) = record
        && record.operation.is_none()
    {
        problems.push(format!(
            "{} is already deployed; updating or removing it is not available yet",
            manifest.id
        ));
        return plan;
    }
    if !cluster.releases.iter().any(|release| release.image == plan.image) {
        problems.push("Choose a published release".to_owned());
    } else {
        match hs_core::registry::resolve(&plan.image).await {
            Ok(resolved) => plan.resolved = Some(resolved),
            Err(error) => problems.push(error),
        }
    }
    if choice.nodes.is_empty() {
        problems.push("Tick at least one node".to_owned());
    }
    for node in &choice.nodes {
        let prefix = format!("{node}:");
        if choice.root_storages.iter().filter(|p| p.starts_with(&prefix)).count() > 1 {
            problems.push(format!("{node}: tick one storage"));
        }
    }
    if !manifest.multi_node && choice.nodes.len() > 1 {
        problems.push(format!(
            "{} runs on exactly one node (multi_node: false); tick one",
            manifest.id
        ));
    }
    let sizes = (
        number(&choice.cores, "Cores", 1..=64),
        number(&choice.memory_mib, "Memory (MiB)", 64..=1_048_576),
        number(&choice.root_gib, "Container disk (GiB)", 1..=1024),
        number(&choice.home_gib, "Home volume (GiB)", 1..=16_384),
    );
    let (cores, memory_mib, root_gib, home_gib) = match sizes {
        (Ok(cores), Ok(memory), Ok(root), Ok(home)) => (cores, memory, root, home),
        (cores, memory, root, home) => {
            problems.extend(
                [cores.err(), memory.err(), root.err(), home.err()]
                    .into_iter()
                    .flatten(),
            );
            (0, 0, 0, 0)
        }
    };
    match environment(&choice.environment) {
        Ok(entries) => plan.environment = entries,
        Err(error) => problems.push(error),
    }
    let mut ctids: Vec<String> = choice
        .ctids
        .split([',', ' '])
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect();
    if ctids.is_empty() {
        match automatic(config, pve, choice.nodes.len()).await {
            Ok(found) => ctids = found.iter().map(u32::to_string).collect(),
            Err(error) => problems.push(error),
        }
    }
    if ctids.len() != choice.nodes.len() {
        problems.push(format!(
            "Give one CTID per ticked node ({} ticked, {} given)",
            choice.nodes.len(),
            ctids.len()
        ));
    }
    let gib = 1024 * 1024 * 1024;
    let mut seen = BTreeSet::new();
    for (node, ctid) in choice.nodes.iter().zip(&ctids) {
        let Ok(ctid) = ctid.parse::<u32>() else {
            problems.push(format!(
                "{ctid} is not a CTID: a number of the network's addresses, like 102240"
            ));
            continue;
        };
        if !seen.insert(ctid) {
            problems.push(format!("CT {ctid} is given twice"));
            continue;
        }
        if !cluster.nodes.contains(node) {
            problems.push(format!("{node} is not an online node"));
            continue;
        }
        let Some(ip) = address(config, ctid) else {
            problems.push(format!(
                "CT {ctid} has no address in {}/{}: from 1000 the CTID is the address's last two octets (102010 is .102.10)",
                config.gateway, config.cidr
            ));
            continue;
        };
        if !cluster.templates.contains(node) {
            problems.push(format!(
                "{node}: the image storage {} is not there",
                config.template_storage
            ));
        }
        let recorded = record.and_then(|r| r.instances.iter().find(|i| i.ctid == ctid));
        if let Some(recorded) = recorded.filter(|i| i.stage != Stage::Planned) {
            // A container an unfinished run made keeps what it was made with.
            if recorded.node != *node {
                problems.push(format!(
                    "CT {ctid} was made on {} by the unfinished run; keep it there",
                    recorded.node
                ));
                continue;
            }
            match config_of(pve, ctid).await {
                Ok(Some((found, current))) if found.node == *node && made_by(source, &current) => {
                    plan.resumed.push(ctid);
                    plan.instances.push(recorded.clone());
                }
                Ok(_) => problems.push(format!(
                    "CT {ctid} recorded by the unfinished run is gone or no longer this service's; inspect it"
                )),
                Err(error) => problems.push(error),
            }
            continue;
        }
        let pick = |wanted: &[String], bytes: u64, what: &str| -> Result<String, String> {
            let prefix = format!("{node}:");
            wanted
                .iter()
                .filter_map(|picked| picked.strip_prefix(&prefix))
                .find(|id| cluster.storages.iter().any(|s| s.node == *node && s.storage == *id))
                .map(str::to_owned)
                .ok_or_else(|| format!("{node}: tick a {what} storage of this node"))
                .and_then(|id| {
                    let available = cluster
                        .storages
                        .iter()
                        .find(|s| s.node == *node && s.storage == id)
                        .map_or(0, |s| s.available);
                    if available < bytes {
                        Err(format!("{node}: {id} has {} GiB free", available / gib))
                    } else {
                        Ok(id)
                    }
                })
        };
        let root_storage = pick(&choice.root_storages, u64::from(root_gib) * gib, "container disk");
        let home_storage = pick(&choice.home_storages, u64::from(home_gib) * gib, "home volume");
        let (root_storage, home_storage) = match (root_storage, home_storage) {
            (Ok(root), Ok(home)) => (root, home),
            (root, home) => {
                problems.extend([root.err(), home.err()].into_iter().flatten());
                continue;
            }
        };
        let mut stage = Stage::Planned;
        match free(pve, ctid).await {
            Ok(true) => {}
            // Made by a run that stopped before it recorded the container.
            Ok(false) => match config_of(pve, ctid).await {
                Ok(Some((found, current)))
                    if recorded.is_some() && found.node == *node && made_by(source, &current) =>
                {
                    plan.resumed.push(ctid);
                    stage = Stage::Created;
                }
                Ok(_) => problems.push(format!("CT {ctid} is taken")),
                Err(error) => problems.push(error),
            },
            Err(error) => problems.push(error),
        }
        plan.instances.push(Instance {
            node: node.clone(),
            ctid,
            hostname: hostname(manifest, node),
            address: ip.to_string(),
            mac: String::new(),
            pool: config.pool.clone(),
            tags: vec![node::SERVER_TAG.to_owned()],
            cores,
            memory_mib,
            root_storage,
            root_gib,
            home_storage,
            home_gib,
            root_volume: String::new(),
            home_volume: String::new(),
            stage,
            verified: None,
            desired: Desired::Running,
        });
    }
    if let Some(record) = record {
        if record.image_version != choice.version && record.instances.iter().any(|i| i.stage != Stage::Planned) {
            problems.push(format!(
                "The unfinished run made containers from {}; keep that version",
                record.image_version
            ));
        }
        for left in record.instances.iter().filter(|i| i.stage != Stage::Planned) {
            if !plan.instances.iter().any(|i| i.ctid == left.ctid) {
                problems.push(format!(
                    "CT {} on {} from the unfinished run is not in this plan; keep its node and CTID",
                    left.ctid, left.node
                ));
            }
        }
    }
    let file = node::image_file(&plan.image).unwrap_or_default();
    let volid = format!("{}:vztmpl/{file}", config.template_storage);
    for instance in plan.instances.iter().filter(|i| i.stage != Stage::Ready) {
        let content = pve
            .get(
                &format!("/nodes/{}/storage/{}/content", instance.node, config.template_storage),
                &[("content", "vztmpl")],
            )
            .await;
        let present = content.is_ok_and(|content| {
            content
                .as_array()
                .into_iter()
                .flatten()
                .any(|item| item["volid"].as_str() == Some(volid.as_str()))
        });
        if !present && !plan.pulls.contains(&instance.node) {
            plan.pulls.push(instance.node.clone());
        }
    }
    plan
}

/// The health check's port number.
fn port(manifest: &Manifest, name: &str) -> Option<u16> {
    manifest.ports.iter().find(|p| p.name == name).map(|p| p.port)
}

/// What the health check of `manifest` is, for the review.
#[must_use]
pub fn health_text(manifest: &Manifest) -> String {
    match &manifest.healthcheck {
        Some(Healthcheck::Http {
            port: name,
            path,
            expected_status,
        }) => format!(
            "HTTP GET {path} on port {} expects {expected_status}",
            port(manifest, name).unwrap_or_default()
        ),
        Some(Healthcheck::Tcp { port: name }) => format!(
            "TCP connect to port {} (the port opens; application readiness unverified)",
            port(manifest, name).unwrap_or_default()
        ),
        None => "unverified: the manifest declares no health check".to_owned(),
    }
}

/// One probe of the health check; what it showed.
async fn probe(manifest: &Manifest, address: &str) -> Result<String, String> {
    match &manifest.healthcheck {
        Some(Healthcheck::Http {
            port: name,
            path,
            expected_status,
        }) => {
            let number = port(manifest, name).ok_or("the health check's port is not declared")?;
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|e| e.to_string())?;
            let status = client
                .get(format!("http://{address}:{number}{path}"))
                .send()
                .await
                .map_err(|e| e.to_string())?
                .status()
                .as_u16();
            if status == *expected_status {
                Ok(format!("GET {path} answered {status}"))
            } else {
                Err(format!("GET {path} answered {status}, expected {expected_status}"))
            }
        }
        Some(Healthcheck::Tcp { port: name }) => {
            let number = port(manifest, name).ok_or("the health check's port is not declared")?;
            let target: std::net::SocketAddr = format!("{address}:{number}").parse().map_err(|_| "bad address")?;
            tokio::task::spawn_blocking(move || std::net::TcpStream::connect_timeout(&target, Duration::from_secs(5)))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "port {number} accepts connections; application readiness unverified"
            ))
        }
        None => Ok("unverified: no health check declared".to_owned()),
    }
}

/// Probe until the check passes or [`HEALTH_LIMIT`] runs out.
async fn healthy(manifest: &Manifest, address: &str) -> Result<String, String> {
    let started = Instant::now();
    loop {
        match probe(manifest, address).await {
            Ok(shown) => return Ok(shown),
            Err(error) if started.elapsed() >= HEALTH_LIMIT => {
                return Err(format!(
                    "no healthy answer within {} s: {error}",
                    HEALTH_LIMIT.as_secs()
                ));
            }
            Err(_) => tokio::time::sleep(HEALTH_RETRY).await,
        }
    }
}

/// A volume option (`rootfs`, `mp0`): its volume and its options.
fn volume(option: &str) -> (&str, Vec<(&str, &str)>) {
    let mut parts = option.split(',');
    let volid = parts.next().unwrap_or_default();
    let options = parts.filter_map(|part| part.split_once('=')).collect();
    (volid, options)
}

/// Check what Proxmox made: the root disk and the home volume on the
/// chosen storages, the home mounted at `/home/app`, unprivileged; the
/// volume IDs and the MAC address.
fn verify(instance: &Instance, config: &Value) -> Result<(String, String, String), String> {
    let ctid = instance.ctid;
    let (root, _) = volume(
        config["rootfs"]
            .as_str()
            .ok_or_else(|| format!("CT {ctid} has no root disk"))?,
    );
    let (home, options) = volume(config["mp0"].as_str().ok_or_else(|| format!("CT {ctid} has no mp0"))?);
    let owned = |volid: &str, storage: &str| {
        volid
            .split_once(':')
            .is_some_and(|(id, name)| id == storage && name.contains(&format!("-{ctid}-")))
    };
    if !owned(root, &instance.root_storage) {
        return Err(format!(
            "CT {ctid}: root disk {root} is not its own on {}",
            instance.root_storage
        ));
    }
    if !owned(home, &instance.home_storage) || home == root {
        return Err(format!(
            "CT {ctid}: home volume {home} is not its own on {}",
            instance.home_storage
        ));
    }
    if !options.contains(&("mp", MOUNT)) {
        return Err(format!("CT {ctid}: mp0 is not mounted at {MOUNT}"));
    }
    if config["unprivileged"].as_u64() != Some(1) {
        return Err(format!("CT {ctid} is not unprivileged"));
    }
    if let Some(recorded) = Some(&instance.home_volume).filter(|v| !v.is_empty())
        && recorded != home
    {
        return Err(format!("CT {ctid}: home volume is {home}, recorded {recorded}"));
    }
    let mac = config["net0"]
        .as_str()
        .and_then(node::extract_hwaddr)
        .ok_or_else(|| format!("CT {ctid} has no MAC address"))?;
    Ok((root.to_owned(), home.to_owned(), mac))
}

/// Record `step` (and changed instances) in the registry.
fn record(path: &Path, source: &Source, change: impl FnOnce(&mut Deployment)) -> Result<(), String> {
    registry::update(path, |registry| {
        let deployment = registry
            .deployments
            .iter_mut()
            .find(|d| d.repository == source.repository && d.service == source.manifest.id)
            .ok_or("the deployment record disappeared")?;
        change(deployment);
        Ok(())
    })
}

fn set_step(path: &Path, source: &Source, step: &str) -> Result<(), String> {
    record(path, source, |deployment| {
        if let Some(operation) = deployment.operation.as_mut() {
            step.clone_into(&mut operation.step);
        }
    })
}

fn set_instance(path: &Path, source: &Source, instance: &Instance) -> Result<(), String> {
    record(path, source, |deployment| {
        if let Some(found) = deployment.instances.iter_mut().find(|i| i.ctid == instance.ctid) {
            *found = instance.clone();
        }
    })
}

/// Apply a reviewed plan, one node at a time; it stops at the first
/// failure and leaves the record for a resumed run. Whether every instance
/// is ready, and what was done.
pub async fn apply(
    pve: &impl Pve,
    registry_path: &Path,
    source: &Source,
    plan: &Plan,
    say: &(dyn Fn(&str) + Send + Sync),
) -> (bool, Vec<String>) {
    let mut lines = Vec::new();
    match run(pve, registry_path, source, plan, say, &mut lines).await {
        Ok(()) => (true, lines),
        Err(error) => {
            let step = format!("failed: {error}");
            if let Err(saved) = set_step(registry_path, source, &step) {
                lines.push(format!("The registry could not record the failure: {saved}"));
            }
            lines.push(error);
            lines.push(
                "Nothing was removed. The registry keeps the run; deploy again to resume it after fixing the cause."
                    .to_owned(),
            );
            (false, lines)
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run(
    pve: &impl Pve,
    registry_path: &Path,
    source: &Source,
    plan: &Plan,
    say: &(dyn Fn(&str) + Send + Sync),
    lines: &mut Vec<String>,
) -> Result<(), String> {
    if !plan.problems.is_empty() || plan.instances.is_empty() {
        return Err("The plan has problems; review it again".to_owned());
    }
    let resolved = plan.resolved.as_ref().ok_or("The release was not resolved")?;
    let manifest = &source.manifest;
    let version = plan.image.rsplit_once(':').map_or("", |(_, tag)| tag).to_owned();
    let keys: Vec<String> = plan.environment.iter().map(|(key, _)| key.clone()).collect();
    registry::update(registry_path, |registry| {
        let at = registry
            .deployments
            .iter()
            .position(|d| d.repository == source.repository && d.service == manifest.id);
        if let Some(at) = at {
            if registry.deployments[at].operation.is_none() {
                return Err(format!("{} is already deployed", manifest.id));
            }
            registry.deployments.remove(at);
        }
        registry.deployments.push(Deployment {
            repository: source.repository.clone(),
            service: manifest.id.clone(),
            git_ref: source.branch.clone(),
            commit: source.commit.clone(),
            manifest_path: source.path.clone(),
            manifest_blob: source.blob.clone(),
            image_repository: manifest.image_repository.clone(),
            image_version: version.clone(),
            image_digest: resolved.digest.clone(),
            environment: keys.clone(),
            instances: plan.instances.clone(),
            operation: Some(Operation {
                kind: "deploy".to_owned(),
                step: "started".to_owned(),
                started: stamp(),
            }),
        });
        Ok(())
    })?;
    let text = description(source, &plan.image, &resolved.digest);
    let net0 = |address: &str| {
        format!(
            "name=eth0,bridge={},ip={address}/{},gw={}",
            plan.bridge, plan.cidr, plan.gateway
        )
    };
    for planned in &plan.instances {
        let mut instance = planned.clone();
        let ctid = instance.ctid;
        let node = instance.node.clone();
        say(&format!(
            "{node}: CT {ctid} {} at {}",
            instance.hostname, instance.address
        ));
        if instance.stage == Stage::Ready {
            let state = node::container(pve, ctid).await?.map(|c| c.status);
            if state.as_deref() != Some("running") {
                return Err(format!("CT {ctid} was ready but is not running now; inspect it"));
            }
            lines.push(format!("{node}: CT {ctid} was already ready; kept"));
            continue;
        }
        set_step(registry_path, source, &format!("image on {node}"))?;
        let ostemplate = node::ensure_image(pve, &node, &plan.template_storage, &plan.image, &|line| say(line)).await?;
        if instance.stage == Stage::Planned {
            set_step(registry_path, source, &format!("create CT {ctid}"))?;
            if !free(pve, ctid).await? {
                return Err(format!("CT {ctid} was taken after the review; nothing was made"));
            }
            let spec = NewContainer {
                node: &node,
                ctid,
                hostname: &instance.hostname,
                ostemplate: &ostemplate,
                pool: &instance.pool,
                storage: &instance.root_storage,
                disk_gib: instance.root_gib,
                memory_mib: instance.memory_mib,
                cores: instance.cores,
                net0: net0(&instance.address),
                nameserver: plan.nameserver.clone(),
                description: text.clone(),
                tag: Some(node::SERVER_TAG),
                mp0: Some(format!(
                    "{}:{},mp={MOUNT},backup=1",
                    instance.home_storage, instance.home_gib
                )),
            };
            node::create(pve, &spec, &|line| say(line)).await?;
        }
        let (found, current) = config_of(pve, ctid)
            .await?
            .ok_or_else(|| format!("CT {ctid} is not visible after it was made"))?;
        if found.node != node || !made_by(source, &current) {
            return Err(format!(
                "CT {ctid} is not this service's container; nothing more is done"
            ));
        }
        let (root, home, mac) = verify(&instance, &current)?;
        say(&format!("  root {root}, home {home} at {MOUNT}, MAC {mac}"));
        instance.root_volume = root;
        instance.home_volume = home;
        instance.mac = mac;
        instance.stage = Stage::Created;
        set_instance(registry_path, source, &instance)?;
        if !plan.environment.is_empty() {
            node::set_env(pve, &node, ctid, &plan.environment, &[]).await?;
            say(&format!("  environment: {}", keys.join(", ")));
        }
        if found.status != "running" {
            set_step(registry_path, source, &format!("start CT {ctid}"))?;
            say(&format!("  starting CT {ctid}"));
            node::power(pve, &node, ctid, "start").await?;
        }
        set_step(registry_path, source, &format!("health of CT {ctid}"))?;
        let first = healthy(manifest, &instance.address).await?;
        say(&format!("  {first}"));
        // A second start proves the service comes up on the data it made.
        set_step(registry_path, source, &format!("restart CT {ctid}"))?;
        say(&format!("  restarting CT {ctid} to check it starts on its own data"));
        node::power(pve, &node, ctid, "reboot").await?;
        let second = healthy(manifest, &instance.address).await?;
        say(&format!("  after the restart: {second}"));
        instance.stage = Stage::Ready;
        instance.verified = Some(stamp());
        set_instance(registry_path, source, &instance)?;
        lines.push(format!(
            "{node}: CT {ctid} {} at {} runs {}; {second}",
            instance.hostname, instance.address, plan.image
        ));
        lines.push(format!("{node}: CT {ctid} home {} at {MOUNT}", instance.home_volume));
    }
    record(registry_path, source, |deployment| deployment.operation = None)?;
    Ok(())
}
