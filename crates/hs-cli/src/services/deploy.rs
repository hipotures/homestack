//! Deploying instances of a manifest-defined service, through the deploy
//! token only: each instance is an independent unprivileged container made
//! from a published release of the manifest's image, with its own new
//! Proxmox volume at `/home/app` (`mp0`) before its first start. Its
//! description records the service, so Proxmox alone tells what is deployed.
//!
//! Nothing changes before the operator applies a reviewed [`Plan`]. Nothing
//! here stops, replaces or removes a container or a volume.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use hs_core::pve::Pve;
use hs_core::registry::{Published, Resolved};
use hs_deploy::config::{DeployConfig, InstanceSpec};
use hs_deploy::deploy::ApiBootstrap;
use hs_deploy::node::{self, NewContainer};
use serde_json::Value;

use super::manifest::{Healthcheck, MOUNT, Manifest};

/// How long a started instance may take to pass its health check.
const HEALTH_LIMIT: Duration = Duration::from_secs(180);
const HEALTH_RETRY: Duration = Duration::from_secs(2);
/// How many published releases are offered.
const RELEASES: usize = 10;
const API_IMAGE: &str = "ghcr.io/hipotures/homestack-server";
const SITE_IMAGE: &str = "ghcr.io/hipotures/homestack-pangolin-site";
const API_INIT_HOME: &str = "api-initialization-home ";
const SITE_INIT_HOME: &str = "site-initialization-home ";
/// The first Site image whose runtime selects `/home/app/pangolin-site`.
const SITE_MANAGED: (u64, u64, u64, u64) = (0, 1, 2, 0);

/// A non-secret PVE marker for unfinished initialization, never an instance registry.
pub(super) fn initializing(description: &str) -> bool {
    description
        .lines()
        .any(|line| line.starts_with(API_INIT_HOME) || line.starts_with(SITE_INIT_HOME))
}

/// Initialization of a new managed Pangolin Site connector, with the
/// Pangolin integration's verified context (Step 5 tunnel, Step 7 key and
/// Step 8 credential journal) and root access for the credential only.
/// Blocking; nothing here creates, replaces or destroys a container.
pub trait SiteInit {
    /// Read only: Site `name` of CT `ctid`, or why it cannot be; `home` is the
    /// home volume of an existing CT whose initialization resumes.
    fn review(&mut self, ctid: u32, name: &str, home: Option<&str>) -> Result<String, String>;
    /// Revalidate the context, then the journal, credential and Site, before
    /// the container starts.
    fn prepare(&mut self, node: &str, ctid: u32, name: &str, home: &str, say: &dyn Fn(&str)) -> Result<(), String>;
    /// Install the credential into the running container's persistent home.
    fn install(&mut self, node: &str, ctid: u32, say: &dyn Fn(&str)) -> Result<(), String>;
    /// Wait until exactly this Site is online; the journal records it.
    fn online(&mut self, ctid: u32, say: &dyn Fn(&str)) -> Result<String, String>;
}

/// A managed Site's readiness and removal: an outbound connector has no
/// inbound health endpoint, so its own Pangolin Site must be online; the Site
/// HomeStack made for it goes with it. Blocking.
pub trait SiteReady: Sync {
    /// Read only: CT `ctid` is a completed managed Site of the verified organization.
    fn identity(&self, ctid: u32) -> Result<String, String>;
    /// Wait until its Site is online.
    fn ready(&self, ctid: u32, say: &dyn Fn(&str)) -> Result<String, String>;
    /// Read only: what removing CT `ctid` removes from Pangolin and this desktop.
    fn removal(&self, ctid: u32) -> Result<String, String>;
    /// Delete its Site from Pangolin, then its credential and journal here.
    fn remove(&self, ctid: u32, say: &dyn Fn(&str)) -> Result<String, String>;
}

/// The service's readiness after a start: its own Site online for a Pangolin
/// Site connector, else the manifest's health check.
pub(super) async fn ready(
    source: &Source,
    site: Option<&dyn SiteReady>,
    ctid: u32,
    address: &str,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<String, String> {
    if source.is_site() {
        let site = site.ok_or("Site readiness requires the verified Pangolin context")?;
        return tokio::task::block_in_place(|| site.ready(ctid, say));
    }
    healthy(&source.manifest, address).await
}

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
    /// Refuse unsupported internal initializers before any creation.
    pub fn initialization_blocker(&self) -> Option<&'static str> {
        match self.manifest.image_repository.as_str() {
            API_IMAGE
                if self.repository != "hipotures/homestack"
                    || self.manifest.id != "homestack-api"
                    || self.path != ".homestack/homestack-api.yaml" =>
            {
                Some("API initialization requires the canonical HomeStack API manifest from hipotures/homestack.")
            }
            SITE_IMAGE
                if self.repository != "hipotures/homestack"
                    || self.manifest.id != "pangolin"
                    || self.path != ".homestack/pangolin-site.yaml" =>
            {
                Some("Site initialization requires the canonical Pangolin Site manifest from hipotures/homestack.")
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn is_api(&self) -> bool {
        self.manifest.image_repository == API_IMAGE
    }

    /// The Pangolin Site connector, whose readiness is its own Site online.
    #[must_use]
    pub fn is_site(&self) -> bool {
        self.manifest.image_repository == SITE_IMAGE
    }

    /// The PVE marker of an unfinished initialization of this service.
    fn marker(&self) -> Option<&'static str> {
        if self.is_api() {
            Some(API_INIT_HOME)
        } else if self.is_site() {
            Some(SITE_INIT_HOME)
        } else {
            None
        }
    }

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

/// What the cluster offers.
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
    /// `NODE:STORAGE` entries; each node gets one instance.
    pub placements: Vec<String>,
    /// One CTID per instance, in the placements' order; empty: the first free.
    pub ctids: String,
    pub cores: String,
    pub memory_mib: String,
    pub root_mib: String,
    pub home_mib: String,
    /// `KEY=value` entries separated by spaces.
    pub environment: String,
}

/// A container a plan makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInstance {
    pub node: String,
    pub ctid: u32,
    pub hostname: String,
    pub address: String,
    pub cores: u32,
    pub memory_mib: u32,
    pub root_storage: String,
    pub root_mib: u32,
    pub home_storage: String,
    pub home_mib: u32,
    /// Non-secret observed identity of an interrupted API initialization.
    pub resume: Option<Resume>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    pub home_mount: String,
    pub root: String,
    pub network: String,
    pub description: String,
}

impl NewInstance {
    fn api_spec(&self) -> Result<InstanceSpec, String> {
        Ok(InstanceSpec {
            name: self.hostname.clone(),
            node: self.node.clone(),
            ctid: self.ctid,
            address: self.address.parse().map_err(|_| "Invalid API instance address")?,
            storage: self.root_storage.clone(),
        })
    }
}

/// What applying would do; nothing is changed by making it.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub image: String,
    pub resolved: Option<Resolved>,
    pub instances: Vec<NewInstance>,
    /// The nodes the image is pulled on.
    pub pulls: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub bridge: String,
    pub cidr: u8,
    pub gateway: String,
    pub nameserver: Option<String>,
    pub template_storage: String,
    /// The pool the containers join.
    pub pool: String,
    pub problems: Vec<String>,
    /// What the Site initializer reviewed for each instance, in order.
    pub sites: Vec<String>,
    // File paths and non-secret settings only; credentials are loaded at apply.
    api_config: Option<DeployConfig>,
}

/// The first of `id-1`, `id-2`… no container in `taken` is named.
fn hostname(id: &str, taken: &BTreeSet<String>) -> String {
    (1..)
        .map(|n| format!("{id}-{n}"))
        .find(|name| !taken.contains(name))
        .unwrap_or_default()
}

pub(super) fn description(source: &Source, image: &str, digest: &str) -> String {
    let telemetry = match &source.manifest.healthcheck {
        Some(Healthcheck::Http { port: name, .. }) => port(&source.manifest, name)
            .map(|port| format!("\nstatus-port {port}"))
            .unwrap_or_default(),
        _ => String::new(),
    };
    format!(
        "HomeStack service {}\nrepository {}\nmanifest {} {}\ncommit {}\nimage {image}\ndigest {digest}\nmade by homestack{telemetry}",
        source.manifest.id, source.repository, source.path, source.blob, source.commit
    )
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

/// A size typed as `10M` or `1G` (a bare number is GiB), in MiB within `range`.
fn size(text: &str, what: &str, range: std::ops::RangeInclusive<u32>) -> Result<u32, String> {
    node::parse_size(text).filter(|mib| range.contains(mib)).ok_or_else(|| {
        format!(
            "{what} is a whole number followed by M or G, from {} to {}",
            node::show_size(*range.start()),
            node::show_size(*range.end())
        )
    })
}

/// Names that look like secrets: the container configuration is readable
/// in Proxmox, and HomeStack has no secret transport for services.
pub fn secret_name(key: &str) -> bool {
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

/// Plan new instances or explicit recovery of an interrupted new API or Site
/// initialization. `site` is the Pangolin context a Site connector requires.
#[allow(clippy::too_many_lines)]
pub async fn plan(
    config: &DeployConfig,
    pve: &impl Pve,
    source: &Source,
    cluster: &Cluster,
    choice: &Choice,
    mut site: Option<&mut dyn SiteInit>,
) -> Plan {
    let manifest = &source.manifest;
    let mut plan = Plan {
        image: source.image(&choice.version),
        bridge: config.bridge.clone(),
        cidr: config.cidr,
        gateway: config.gateway.to_string(),
        nameserver: config.nameserver.map(|ip| ip.to_string()),
        template_storage: config.template_storage.clone(),
        pool: config.pool.clone(),
        ..Plan::default()
    };
    if source.is_api() {
        plan.api_config = Some(config.clone());
    }
    let bootstrap = if source.is_api() {
        match ApiBootstrap::load(config) {
            Ok(bootstrap) => Some(bootstrap),
            Err(error) => {
                plan.problems.push(error);
                None
            }
        }
    } else {
        None
    };
    let problems = &mut plan.problems;
    if let Some(problem) = source.initialization_blocker() {
        problems.push(problem.to_owned());
    }
    if !cluster.releases.iter().any(|release| release.image == plan.image) {
        problems.push("Choose a published release".to_owned());
    } else {
        match hs_core::registry::resolve(&plan.image).await {
            Ok(resolved) => plan.resolved = Some(resolved),
            Err(error) => problems.push(error),
        }
    }
    if source.is_api() {
        if choice.placements.len() > 1 {
            problems.push("Deploy one independently configured API instance at a time".to_owned());
        }
        if hs_core::registry::version(&choice.version).is_none_or(|version| version < (0, 2, 10, 0)) {
            problems.push("Managed API deployment requires API image 0.2.10 or later".to_owned());
        }
        if config.pve.ca_file.is_some() {
            problems.push("Managed API initialization cannot install a custom PVE CA file; endpoints must use system-trusted certificates".to_owned());
        }
        if !choice.environment.trim().is_empty() {
            problems.push("API environment is supplied by authenticated bootstrap; leave Environment empty".to_owned());
        }
    }
    if source.is_site() {
        if choice.placements.len() > 1 {
            problems.push("Deploy one Site connector at a time; each gets its own Site and credential".to_owned());
        }
        if hs_core::registry::version(&choice.version).is_none_or(|version| version < SITE_MANAGED) {
            problems.push("Managed Site deployment requires Site image 0.1.2 or later".to_owned());
        }
        if !choice.environment.trim().is_empty() {
            problems.push("The Site runtime selector is set by initialization; leave Environment empty".to_owned());
        }
        if site.is_none() {
            problems.push("The verified Pangolin context (Step 5 tunnel, Step 7 key) is not open".to_owned());
        }
    }
    if choice.placements.is_empty() {
        problems.push("Tick at least one node and storage".to_owned());
    }
    let sizes = (
        number(&choice.cores, "Cores", 1..=64),
        number(&choice.memory_mib, "Memory (MiB)", 64..=1_048_576),
        size(&choice.root_mib, "Container disk", 64..=1024 * 1024),
        size(&choice.home_mib, "Home volume", 1..=16_384 * 1024),
    );
    let (cores, memory_mib, root_mib, home_mib) = match sizes {
        (Ok(cores), Ok(memory), Ok(root), Ok(home)) => (cores, memory, root, home),
        (cores, memory, root, home) => {
            problems.extend(
                [cores.err(), memory.err(), root.err(), home.err()]
                    .into_iter()
                    .flatten(),
            );
            return plan;
        }
    };
    match environment(&choice.environment) {
        Ok(entries) => plan.environment = entries,
        Err(error) => problems.push(error),
    }
    let placements: Vec<(String, String)> = choice
        .placements
        .iter()
        .filter_map(|p| p.split_once(':'))
        .map(|(node, storage)| (node.to_owned(), storage.to_owned()))
        .collect();
    let nodes: BTreeSet<&String> = placements.iter().map(|(node, _)| node).collect();
    if nodes.len() != placements.len() {
        problems.push("Tick one storage on each node; a node gets one instance".to_owned());
    }
    let mut ctids: Vec<String> = choice
        .ctids
        .split([',', ' '])
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect();
    if ctids.is_empty() {
        match automatic(config, pve, placements.len()).await {
            Ok(found) => ctids = found.iter().map(u32::to_string).collect(),
            Err(error) => problems.push(error),
        }
    }
    if ctids.len() != placements.len() {
        problems.push(format!(
            "Give one CTID per instance ({} instances, {} CTIDs given)",
            placements.len(),
            ctids.len()
        ));
    }
    let mut taken: BTreeSet<String> = match super::instance::names(pve).await {
        Ok(names) => names.into_iter().collect(),
        Err(error) => {
            problems.push(error);
            BTreeSet::new()
        }
    };
    let gib = 1024 * 1024 * 1024;
    // What the planned instances take of each storage.
    let mut used: std::collections::BTreeMap<(String, String), u64> = std::collections::BTreeMap::new();
    let mut seen = BTreeSet::new();
    for ((node, storage), ctid) in placements.iter().zip(&ctids) {
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
        let Some(free_bytes) = cluster
            .storages
            .iter()
            .find(|s| s.node == *node && s.storage == *storage)
            .map(|s| s.available)
        else {
            problems.push(format!("{node}: {storage} cannot hold containers"));
            continue;
        };
        let mut instance = NewInstance {
            node: node.clone(),
            ctid,
            hostname: hostname(&manifest.id, &taken),
            address: ip.to_string(),
            cores,
            memory_mib,
            root_storage: storage.clone(),
            root_mib,
            home_storage: storage.clone(),
            home_mib,
            resume: None,
        };
        match free(pve, ctid).await {
            Ok(true) => {}
            Ok(false) if source.marker().is_some() => {
                match resumable(
                    pve,
                    source,
                    &plan.image,
                    plan.resolved.as_ref().map(|r| r.digest.as_str()).unwrap_or_default(),
                    &plan.pool,
                    &instance,
                )
                .await
                {
                    Ok((name, resume)) => {
                        instance.hostname = name;
                        instance.resume = Some(resume);
                    }
                    Err(error) => problems.push(error),
                }
            }
            Ok(false) => problems.push(format!("CT {ctid} is taken")),
            Err(error) => problems.push(error),
        }
        if instance.resume.is_none() {
            let wanted = used.entry((node.clone(), storage.clone())).or_default();
            *wanted += u64::from(root_mib + home_mib) * 1024 * 1024;
            if free_bytes < *wanted {
                problems.push(format!("{node}: {storage} has {} GiB free", free_bytes / gib));
            }
        }
        if let Some(bootstrap) = &bootstrap {
            match instance.api_spec().and_then(|spec| bootstrap.validate_instance(&spec)) {
                Ok(()) => {}
                Err(error) => problems.push(error),
            }
        }
        if let Some(site) = site.as_deref_mut() {
            let home = instance.resume.as_ref().map(|resume| home_volume(&resume.home_mount));
            match site.review(ctid, &instance.hostname, home) {
                Ok(line) => plan.sites.push(line),
                Err(error) => problems.push(error),
            }
        }
        taken.insert(instance.hostname.clone());
        plan.instances.push(instance);
    }
    let file = node::image_file(&plan.image).unwrap_or_default();
    let volid = format!("{}:vztmpl/{file}", config.template_storage);
    for instance in &plan.instances {
        if plan.pulls.contains(&instance.node) {
            continue;
        }
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
        if !present {
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
pub(super) async fn healthy(manifest: &Manifest, address: &str) -> Result<String, String> {
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

/// The volume of a mount option, without its size or options.
fn home_volume(mount: &str) -> &str {
    mount.split(',').next().unwrap_or_default()
}

/// Check what Proxmox made: the root disk and the home volume on the
/// chosen storages, the home mounted at `/home/app`, unprivileged; the
/// volume IDs and the MAC address.
fn verify(instance: &NewInstance, config: &Value) -> Result<(String, String, String), String> {
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
    let mac = config["net0"]
        .as_str()
        .and_then(node::extract_hwaddr)
        .ok_or_else(|| format!("CT {ctid} has no MAC address"))?;
    Ok((root.to_owned(), home.to_owned(), mac))
}

async fn set_description(pve: &impl Pve, instance: &NewInstance, cfg: &Value, description: &str) -> Result<(), String> {
    pve.put(
        &format!("/nodes/{}/lxc/{}/config", instance.node, instance.ctid),
        &[
            ("description", description),
            (
                "digest",
                cfg["digest"].as_str().ok_or("PVE configuration revision is missing")?,
            ),
        ],
    )
    .await
    .map_err(|_| format!("CT {}: cannot record initialization state", instance.ctid))?;
    Ok(())
}

/// `description` without the initialization marker `marker`.
fn without(description: &str, marker: &str) -> String {
    description
        .lines()
        .filter(|line| !line.starts_with(marker))
        .collect::<Vec<_>>()
        .join("\n")
}

fn check_resume(instance: &NewInstance, cfg: &Value, expected: &Resume) -> Result<(), String> {
    let (root, _, _) = verify(instance, cfg)?;
    if cfg.get("lock").is_some()
        || cfg["mp0"].as_str() != Some(expected.home_mount.as_str())
        || root != expected.root
        || cfg["net0"].as_str() != Some(expected.network.as_str())
        || cfg["description"].as_str().map(str::trim_end) != Some(expected.description.trim_end())
        || cfg["hostname"].as_str() != Some(instance.hostname.as_str())
    {
        return Err(format!(
            "CT {}: home, network or ownership changed; initialization stopped",
            instance.ctid
        ));
    }
    Ok(())
}

/// Only the operator's explicit CTID can resume a matching unfinished API or
/// Site. Legacy containers and completed common services never satisfy this.
async fn resumable(
    pve: &impl Pve,
    source: &Source,
    image: &str,
    digest: &str,
    pool: &str,
    instance: &NewInstance,
) -> Result<(String, Resume), String> {
    let ctid = instance.ctid;
    let kind = source.marker().ok_or("This service has no initialization to resume")?;
    let found = node::container(pve, ctid)
        .await?
        .ok_or("Initialization container disappeared")?;
    if found.node != instance.node || found.pool != pool {
        return Err(format!(
            "CT {ctid}: node or pool differs; it is not eligible for initialization recovery"
        ));
    }
    let cfg = pve
        .get(&format!("/nodes/{}/lxc/{ctid}/config", instance.node), &[])
        .await
        .map_err(|_| format!("CT {ctid}: cannot inspect initialization"))?;
    let desc = cfg["description"].as_str().unwrap_or_default();
    if without(desc, kind) != description(source, image, digest) || cfg.get("lock").is_some() {
        return Err(format!("CT {ctid}: not a matching unfinished common deployment"));
    }
    let current = super::instance::parse(ctid, &instance.node, pool, &cfg, found.status == "running")?;
    let (root, _, _) = verify(instance, &cfg)?;
    let home_mount = cfg["mp0"].as_str().ok_or("Home is missing")?;
    let marker = desc.lines().find_map(|line| line.strip_prefix(kind));
    let pristine =
        found.status == "stopped" && !current.environment.iter().any(|(key, _)| key.starts_with("HOMESTACK_"));
    // A Site has nothing in its environment but the runtime selector.
    let foreign = source.is_site()
        && current
            .environment
            .iter()
            .any(|(key, value)| key != "PATH" && (key != "HOMESTACK_MANAGED_HOME" || value != "1"));
    if foreign {
        return Err(format!(
            "CT {ctid}: unexpected environment; Site initialization recovery refused"
        ));
    }
    if marker.is_some_and(|original| original != home_mount)
        || (marker.is_none() && !pristine)
        || current.address != instance.address
        || current.cores != instance.cores
        || current.memory_mib != instance.memory_mib
        || current.root_mib != instance.root_mib
        || current.home_mib != instance.home_mib
        || hs_deploy::recreate::mounts(&cfg)? != ["mp0"]
        || cfg.get("hookscript").is_some()
    {
        return Err(format!(
            "CT {ctid}: initialization recovery requires its original home and reviewed settings"
        ));
    }
    Ok((
        current.hostname,
        Resume {
            home_mount: home_mount.to_owned(),
            root,
            network: current.network,
            description: desc.to_owned(),
        },
    ))
}

/// A created Site container: the managed runtime selector before its first
/// start, then its own Site and credential, the start, the credential in its
/// home and the Site online. What showed it online.
async fn initialize_site(
    pve: &impl Pve,
    site: &mut dyn SiteInit,
    instance: &NewInstance,
    home: &str,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<String, String> {
    let (node, ctid) = (instance.node.as_str(), instance.ctid);
    let cfg = pve
        .get(&format!("/nodes/{node}/lxc/{ctid}/config"), &[])
        .await
        .map_err(|_| "cannot read the container configuration")?;
    let selector = ("HOMESTACK_MANAGED_HOME".to_owned(), "1".to_owned());
    let env = node::parse_env(cfg["env"].as_str().unwrap_or_default());
    if env.iter().any(|entry| entry.0 != "PATH" && *entry != selector) {
        return Err("unexpected container environment; nothing is changed".to_owned());
    }
    if !env.contains(&selector) {
        if pve_status(pve, node, ctid).await? != "stopped" {
            return Err("the container runs without the managed runtime selector".to_owned());
        }
        node::set_env(pve, node, ctid, std::slice::from_ref(&selector), &[]).await?;
        say("  managed runtime selector set before the first start");
    }
    // The credential reaches the container through its console, a shell inside it.
    let cfg = pve
        .get(&format!("/nodes/{node}/lxc/{ctid}/config"), &[])
        .await
        .map_err(|_| "cannot read the container configuration")?;
    if cfg["cmode"].as_str() != Some("shell") {
        pve.put(
            &format!("/nodes/{node}/lxc/{ctid}/config"),
            &[
                ("cmode", "shell"),
                ("digest", cfg["digest"].as_str().unwrap_or_default()),
            ],
        )
        .await
        .map_err(|e| format!("console mode: {e}"))?;
    }
    tokio::task::block_in_place(|| site.prepare(node, ctid, &instance.hostname, home_volume(home), say))?;
    if pve_status(pve, node, ctid).await? != "running" {
        say(&format!("  starting CT {ctid}"));
        node::power(pve, node, ctid, "start").await?;
    }
    tokio::task::block_in_place(|| site.install(node, ctid, say))?;
    tokio::task::block_in_place(|| site.online(ctid, say))
}

async fn pve_status(pve: &impl Pve, node: &str, ctid: u32) -> Result<String, String> {
    let power = pve
        .get(&format!("/nodes/{node}/lxc/{ctid}/status/current"), &[])
        .await
        .map_err(|_| format!("CT {ctid}: cannot read its power state"))?;
    Ok(power["status"].as_str().unwrap_or_default().to_owned())
}

/// Apply a reviewed plan, one instance at a time; it stops at the first
/// failure. Whether every instance is ready, and what was done.
pub async fn apply(
    pve: &impl Pve,
    source: &Source,
    plan: &Plan,
    site: Option<&mut dyn SiteInit>,
    say: &(dyn Fn(&str) + Send + Sync),
) -> (bool, Vec<String>) {
    let mut lines = Vec::new();
    match run(pve, source, plan, site, say, &mut lines).await {
        Ok(()) => (true, lines),
        Err(error) => {
            lines.push(error);
            lines.push(if source.marker().is_some() {
                "Nothing was removed. For an interrupted initialization, review deployment again with the same \
                 CTID, version, node and sizes to resume using its existing home. Do not remove or replace it."
                    .to_owned()
            } else {
                "Nothing was removed. A container made before the failure is listed as an instance; remove it or \
                 update its image."
                    .to_owned()
            });
            (false, lines)
        }
    }
}

async fn run(
    pve: &impl Pve,
    source: &Source,
    plan: &Plan,
    mut site: Option<&mut dyn SiteInit>,
    say: &(dyn Fn(&str) + Send + Sync),
    lines: &mut Vec<String>,
) -> Result<(), String> {
    if let Some(problem) = source.initialization_blocker() {
        return Err(problem.to_owned());
    }
    if !plan.problems.is_empty() || plan.instances.is_empty() {
        return Err("The plan has problems; review it again".to_owned());
    }
    let resolved = plan.resolved.as_ref().ok_or("The release was not resolved")?;
    let version = plan
        .image
        .rsplit_once(':')
        .map(|(_, version)| version)
        .unwrap_or_default();
    if source.is_site() {
        if plan.instances.len() != 1
            || plan.sites.len() != 1
            || source.image(version) != plan.image
            || hs_core::registry::version(version).is_none_or(|version| version < SITE_MANAGED)
            || !plan.environment.is_empty()
        {
            return Err("Review one Site connector with a managed-runtime image release".to_owned());
        }
        let site = site
            .as_deref_mut()
            .ok_or("The verified Pangolin context is not open; review again")?;
        // The Pangolin context, key and Site state are verified again before anything is made.
        for (instance, reviewed) in plan.instances.iter().zip(&plan.sites) {
            let home = instance.resume.as_ref().map(|resume| home_volume(&resume.home_mount));
            if site.review(instance.ctid, &instance.hostname, home)? != *reviewed {
                return Err("Pangolin Site state changed since review; review again".to_owned());
            }
        }
        let current = hs_core::registry::resolve(&plan.image).await?;
        if current.digest != resolved.digest || current.platform != resolved.platform {
            return Err("Site image changed since review; review again".to_owned());
        }
    }
    let bootstrap = if source.is_api() {
        let config = plan.api_config.as_ref().ok_or("API initialization was not reviewed")?;
        if plan.instances.len() != 1
            || source.image(version) != plan.image
            || hs_core::registry::version(version).is_none_or(|version| version < (0, 2, 10, 0))
        {
            return Err("Review one API instance with a managed-runtime image release".to_owned());
        }
        if config.pve.ca_file.is_some() || !plan.environment.is_empty() {
            return Err(
                "API bootstrap settings changed or include unsupported environment/CA overrides; review again"
                    .to_owned(),
            );
        }
        let bootstrap = ApiBootstrap::load(config)?;
        for instance in &plan.instances {
            bootstrap.validate_instance(&instance.api_spec()?)?;
        }
        let current = hs_core::registry::resolve(&plan.image).await?;
        if current.digest != resolved.digest || current.platform != resolved.platform {
            return Err("API image changed since review; review again".to_owned());
        }
        Some(bootstrap)
    } else {
        None
    };
    let manifest = &source.manifest;
    let keys: Vec<String> = plan.environment.iter().map(|(key, _)| key.clone()).collect();
    let text = description(source, &plan.image, &resolved.digest);
    let net0 = |address: &str| {
        format!(
            "name=eth0,bridge={},ip={address}/{},gw={}",
            plan.bridge, plan.cidr, plan.gateway
        )
    };
    for instance in &plan.instances {
        let ctid = instance.ctid;
        let node = instance.node.clone();
        say(&format!(
            "{node}: CT {ctid} {} at {}",
            instance.hostname, instance.address
        ));
        let existing = !free(pve, ctid).await?;
        if existing && instance.resume.is_none() {
            return Err(format!(
                "CT {ctid} was taken after review; review again to inspect initialization"
            ));
        }
        if !existing && instance.resume.is_some() {
            return Err(format!("CT {ctid} disappeared; refusing to create a replacement home"));
        }
        if existing {
            let (_, observed) = resumable(pve, source, &plan.image, &resolved.digest, &plan.pool, instance).await?;
            if Some(&observed) != instance.resume.as_ref() {
                return Err(format!("CT {ctid}: initialization state changed after review"));
            }
        }
        let ostemplate = if existing {
            String::new()
        } else {
            node::ensure_image(pve, &node, &plan.template_storage, &plan.image, &|line| say(line)).await?
        };
        let spec = NewContainer {
            node: &node,
            ctid,
            hostname: &instance.hostname,
            ostemplate: &ostemplate,
            pool: &plan.pool,
            storage: &instance.root_storage,
            disk_mib: instance.root_mib,
            memory_mib: instance.memory_mib,
            cores: instance.cores,
            net0: Some(net0(&instance.address)),
            // A host reboot must not start an API or Site before its initialization completes.
            onboot: source.marker().is_none(),
            nameserver: plan.nameserver.clone(),
            description: text.clone(),
            tag: Some(node::SERVER_TAG),
            mp0: Some(format!(
                "{}:{},mp={MOUNT},backup=1",
                instance.home_storage,
                node::gib(instance.home_mib)
            )),
        };
        if !existing {
            if !free(pve, ctid).await? {
                return Err(format!("CT {ctid} was taken after review; nothing was made"));
            }
            node::create(pve, &spec, &|line| say(line)).await?;
        }
        let current = pve
            .get(&format!("/nodes/{node}/lxc/{ctid}/config"), &[])
            .await
            .map_err(|e| format!("CT {ctid} config: {e}"))?;
        let (root, home, mac) = verify(instance, &current)?;
        say(&format!("  root {root}, home {home} at {MOUNT}, MAC {mac}"));
        if let Some(marker) = source.marker() {
            if let Some(expected) = &instance.resume {
                check_resume(instance, &current, expected)?;
            } else if current["description"].as_str().map(str::trim_end) != Some(text.trim_end()) {
                return Err(format!("CT {ctid}: common ownership changed; initialization stopped"));
            }
            let original_mount = current["mp0"].as_str().ok_or("Home is missing")?;
            let base = without(current["description"].as_str().ok_or("Ownership is missing")?, marker);
            let pending = format!("{base}\n{marker}{original_mount}");
            if current["description"].as_str().map(str::trim_end) != Some(pending.trim_end()) {
                set_description(pve, instance, &current, &pending).await?;
            }
            let done = if let Some(bootstrap) = &bootstrap {
                let initialize = bootstrap
                    .initialize(pve, &instance.api_spec()?, true, &|line| say(line))
                    .await;
                initialize.map_err(|error| format!("CT {ctid}: API initialization failed: {error}"))?;
                "authenticated HTTPS, stored configuration and PVE credentials verified after reboot".to_owned()
            } else {
                let site = site.as_deref_mut().ok_or("The verified Pangolin context is not open")?;
                let online = initialize_site(pve, site, instance, original_mount, say)
                    .await
                    .map_err(|error| format!("CT {ctid}: Site initialization failed: {error}"))?;
                format!("{online}; its credential in {MOUNT}/pangolin-site verified")
            };
            let after = pve
                .get(&format!("/nodes/{node}/lxc/{ctid}/config"), &[])
                .await
                .map_err(|_| "Initialization: cannot verify final container configuration")?;
            let expected = Resume {
                home_mount: original_mount.to_owned(),
                root: root.clone(),
                network: current["net0"].as_str().unwrap_or_default().to_owned(),
                description: pending,
            };
            check_resume(instance, &after, &expected)?;
            let power = pve
                .get(&format!("/nodes/{node}/lxc/{ctid}/status/current"), &[])
                .await
                .map_err(|_| "Initialization: cannot verify final power state")?;
            if power["status"] != "running" {
                return Err(format!("CT {ctid} is not running"));
            }
            let env = node::parse_env(after["env"].as_str().unwrap_or_default());
            if !env.contains(&("HOMESTACK_MANAGED_HOME".to_owned(), "1".to_owned()))
                || env
                    .iter()
                    .any(|(key, _)| key == "HOMESTACK_TLS_CERT_B64" || key == "HOMESTACK_TLS_KEY_B64")
            {
                return Err(format!("CT {ctid}: managed runtime or TLS cleanup verification failed"));
            }
            pve.put(
                &format!("/nodes/{node}/lxc/{ctid}/config"),
                &[
                    ("description", base.as_str()),
                    ("onboot", "1"),
                    (
                        "digest",
                        after["digest"]
                            .as_str()
                            .ok_or("PVE configuration revision is missing")?,
                    ),
                ],
            )
            .await
            .map_err(|_| format!("CT {ctid}: cannot finish initialization metadata"))?;
            lines.push(format!(
                "{node}: CT {ctid} {} at {} initialized; {done}; autostart enabled",
                instance.hostname, instance.address
            ));
            continue;
        }
        // The console opens a root shell; an image has no getty to log in.
        if current["cmode"].as_str() != Some("shell") {
            let digest = current["digest"].as_str().unwrap_or_default();
            pve.put(
                &format!("/nodes/{node}/lxc/{ctid}/config"),
                &[("cmode", "shell"), ("digest", digest)],
            )
            .await
            .map_err(|e| format!("CT {ctid} console: {e}"))?;
        }
        if !plan.environment.is_empty() {
            node::set_env(pve, &node, ctid, &plan.environment, &[]).await?;
            say(&format!("  environment: {}", keys.join(", ")));
        }
        say(&format!("  starting CT {ctid}"));
        node::power(pve, &node, ctid, "start").await?;
        let first = healthy(manifest, &instance.address).await?;
        say(&format!("  {first}"));
        // A second start proves the service comes up on the data it made.
        say(&format!("  restarting CT {ctid} to check it starts on its own data"));
        node::power(pve, &node, ctid, "reboot").await?;
        let second = healthy(manifest, &instance.address).await?;
        say(&format!("  after the restart: {second}"));
        lines.push(format!(
            "{node}: CT {ctid} {} at {} runs {}; {second}",
            instance.hostname, instance.address, plan.image
        ));
        lines.push(format!("{node}: CT {ctid} home {home} at {MOUNT}"));
    }
    Ok(())
}
