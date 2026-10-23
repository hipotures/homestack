//! Step 8: one Pangolin Site connector per selected Proxmox node, deployed
//! through the install screen (access › settings › review › deploy). Sites
//! are created with the Step 7 organization key through the Step 5 tunnel;
//! containers are made with the deploy token, as the API servers are
//! (`hs_deploy::node`). Each Site credential is generated and saved on this
//! desktop before Pangolin sees it, and reaches only its own container over
//! the root SSH session's standard input (`pct exec`), never a command
//! line, the environment, a description or a log.

pub mod managed;

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use hs_core::pve::Pve;
use hs_deploy::config::DeployConfig;
use hs_deploy::node::{self, NewContainer};
use hs_install::{Access, Root};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::install::{Event, Found, Mark, Method, Setting};
use super::installing::{Command, Setup};
use super::pangolin_key::{Client, Credential, Probe, Problem};
use super::pangolin_permanent::{Record, Stage};
use super::pangolin_store::{Storage, read_credential};
use super::pangolin_tunnel::Endpoint;
use crate::install_flow::Answer;

/// The connector image; a new release is a new `pangolin-site-X.Y.Z` tag.
pub const IMAGE: &str = "ghcr.io/hipotures/homestack-pangolin-site:0.1.1";
/// Every connector container carries it, and nothing else does.
pub const TAG: &str = "homestack-pangolin-site";
/// Inside the image: writes the configuration from standard input.
const TOOL: &str = "/usr/local/bin/homestack-site-credential";
/// How long all connectors may take to come online after the deployment.
const HEALTH_LIMIT: Duration = Duration::from_secs(180);
const HEALTH_RETRY: Duration = Duration::from_secs(5);
const PAGE: usize = 100;

/// What only the Pangolin wizard knows, handed to the install worker.
#[derive(Clone)]
pub struct Context {
    pub image: String,
    pub endpoint: Endpoint,
    /// The Pangolin server's SSH destination (Step 1).
    pub target: String,
    pub organization: String,
    /// Public endpoint read from app.dashboard_url on the Pangolin server.
    pub url: String,
    /// The Step 7 credential; its recovery record names the key.
    pub key: PathBuf,
    /// SHA-256 of server and organization, as Step 7 names its files.
    pub identity: String,
    /// `integrations/pangolin-sites.toml`: choices and the last result.
    pub summary: PathBuf,
}

impl Context {
    fn site_key(&self, node: &str) -> PathBuf {
        self.key
            .with_file_name(format!("pangolin-site-{}-{node}.key", self.identity))
    }
}

/// The settings of the screen, in this order.
const NODES: usize = 0;
const STORAGES: usize = 1;
const TEMPLATE: usize = 2;
const BRIDGE: usize = 3;

/// Non-secret choices and the last deployment, shown in Step 8.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Summary {
    pub image: String,
    pub target: String,
    pub organization: String,
    pub url: String,
    pub nodes: Vec<String>,
    pub storages: Vec<String>,
    pub template_storage: String,
    pub bridge: String,
    pub at: String,
    pub deployed: Vec<Deployed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Deployed {
    pub node: String,
    pub site: String,
    pub site_id: u64,
    pub ctid: u32,
    pub container: String,
    pub connection: String,
}

impl Summary {
    #[must_use]
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        toml::from_str(&text).ok()
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        let table = toml::Table::try_from(self).map_err(|_| "Cannot serialize the Site summary.".to_owned())?;
        super::pangolin::persist(path, &table)
    }

    /// The last deployment for this server and organization, all running
    /// and online when it ended (historical, not monitoring).
    #[must_use]
    pub fn healthy(&self, target: &str, organization: &str) -> bool {
        self.target == target
            && self.organization == organization
            && !self.deployed.is_empty()
            && self
                .deployed
                .iter()
                .all(|d| d.container == "running" && d.connection == "online")
    }
}

/// The non-secret journal of one Site (one node's, or one managed
/// instance's); its credential is the `ID.secret` file next to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SiteRecord {
    target: String,
    organization: String,
    node: String,
    site: String,
    newt_id: String,
    site_id: Option<u64>,
    ctid: Option<u32>,
    stage: SiteStage,
    at: String,
    /// A managed instance's home volume, recorded before Pangolin is asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    home: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
enum SiteStage {
    /// The credential is saved; Pangolin has not been asked.
    Prepared,
    /// Creation was sent; its outcome may be unknown.
    CreationRequested,
    Created,
    /// Managed instances only: the credential is in the container's home.
    Installed,
    /// Managed instances only: Pangolin reported the Site online.
    Online,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SiteStep {
    Reuse(u64),
    /// A creation of an earlier run that Pangolin did make.
    Adopt(u64),
    Create,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CtStep {
    Keep,
    Start,
    Create,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NodePlan {
    node: String,
    site: String,
    step: SiteStep,
    ctid: u32,
    storage: String,
    container: CtStep,
    pull: bool,
    /// The installed credential matches (a running container only).
    installed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Plan {
    url: String,
    template: String,
    bridge: String,
    nodes: Vec<NodePlan>,
    kept: Vec<String>,
    problems: Vec<String>,
}

/// The answers of the screen.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Choice {
    nodes: Vec<String>,
    storages: Vec<String>,
    template: Option<String>,
    bridge: Option<String>,
    url: String,
}

fn stamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn hostname(node: &str) -> String {
    format!("pangolin-{node}")
}

fn description(organization: &str, site: &str, image: &str) -> String {
    format!(
        "HomeStack Pangolin Site connector {site}\norganization {organization}\nsite {site}\nimage {image}\nmade by homestack"
    )
}

fn net0(bridge: &str) -> String {
    format!("name=eth0,bridge={bridge},ip=dhcp")
}

/// `https://host[:port]` with nothing after it: where the connectors reach Pangolin.
fn valid_url(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|parsed| {
        parsed.scheme() == "https"
            && parsed.host_str().is_some_and(|host| !host.is_empty())
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none()
            && matches!(parsed.path(), "" | "/")
            && !url.ends_with('/')
    })
}

fn api<T>(result: Result<T, Problem>) -> Result<T, String> {
    result.map_err(explain)
}

fn explain(problem: Problem) -> String {
    match problem {
        Problem::Rejected(_) => "The HomeStack API key was rejected. Refresh Step 7.".to_owned(),
        Problem::Forbidden(_) => "The HomeStack API key lacks a required Site action. Refresh Step 7.".to_owned(),
        Problem::Incompatible(_) => "Unexpected Pangolin API response; response omitted.".to_owned(),
        Problem::Timeout => "A Pangolin API request timed out.".to_owned(),
        Problem::Cancelled => "Cancelled.".to_owned(),
        _ => problem.message().to_owned(),
    }
}

/// A client for one group of requests (the shared deadline is per client).
fn client<'a>(endpoint: &'a Endpoint, key: &'a Credential, stop: &'a AtomicBool) -> Client<'a> {
    Client {
        endpoint,
        credential: key,
        stop,
        started: Instant::now(),
    }
}

/// The Step 7 key, from its recovery record: complete, for this server
/// and organization, and the saved file holding that very key.
fn api_key(ctx: &Context) -> Result<Credential, String> {
    let missing =
        "Step 7 has not saved a verified HomeStack API key for this server and organization. Complete Step 7 first.";
    let record: Record = Storage::open(&ctx.key)?.load()?.ok_or(missing)?;
    if record.stage != Stage::Complete
        || record.target != ctx.target
        || record.organization != ctx.organization
        || record.name != "homestack"
        || !record.credential_saved
        || !record.actions_verified
    {
        return Err(missing.to_owned());
    }
    let key = read_credential(&record.credential_path)?;
    if key.key_id().ok() != record.key_id.as_deref() {
        return Err("The saved HomeStack API key does not match its Step 7 record. Refresh Step 7.".to_owned());
    }
    Ok(key)
}

/// The key reads exactly the expected organization (getOrg).
fn organization(ctx: &Context, key: &Credential) -> Result<(), String> {
    let stop = AtomicBool::new(false);
    match api(client(&ctx.endpoint, key, &stop).get(&format!("/v1/org/{}", ctx.organization), Probe::Organization))? {
        (200, Some(data))
            if data
                .get("org")
                .and_then(|o| o.get("orgId"))
                .and_then(serde_json::Value::as_str)
                == Some(ctx.organization.as_str()) =>
        {
            Ok(())
        }
        _ => Err("The HomeStack API key cannot confirm the organization. Refresh Step 7.".to_owned()),
    }
}

/// Whether Site `site_id` is listed online (listSites).
fn online(endpoint: &Endpoint, key: &Credential, organization: &str, site_id: u64) -> Result<bool, String> {
    Ok(sites(endpoint, key, organization)?
        .iter()
        .any(|row| row.site_id == site_id && row.online == Some(true)))
}

/// One Site of the organization, as listed.
struct Listed {
    site_id: u64,
    nice: String,
    online: Option<bool>,
}

/// `GET /v1/org/{orgId}/sites`, every page (`page`/`pageSize`).
fn sites(endpoint: &Endpoint, key: &Credential, organization: &str) -> Result<Vec<Listed>, String> {
    let stop = AtomicBool::new(false);
    let client = client(endpoint, key, &stop);
    let mut found = Vec::new();
    for page in 1..=1000 {
        let path = format!("/v1/org/{organization}/sites?pageSize={PAGE}&page={page}");
        let data = match api(client.get(&path, Probe::Organization))? {
            (200, Some(data)) => data,
            (403, _) => return Err("The HomeStack API key cannot list Sites (listSites). Refresh Step 7.".to_owned()),
            _ => return Err("Unexpected Site listing; response omitted.".to_owned()),
        };
        let rows = data
            .get("sites")
            .and_then(serde_json::Value::as_array)
            .filter(|rows| rows.len() <= PAGE)
            .ok_or("Unexpected Site listing; response omitted.")?;
        for row in rows {
            let site_id = row.get("siteId").and_then(serde_json::Value::as_u64);
            let nice = row.get("niceId").and_then(serde_json::Value::as_str);
            let (Some(site_id), Some(nice)) = (site_id, nice) else {
                return Err("Unexpected Site listing; response omitted.".to_owned());
            };
            found.push(Listed {
                site_id,
                nice: nice.to_owned(),
                online: row.get("online").and_then(serde_json::Value::as_bool),
            });
        }
        if rows.len() < PAGE {
            return Ok(found);
        }
    }
    Err("Too many Sites to list.".to_owned())
}

/// One Site by its nice ID: its number and Newt ID; `None` when absent.
fn site(
    endpoint: &Endpoint,
    key: &Credential,
    organization: &str,
    nice: &str,
) -> Result<Option<(u64, String)>, String> {
    let stop = AtomicBool::new(false);
    let client = client(endpoint, key, &stop);
    match api(client.get(&format!("/v1/org/{organization}/site/{nice}"), Probe::Organization))? {
        (200, Some(data)) => {
            let site_id = data.get("siteId").and_then(serde_json::Value::as_u64);
            let newt = data
                .get("newtId")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let same_org = data.get("orgId").and_then(serde_json::Value::as_str) == Some(organization);
            match site_id {
                Some(id) if same_org && data.get("niceId").and_then(serde_json::Value::as_str) == Some(nice) => {
                    Ok(Some((id, newt.to_owned())))
                }
                _ => Err("Unexpected Site response; response omitted.".to_owned()),
            }
        }
        (404, _) => Ok(None),
        (403, _) => Err("The HomeStack API key cannot read Sites (getSite). Refresh Step 7.".to_owned()),
        _ => Err("Unexpected Site response; response omitted.".to_owned()),
    }
}

/// Random lowercase letters and digits, as Pangolin generates its own.
fn random(length: usize) -> Result<Zeroizing<String>, String> {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut source = std::fs::File::open("/dev/urandom").map_err(|_| "Cannot read /dev/urandom.")?;
    let mut out = Zeroizing::new(String::with_capacity(length));
    let mut byte = Zeroizing::new([0u8; 1]);
    while out.len() < length {
        source.read_exact(&mut *byte).map_err(|_| "Cannot read /dev/urandom.")?;
        // 252 = 7 × 36: rejecting the rest keeps every character equally likely.
        if byte[0] < 252 {
            out.push(char::from(ALPHABET[usize::from(byte[0] % 36)]));
        }
    }
    Ok(out)
}

/// The connector configuration file: endpoint, Newt ID and secret.
fn configuration(url: &str, credential: &Credential) -> Result<Zeroizing<String>, String> {
    let (id, secret) = credential.value.split_once('.').ok_or("Invalid Site credential.")?;
    let text = serde_json::to_string(&serde_json::json!({"endpoint": url, "id": id, "secret": secret}))
        .map_err(|_| "Invalid Site credential.")?;
    Ok(Zeroizing::new(format!("{text}\n")))
}

fn digest(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A guest or storage of `/cluster/resources`, as root sees them.
struct Guest {
    vmid: u32,
    node: String,
    kind: String,
    name: String,
    status: String,
    tags: Vec<String>,
}

struct Store {
    node: String,
    storage: String,
    content: Vec<String>,
}

fn cluster(root: &mut dyn Root) -> Result<(Vec<Guest>, Vec<Store>), String> {
    let output = root
        .run("pvesh get /cluster/resources --output-format json")
        .map_err(|e| e.to_string())?;
    if output.code != 0 {
        return Err(format!("cannot read the cluster's resources: {}", output.detail()));
    }
    let items: Vec<serde_json::Value> =
        serde_json::from_str(output.stdout.trim()).map_err(|_| "unexpected cluster resources".to_owned())?;
    let text = |item: &serde_json::Value, key: &str| item[key].as_str().unwrap_or_default().to_owned();
    let mut guests = Vec::new();
    let mut stores = Vec::new();
    for item in &items {
        match item["type"].as_str() {
            Some("qemu" | "lxc") => guests.push(Guest {
                vmid: item["vmid"].as_u64().and_then(|id| u32::try_from(id).ok()).unwrap_or(0),
                node: text(item, "node"),
                kind: text(item, "type"),
                name: text(item, "name"),
                status: text(item, "status"),
                tags: text(item, "tags")
                    .split([';', ','])
                    .map(str::trim)
                    .filter(|tag| !tag.is_empty())
                    .map(str::to_owned)
                    .collect(),
            }),
            Some("storage") if item["status"] == "available" => stores.push(Store {
                node: text(item, "node"),
                storage: text(item, "storage"),
                content: text(item, "content").split(',').map(str::to_owned).collect(),
            }),
            _ => {}
        }
    }
    Ok((guests, stores))
}

fn on_node(access: &Access, node: &str, command: &str) -> Result<String, String> {
    let local = access.topology.local_node().map(|n| n.name.clone()).unwrap_or_default();
    let target = access
        .topology
        .nodes
        .iter()
        .find(|n| n.name == node)
        .ok_or_else(|| format!("no node {node}"))?;
    Ok(hs_install::node_command(&local, target, command))
}

/// The receiver inside `ctid`; a managed instance's selects its persistent
/// home explicitly, since `pct exec` does not inherit PID 1's environment.
fn tool(ctid: u32, managed: bool) -> String {
    let selector = if managed { "env HOMESTACK_MANAGED_HOME=1 " } else { "" };
    format!("pct exec {ctid} -- {selector}{TOOL}")
}

/// The SHA-256 of the configuration installed in `ctid`, or `missing`.
fn installed(root: &mut dyn Root, access: &Access, node: &str, ctid: u32, managed: bool) -> Result<String, String> {
    let command = on_node(access, node, &format!("{} --digest", tool(ctid, managed)))?;
    let output = root.run(&command).map_err(|e| e.to_string())?;
    let value = output.stdout.trim();
    if output.code != 0 || !(value == "missing" || (value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())))
    {
        return Err(format!("CT {ctid}: cannot read the installed credential's digest"));
    }
    Ok(value.to_owned())
}

/// What the worker keeps between the screen's commands.
struct Session {
    root: Box<dyn Root>,
    access: Access,
    describe: String,
    settings: Vec<Setting>,
    reviewed: Option<(Choice, Plan)>,
}

/// The worker of the install screen in Sites mode.
pub(super) fn work(setup: &Setup, commands: &Receiver<Command>, tell: &(dyn Fn(Event) + Send + Sync)) {
    let Some(mut ctx) = setup.sites.clone() else {
        tell(Event::Failed(
            "The Pangolin wizard's tunnel is not available.".to_owned(),
        ));
        return;
    };
    let mut session: Option<Session> = None;
    while let Ok(command) = commands.recv() {
        match command {
            Command::Connect { target, .. } => {
                tell(Event::Busy(format!(
                    "Connecting to {target} (touch your key or give its PIN if it asks)"
                )));
                if ctx.url.is_empty() {
                    tell(Event::Busy(
                        "Reading the dashboard address from the Pangolin server".to_owned(),
                    ));
                    let discovery = super::pangolin::discover(
                        setup.system.as_ref(),
                        &ctx.target,
                        &setup.ssh_prefix,
                        &setup.stop,
                        &setup.sounds,
                        crate::touch::default_player(),
                    );
                    ctx.url = discovery.known("dashboard_url").unwrap_or_default().to_owned();
                }
                if !valid_url(&ctx.url) {
                    tell(Event::Failed("Cannot read a valid HTTPS app.dashboard_url from the Pangolin server configuration. Check the server configuration and rerun Step 3 discovery.".to_owned()));
                    continue;
                }
                let opened = setup.open(Method::Ssh, &target).map_err(|e| e.0).and_then(|mut root| {
                    tell(Event::Busy("Connected; reading the cluster".to_owned()));
                    let access = hs_install::probe(root.as_mut()).map_err(|e| e.to_string())?;
                    let describe = root.describe();
                    Ok(Session {
                        root,
                        access,
                        describe,
                        settings: Vec::new(),
                        reviewed: None,
                    })
                });
                match opened.and_then(|mut s| survey(setup, &ctx, &mut s).map(|found| (s, found))) {
                    Ok((started, found)) => {
                        session = Some(started);
                        tell(Event::Connected(found));
                    }
                    Err(error) => tell(Event::Failed(error)),
                }
            }
            Command::Rescan { answers, .. } => {
                let Some(s) = session.as_mut() else { continue };
                tell(Event::Busy("Rescanning the cluster".to_owned()));
                let choice = choice(&ctx, &s.settings, &answers);
                match survey(setup, &ctx, s) {
                    Ok(mut found) => {
                        keep_answers(&mut found.settings, &choice);
                        s.settings.clone_from(&found.settings);
                        tell(Event::Connected(found));
                    }
                    Err(error) => tell(Event::Failed(error)),
                }
            }
            Command::Review { answers, .. } => {
                let Some(s) = session.as_mut() else { continue };
                tell(Event::Busy("Reading Pangolin and the cluster".to_owned()));
                let choice = choice(&ctx, &s.settings, &answers);
                remember(&ctx, &choice);
                let plan = setup
                    .handle
                    .block_on(plan(setup, &ctx, s.root.as_mut(), &s.access, &choice));
                tell(Event::Reviewed {
                    sections: sections(&ctx, &plan),
                    problems: plan.problems.clone(),
                });
                s.reviewed = Some((choice, plan));
            }
            Command::Apply => {
                let Some(s) = session.as_mut() else { continue };
                let Some((choice, reviewed)) = s.reviewed.clone() else {
                    continue;
                };
                let (ok, lines) = setup.handle.block_on(deploy(setup, &ctx, s, &choice, &reviewed, tell));
                tell(Event::Done { ok, lines });
            }
            Command::Cancel => return,
        }
    }
}

/// The settings: nodes, connector storages, image storage, bridge and the
/// Connector placement choices from the Proxmox cluster.
fn survey(setup: &Setup, ctx: &Context, s: &mut Session) -> Result<Found, String> {
    let settings = hs_install::Settings {
        test: false,
        storages: Vec::new(),
        snippet_storage: None,
        service_storages: Vec::new(),
        template_storage: None,
        gold: None,
        bridge: None,
        adopt: Vec::new(),
        token_dir: setup.token_dir.clone(),
    };
    let mut questions = hs_install::questions(s.root.as_mut(), &s.access, &settings).map_err(|e| e.to_string())?;
    let draft = crate::install_flow::load_draft(&crate::install_flow::draft_path(&settings));
    crate::install_flow::apply_draft(&mut questions, &draft);
    let saved = Summary::load(&ctx.summary).filter(|saved| saved.target == ctx.target);
    let mut found_settings = Vec::new();
    let online: Vec<&str> = s
        .access
        .topology
        .nodes
        .iter()
        .filter(|node| node.online)
        .map(|node| node.name.as_str())
        .collect();
    let nodes: Vec<(String, String)> = online
        .iter()
        .map(|name| ((*name).to_owned(), format!("Site {}", hostname(name))))
        .collect();
    let picked = match &saved {
        Some(saved) if !saved.nodes.is_empty() => nodes
            .iter()
            .enumerate()
            .filter(|(_, (name, _))| saved.nodes.contains(name))
            .map(|(index, _)| index)
            .collect(),
        _ => (0..nodes.len()).collect(),
    };
    found_settings.push(Setting {
        list: false,
        title: "Nodes".to_owned(),
        note: "The nodes that get a Site connector: one Site pangolin-<node> and one container on each. Unticked nodes keep what they have.".to_owned(),
        multiple: true,
        choices: nodes,
        picked,
        text: None,
    });
    for (option, title, note) in [
        (
            "--service-storages",
            "Connector disks",
            "Where the connector containers' disks are created; each node uses the first ticked storage it has.",
        ),
        (
            "--template-storage",
            "Connector image",
            "Where the connector image is pulled on each node before it becomes a container.",
        ),
        (
            "--bridge",
            "Network bridge",
            "The bridge of the connectors' network; they get their address by DHCP and need only outbound access to Pangolin.",
        ),
    ] {
        let question = questions
            .iter()
            .find(|q| q.option == option)
            .ok_or_else(|| format!("the cluster offers no choice for {title}"))?;
        let choices: Vec<(String, String)> = question
            .choices
            .iter()
            .map(|c| (c.value.clone(), c.detail.clone()))
            .collect();
        let mut picked = question.defaults.clone();
        let remembered: Vec<&String> = match (&saved, option) {
            (Some(saved), "--service-storages") => saved.storages.iter().collect(),
            (Some(saved), "--template-storage") => vec![&saved.template_storage],
            (Some(saved), _) => vec![&saved.bridge],
            (None, _) => Vec::new(),
        };
        let kept: Vec<usize> = remembered
            .iter()
            .filter_map(|value| choices.iter().position(|(choice, _)| choice == *value))
            .collect();
        if !kept.is_empty() {
            picked = kept;
        }
        found_settings.push(Setting {
            list: false,
            title: title.to_owned(),
            note: note.to_owned(),
            multiple: question.multiple,
            choices,
            picked,
            text: None,
        });
    }
    s.settings.clone_from(&found_settings);
    Ok(Found {
        root: format!("{} · PVE {}", s.describe, s.access.version),
        check: vec![
            (
                Mark::Ok,
                "root".to_owned(),
                format!("{} answers, Proxmox VE {}", s.describe, s.access.version),
            ),
            (Mark::Ok, "nodes".to_owned(), s.access.topology.describe()),
        ],
        settings: found_settings,
        domain: None,
    })
}

fn choice(ctx: &Context, settings: &[Setting], answers: &[Answer]) -> Choice {
    let picked = |index: usize| -> Vec<String> {
        match (settings.get(index), answers.get(index)) {
            (Some(setting), Some(Answer::Picked(picked))) => picked
                .iter()
                .filter_map(|at| setting.choices.get(*at))
                .map(|(value, _)| value.clone())
                .collect(),
            _ => Vec::new(),
        }
    };
    Choice {
        nodes: picked(NODES),
        storages: picked(STORAGES),
        template: picked(TEMPLATE).into_iter().next(),
        bridge: picked(BRIDGE).into_iter().next(),
        url: ctx.url.clone(),
    }
}

/// A rescan keeps what was picked where it is still offered.
fn keep_answers(settings: &mut [Setting], choice: &Choice) {
    let lists = [
        (NODES, choice.nodes.clone()),
        (STORAGES, choice.storages.clone()),
        (TEMPLATE, choice.template.iter().cloned().collect()),
        (BRIDGE, choice.bridge.iter().cloned().collect()),
    ];
    for (index, values) in lists {
        if let Some(setting) = settings.get_mut(index) {
            setting.picked = values
                .iter()
                .filter_map(|value| setting.choices.iter().position(|(choice, _)| choice == value))
                .collect();
        }
    }
}

/// Keep the choices for the next run; the last result stays.
fn remember(ctx: &Context, choice: &Choice) {
    let mut summary = Summary::load(&ctx.summary).unwrap_or_default();
    if summary.target != ctx.target || summary.organization != ctx.organization {
        summary = Summary::default();
    }
    summary.target.clone_from(&ctx.target);
    summary.organization.clone_from(&ctx.organization);
    summary.image.clone_from(&ctx.image);
    summary.url.clone_from(&choice.url);
    summary.nodes.clone_from(&choice.nodes);
    summary.storages.clone_from(&choice.storages);
    summary.template_storage = choice.template.clone().unwrap_or_default();
    summary.bridge = choice.bridge.clone().unwrap_or_default();
    let _ = summary.save(&ctx.summary);
}

/// The deployment's own files: `deploy.toml` and the deploy token.
fn deployment(setup: &Setup) -> Result<(DeployConfig, impl Pve + use<>), String> {
    let config = DeployConfig::load(&setup.token_dir.join("deploy.toml"))
        .map_err(|error| format!("{error}. Install HomeStack (its servers) before deploying Site connectors."))?;
    let pve = hs_deploy::deploy::deploy_client(&config)?;
    Ok((config, pve))
}

/// What deploying `choice` would do. Nothing is changed.
async fn plan(setup: &Setup, ctx: &Context, root: &mut dyn Root, access: &Access, choice: &Choice) -> Plan {
    let mut plan = Plan {
        url: choice.url.clone(),
        template: choice.template.clone().unwrap_or_default(),
        bridge: choice.bridge.clone().unwrap_or_default(),
        ..Plan::default()
    };
    if let Err(problem) = survey_plan(setup, ctx, root, access, choice, &mut plan).await {
        plan.problems.push(problem);
    }
    plan
}

async fn survey_plan(
    setup: &Setup,
    ctx: &Context,
    root: &mut dyn Root,
    access: &Access,
    choice: &Choice,
    plan: &mut Plan,
) -> Result<(), String> {
    if choice.nodes.is_empty() {
        return Err("Tick at least one node.".to_owned());
    }
    if !valid_url(&choice.url) {
        return Err("The Pangolin server configuration must provide a valid HTTPS app.dashboard_url.".to_owned());
    }
    if choice.storages.is_empty() || choice.template.is_none() || choice.bridge.is_none() {
        return Err("Choose the connector disks, the image storage and the network bridge.".to_owned());
    }
    if !ctx.endpoint.alive() {
        return Err("The Step 5 API tunnel is not connected. Reconnect it, then review again.".to_owned());
    }
    let key = api_key(ctx)?;
    organization(ctx, &key)?;
    let listed = sites(&ctx.endpoint, &key, &ctx.organization)?;
    let (config, pve) = deployment(setup)?;
    let (guests, stores) = cluster(root)?;
    let mut taken: BTreeSet<u32> = guests.iter().map(|guest| guest.vmid).collect();
    let template = plan.template.clone();
    for name in &choice.nodes {
        if !access.topology.nodes.iter().any(|n| n.name == *name && n.online) {
            plan.problems.push(format!("{name} is not an online node"));
            continue;
        }
        match node_plan(
            ctx, root, access, &pve, &config, &key, &listed, &guests, &stores, &mut taken, choice, &template, name,
        )
        .await
        {
            Ok(node) => plan.nodes.push(node),
            Err(problem) => plan.problems.push(format!("{name}: {problem}")),
        }
    }
    let mut kept: Vec<String> = guests
        .iter()
        .filter(|g| g.tags.iter().any(|t| t == TAG) && !choice.nodes.iter().any(|n| hostname(n) == g.name))
        .map(|g| format!("CT {} {} on {} (node not ticked)", g.vmid, g.name, g.node))
        .collect();
    let ours: BTreeSet<String> = choice.nodes.iter().map(|n| hostname(n)).collect();
    let others = listed.iter().filter(|site| !ours.contains(&site.nice)).count();
    if others > 0 {
        kept.push(format!("{others} other Sites of organization {}", ctx.organization));
    }
    kept.push("every other VM, container and volume; no Resource is created or exposed".to_owned());
    kept.push("existing Site credentials (never rotated by a deployment)".to_owned());
    plan.kept = kept;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn node_plan(
    ctx: &Context,
    root: &mut dyn Root,
    access: &Access,
    pve: &impl Pve,
    config: &DeployConfig,
    key: &Credential,
    listed: &[Listed],
    guests: &[Guest],
    stores: &[Store],
    taken: &mut BTreeSet<u32>,
    choice: &Choice,
    template: &str,
    name: &str,
) -> Result<NodePlan, String> {
    let nice = hostname(name);
    let storage = choice
        .storages
        .iter()
        .find(|id| {
            stores
                .iter()
                .any(|s| s.node == name && s.storage == **id && s.content.iter().any(|c| c == "rootdir"))
        })
        .cloned()
        .ok_or("none of the ticked connector disk storages is on this node")?;
    if !stores
        .iter()
        .any(|s| s.node == name && s.storage == template && s.content.iter().any(|c| c == "vztmpl"))
    {
        return Err(format!("the image storage {template} is not on this node"));
    }
    // The Site: the local journal and credential, matched to Pangolin by
    // its nice ID and Newt ID; nothing else is ever adopted.
    let path = ctx.site_key(name);
    let storage_lock = Storage::open(&path)?;
    let record: Option<SiteRecord> = storage_lock.load()?;
    if let Some(record) = &record
        && (record.target != ctx.target
            || record.organization != ctx.organization
            || record.node != name
            || record.site != nice)
    {
        return Err("its Site journal belongs to another server or organization; nothing is changed".to_owned());
    }
    let credential = if storage_lock.has_credential() || storage_lock.has_pending() {
        Some(storage_lock.recover_credential(&path)?)
    } else {
        None
    };
    drop(storage_lock);
    let remote = if listed.iter().any(|s| s.nice == nice) {
        site(&ctx.endpoint, key, &ctx.organization, &nice)?
    } else {
        None
    };
    let ours = credential.as_ref().and_then(|c| c.key_id().ok().map(str::to_owned));
    let step = site_step(record.as_ref(), remote.as_ref(), ours.as_deref(), &nice)?;
    // The container: the one the journal names, or one tagged and named
    // for this node; never a guest whose ownership is not established.
    let host = hostname(name);
    let managed = |g: &&Guest| g.kind == "lxc" && g.name == host && g.tags.iter().any(|t| t == TAG);
    let recorded = record.as_ref().and_then(|r| r.ctid);
    let candidates: Vec<&Guest> = guests.iter().filter(managed).collect();
    let existing = match recorded {
        Some(id) => match guests.iter().find(|g| g.vmid == id) {
            Some(guest) if managed(&guest) => Some(guest),
            Some(guest) => {
                return Err(format!(
                    "CT {id} recorded for {host} is now {} ({}), not a managed connector; it is left alone",
                    guest.name, guest.kind
                ));
            }
            None if candidates.is_empty() => None,
            None => {
                return Err(format!(
                    "CT {id} is gone but another {host} container exists; resolve it first"
                ));
            }
        },
        None if candidates.len() > 1 => return Err(format!("several {host} containers exist; resolve them first")),
        None => candidates.first().copied(),
    };
    let wanted = description(&ctx.organization, &nice, &ctx.image);
    let net = net0(&choice.bridge.clone().unwrap_or_default());
    let say = |_: &str| {};
    let (ctid, container, installed) = match existing {
        Some(guest) => {
            if guest.node != name {
                return Err(format!(
                    "CT {} ({host}) is on {}; move it back to {name}",
                    guest.vmid, guest.node
                ));
            }
            if node::container(pve, guest.vmid).await?.is_none() {
                return Err(format!(
                    "CT {} is not in the deploy token's pool {}; it is left alone",
                    guest.vmid, config.pool
                ));
            }
            let current = pve
                .get(&format!("/nodes/{name}/lxc/{}/config", guest.vmid), &[])
                .await
                .map_err(|e| format!("CT {} config: {e}", guest.vmid))?;
            let text = current["description"].as_str().unwrap_or_default();
            if !text
                .lines()
                .any(|l| l.trim() == format!("organization {}", ctx.organization))
                || !text.lines().any(|l| l.trim() == format!("site {nice}"))
            {
                return Err(format!(
                    "CT {} is tagged {TAG} but made for another organization; it is left alone",
                    guest.vmid
                ));
            }
            let same = node::unchanged(pve, name, guest.vmid, &wanted, &net, &say).await?;
            if !same {
                return Err(format!(
                    "CT {} was made from another image or network; Step 8 never replaces a connector",
                    guest.vmid
                ));
            }
            let step = if guest.status == "running" {
                CtStep::Keep
            } else {
                CtStep::Start
            };
            let installed = match (&step, &credential) {
                (CtStep::Keep, Some(credential)) => {
                    let expected = digest(&configuration(&choice.url, credential)?);
                    installed(root, access, name, guest.vmid, false)? == expected
                }
                _ => false,
            };
            (guest.vmid, step, installed)
        }
        None => {
            let ctid = match recorded {
                Some(id) if !taken.contains(&id) => id,
                _ => (100..1_000_000).find(|id| !taken.contains(id)).ok_or("no free CTID")?,
            };
            (ctid, CtStep::Create, false)
        }
    };
    taken.insert(ctid);
    let pull = if container == CtStep::Create {
        let volid = format!("{template}:vztmpl/{}", node::image_file(&ctx.image)?);
        let content = pve
            .get(
                &format!("/nodes/{name}/storage/{template}/content"),
                &[("content", "vztmpl")],
            )
            .await
            .map_err(|e| format!("{template}: {e}"))?;
        !content
            .as_array()
            .into_iter()
            .flatten()
            .any(|item| item["volid"].as_str() == Some(volid.as_str()))
    } else {
        false
    };
    Ok(NodePlan {
        node: name.to_owned(),
        site: nice,
        step,
        ctid,
        storage,
        container,
        pull,
        installed,
    })
}

/// The Site's step from its journal, the saved credential's Newt ID and
/// Pangolin's own Site of that nice ID: nothing else is ever adopted.
fn site_step(
    record: Option<&SiteRecord>,
    remote: Option<&(u64, String)>,
    ours: Option<&str>,
    nice: &str,
) -> Result<SiteStep, String> {
    match (record, remote) {
        (_, Some((id, newt))) if ours == Some(newt.as_str()) => match record {
            Some(record) if record.stage >= SiteStage::Created && record.site_id == Some(*id) => {
                Ok(SiteStep::Reuse(*id))
            }
            _ => Ok(SiteStep::Adopt(*id)),
        },
        (_, Some(_)) if ours.is_none() => Err(format!(
            "Site {nice} exists, but its credential is not on this desktop. HomeStack never duplicates or rotates it; rotation is a separate operation"
        )),
        (_, Some(_)) => Err(format!(
            "Site {nice} exists with another Newt ID than the saved credential; nothing is changed"
        )),
        (Some(record), None) if record.stage >= SiteStage::Created => Err(format!(
            "Site {nice} (id {}) was created by HomeStack but is gone from Pangolin; inspect it before deploying again",
            record.site_id.unwrap_or_default()
        )),
        (_, None) => Ok(SiteStep::Create),
    }
}

/// The review: what happens on each node, and what is left as it is.
fn sections(ctx: &Context, plan: &Plan) -> Vec<(String, Vec<String>)> {
    let mut sections = vec![(
        "Pangolin".to_owned(),
        vec![
            format!("organization {} through the Step 5 tunnel", ctx.organization),
            "API key: the Step 7 organization key (homestack)".to_owned(),
            format!("connectors connect to {}", plan.url),
        ],
    )];
    if plan.nodes.is_empty() {
        return sections;
    }
    let mut sites = Vec::new();
    let mut containers = Vec::new();
    let mut images = Vec::new();
    for n in &plan.nodes {
        sites.push(match n.step {
            SiteStep::Reuse(id) => format!("{}  {}  existing Site {id}, kept", n.node, n.site),
            SiteStep::Adopt(id) => format!("{}  {}  existing Site {id} of an earlier run, recorded", n.node, n.site),
            SiteStep::Create => format!("{}  {}  new Site with its own new credential", n.node, n.site),
        });
        let what = match n.container {
            CtStep::Keep if n.installed => {
                "running, credential installed: kept (restarted only if its Site is offline)".to_owned()
            }
            CtStep::Keep => "running: install its credential and restart the connector".to_owned(),
            CtStep::Start => "stopped: start it, then check its credential".to_owned(),
            CtStep::Create => format!("new container on {}, then its credential", n.storage),
        };
        containers.push(format!("{}  CT {}  {}", n.node, n.ctid, what));
        if n.pull {
            images.push(format!("pull {} into {} on {}", ctx.image, plan.template, n.node));
        }
    }
    sections.push(("Sites (one per node, independent credentials)".to_owned(), sites));
    sections.push((
        format!("Containers (unprivileged, tag {TAG}, network {} by DHCP)", plan.bridge),
        containers,
    ));
    if !images.is_empty() {
        sections.push(("Image downloads".to_owned(), images));
    }
    sections.push(("Kept, never changed".to_owned(), plan.kept.clone()));
    sections
}

/// Deploy the reviewed plan, then wait until every connector runs and its
/// Site is online. Whether it all went, and the end's lines.
async fn deploy(
    setup: &Setup,
    ctx: &Context,
    s: &mut Session,
    choice: &Choice,
    reviewed: &Plan,
    tell: &(dyn Fn(Event) + Send + Sync),
) -> (bool, Vec<String>) {
    let say = |line: &str| tell(Event::Line(line.to_owned()));
    if !reviewed.problems.is_empty() {
        return (false, reviewed.problems.clone());
    }
    say("Checking that nothing changed since the review");
    let current = plan(setup, ctx, s.root.as_mut(), &s.access, choice).await;
    if current != *reviewed {
        return (
            false,
            vec!["Pangolin or the cluster changed since the review; nothing was changed. Review again.".to_owned()],
        );
    }
    let (key, config, pve) = match api_key(ctx).and_then(|key| deployment(setup).map(|(c, p)| (key, c, p))) {
        Ok(found) => found,
        Err(error) => return (false, vec![error]),
    };
    let mut results = Vec::new();
    let mut lines = Vec::new();
    for n in &reviewed.nodes {
        say(&format!("{}: Site {}, CT {}", n.node, n.site, n.ctid));
        match deploy_node(ctx, s.root.as_mut(), &s.access, &pve, &config, &key, reviewed, n, &say).await {
            Ok(site_id) => results.push((n, site_id)),
            Err(error) => {
                say(&format!("  {}: {error}", n.node));
                lines.push(format!("{}: {error}", n.node));
            }
        }
    }
    let mut ok = lines.is_empty();
    let deployed = health(ctx, &key, &pve, &results, &say).await;
    for d in &deployed {
        lines.push(format!(
            "{}  Site {} ({})  CT {} {}  Pangolin {}",
            d.node, d.site, d.site_id, d.ctid, d.container, d.connection
        ));
        ok &= d.container == "running" && d.connection == "online";
    }
    let mut summary = Summary::load(&ctx.summary).unwrap_or_default();
    summary.target.clone_from(&ctx.target);
    summary.organization.clone_from(&ctx.organization);
    summary.at = stamp();
    summary.deployed = deployed;
    if let Err(error) = summary.save(&ctx.summary) {
        lines.push(error);
    }
    if !ok {
        lines.push("Deploy again when the cause is fixed: Sites, credentials and containers are reused.".to_owned());
    }
    (ok, lines)
}

/// One node: its Site, its container, its credential. The Site's number.
#[allow(clippy::too_many_arguments)]
async fn deploy_node(
    ctx: &Context,
    root: &mut dyn Root,
    access: &Access,
    pve: &impl Pve,
    config: &DeployConfig,
    key: &Credential,
    plan: &Plan,
    n: &NodePlan,
    say: &impl Fn(&str),
) -> Result<u64, String> {
    let path = ctx.site_key(&n.node);
    // Held for the whole node: no other HomeStack run touches this Site.
    let storage = Storage::open(&path)?;
    let (mut record, credential) = journal(
        &storage,
        &path,
        |newt_id| SiteRecord {
            target: ctx.target.clone(),
            organization: ctx.organization.clone(),
            node: n.node.clone(),
            site: n.site.clone(),
            newt_id,
            site_id: None,
            ctid: None,
            stage: SiteStage::Prepared,
            at: stamp(),
            home: None,
        },
        say,
    )?;
    let site_id = match n.step {
        SiteStep::Reuse(id) => id,
        SiteStep::Adopt(id) => id,
        SiteStep::Create => create_site(ctx, key, &storage, &mut record, &credential, say)?,
    };
    record.site_id = Some(site_id);
    record.stage = SiteStage::Created;
    record.ctid = Some(n.ctid);
    record.at = stamp();
    storage.save(&record)?;

    let name = n.node.as_str();
    let host = hostname(name);
    let net = net0(&plan.bridge);
    if n.container == CtStep::Create {
        let ostemplate = node::ensure_image(pve, name, &plan.template, &ctx.image, say).await?;
        let spec = NewContainer {
            node: name,
            ctid: n.ctid,
            hostname: &host,
            ostemplate: &ostemplate,
            pool: &config.pool,
            storage: &n.storage,
            disk_gib: config.disk_gib,
            memory_mib: config.memory_mib,
            cores: config.cores,
            net0: Some(net),
            onboot: true,
            nameserver: None,
            description: description(&ctx.organization, &n.site, &ctx.image),
            tag: Some(TAG),
            mp0: None,
        };
        node::create(pve, &spec, say).await?;
    }
    if n.container != CtStep::Keep {
        say(&format!("  starting CT {}", n.ctid));
        node::power(pve, name, n.ctid, "start").await?;
    }
    let content = configuration(&plan.url, &credential)?;
    if install(root, access, name, n.ctid, &content, false, say)? {
        say("  credential installed and verified by its digest");
    } else if n.container == CtStep::Keep && !online(&ctx.endpoint, key, &ctx.organization, site_id)? {
        say("  credential installed, but the Site is offline: restarting the connector");
        node::power(pve, name, n.ctid, "reboot").await?;
    } else {
        say("  credential already installed");
    }
    Ok(site_id)
}

/// The journal at `path` and its credential; a new one is made with `fresh`
/// (given the Newt ID) when there is none. The credential is generated and
/// saved before Pangolin hears of it; one saved without its journal is kept,
/// never replaced.
fn journal(
    storage: &Storage,
    path: &Path,
    fresh: impl FnOnce(String) -> SiteRecord,
    say: &dyn Fn(&str),
) -> Result<(SiteRecord, Credential), String> {
    let record: SiteRecord = match storage.load()? {
        Some(record) => record,
        None => {
            let credential = if storage.has_credential() || storage.has_pending() {
                storage.recover_credential(path)?
            } else {
                let mut credential = Credential::default();
                credential.insert(&random(15)?);
                credential.insert(".");
                credential.insert(&random(48)?);
                storage.release()?;
                storage.reserve()?;
                storage.persist(&credential)?;
                say("  new Site credential saved on this desktop");
                credential
            };
            let record = fresh(credential.key_id().map_err(|_| "Invalid Site credential.")?.to_owned());
            storage.save(&record)?;
            record
        }
    };
    let credential = storage.recover_credential(path)?;
    if credential.key_id().ok() != Some(record.newt_id.as_str()) {
        return Err("the saved Site credential does not match its journal; nothing is changed".to_owned());
    }
    Ok((record, credential))
}

/// Install `content` into `ctid` over the root session's standard input
/// unless it is there already; verified by its digest. Whether it was written.
fn install(
    root: &mut dyn Root,
    access: &Access,
    node: &str,
    ctid: u32,
    content: &str,
    managed: bool,
    say: &dyn Fn(&str),
) -> Result<bool, String> {
    let expected = digest(content);
    // The container's runtime may need a moment before it can be entered.
    let mut current = Err(String::new());
    for _ in 0..15 {
        current = installed(root, access, node, ctid, managed);
        if current.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    if current? == expected {
        return Ok(false);
    }
    say("  installing the Site credential over the root session's standard input");
    let command = on_node(access, node, &tool(ctid, managed))?;
    let output = root.run_input(&command, content).map_err(|e| e.to_string())?;
    if output.code != 0 {
        return Err(format!("CT {ctid}: the credential could not be installed"));
    }
    if installed(root, access, node, ctid, managed)? != expected {
        return Err(format!("CT {ctid}: the installed credential does not match"));
    }
    Ok(true)
}

/// `PUT /v1/org/{orgId}/site` with the saved Newt ID and secret; the journal
/// marks the request first, so an unknown outcome is resolved by looking.
fn create_site(
    ctx: &Context,
    key: &Credential,
    storage: &Storage,
    record: &mut SiteRecord,
    credential: &Credential,
    say: &dyn Fn(&str),
) -> Result<u64, String> {
    let newt = credential.key_id().map_err(|_| "Invalid Site credential.")?.to_owned();
    if let Some((id, existing)) = site(&ctx.endpoint, key, &ctx.organization, &record.site)? {
        return if existing == newt {
            Ok(id)
        } else {
            Err(format!(
                "Site {} appeared with another Newt ID; nothing is changed",
                record.site
            ))
        };
    }
    record.stage = SiteStage::CreationRequested;
    record.at = stamp();
    storage.save(record)?;
    say(&format!("  creating Site {}", record.site));
    let (_, secret) = credential.value.split_once('.').ok_or("Invalid Site credential.")?;
    let body = Zeroizing::new(
        serde_json::to_string(&serde_json::json!({
            "name": record.site,
            "niceId": record.site,
            "type": "newt",
            "newtId": newt,
            "secret": secret,
        }))
        .map_err(|_| "Invalid Site credential.")?,
    );
    let stop = AtomicBool::new(false);
    let sent = client(&ctx.endpoint, key, &stop).request(
        "PUT",
        &format!("/v1/org/{}/site", ctx.organization),
        Some(&body),
        Probe::Organization,
    );
    drop(body);
    if let Ok((status, response)) = &sent
        && matches!(status, 200 | 201)
        && let Ok(value) = serde_json::from_slice::<serde_json::Value>(response)
        && value["data"]["niceId"].as_str() == Some(record.site.as_str())
        && value["data"]["newtId"].as_str() == Some(newt.as_str())
        && let Some(id) = value["data"]["siteId"].as_u64()
    {
        say(&format!("  Site {} created (id {id})", record.site));
        return Ok(id);
    }
    // Rejected or unknown: Pangolin's own state decides, by the nice ID.
    match site(&ctx.endpoint, key, &ctx.organization, &record.site) {
        Ok(Some((id, existing))) if existing == newt => {
            say(&format!("  Site {} exists (id {id})", record.site));
            Ok(id)
        }
        _ => Err(match sent {
            Ok((403, _)) => "the HomeStack API key may not create Sites (createSite); refresh Step 7".to_owned(),
            Ok((status, _)) => {
                format!("Pangolin refused the Site (HTTP {status}); the saved credential is kept for the next run")
            }
            Err(problem) => format!(
                "Site creation outcome unknown ({}); deploy again: the same credential is reused, never duplicated",
                explain(problem)
            ),
        }),
    }
}

/// Poll until every deployed connector's container runs and its Site is
/// online, or the limit passes. The status of each, as it ended.
async fn health(
    ctx: &Context,
    key: &Credential,
    pve: &impl Pve,
    results: &[(&NodePlan, u64)],
    say: &impl Fn(&str),
) -> Vec<Deployed> {
    let mut deployed: Vec<Deployed> = results
        .iter()
        .map(|(n, site_id)| Deployed {
            node: n.node.clone(),
            site: n.site.clone(),
            site_id: *site_id,
            ctid: n.ctid,
            container: "unknown".to_owned(),
            connection: "unknown".to_owned(),
        })
        .collect();
    if deployed.is_empty() {
        return deployed;
    }
    say(&format!(
        "Waiting for the connectors (Proxmox status and Pangolin online, up to {} s)",
        HEALTH_LIMIT.as_secs()
    ));
    let started = Instant::now();
    loop {
        let listed = sites(&ctx.endpoint, key, &ctx.organization);
        for d in &mut deployed {
            d.container = match node::container(pve, d.ctid).await {
                Ok(Some(found)) => found.status,
                Ok(None) => "missing".to_owned(),
                Err(_) => "unknown".to_owned(),
            };
            d.connection = match &listed {
                Ok(rows) => match rows.iter().find(|row| row.site_id == d.site_id) {
                    Some(row) if row.online == Some(true) => "online".to_owned(),
                    Some(_) => "offline".to_owned(),
                    None => "missing".to_owned(),
                },
                Err(_) => "unknown".to_owned(),
            };
        }
        let done = deployed
            .iter()
            .all(|d| d.container == "running" && d.connection == "online");
        if done || started.elapsed() >= HEALTH_LIMIT {
            for d in &deployed {
                say(&format!(
                    "  {}: container {}, Pangolin {}",
                    d.node, d.container, d.connection
                ));
            }
            return deployed;
        }
        tokio::time::sleep(HEALTH_RETRY).await;
    }
}

impl super::pangolin::Wizard {
    fn sites_path(&self) -> PathBuf {
        self.path.with_file_name("pangolin-sites.toml")
    }

    /// Step 8 needs the connected tunnel, the organization and the saved
    /// Step 7 key (verified again by the deployment itself).
    #[must_use]
    pub fn sites_available(&self) -> bool {
        self.tunnel_connected()
            && super::pangolin_key::valid_identifier(&self.bootstrap.organization)
            && self.permanent_destination().is_file()
    }

    /// The last deployment for this server and organization ended healthy.
    #[must_use]
    pub fn sites_healthy(&self) -> bool {
        self.sites
            .as_ref()
            .is_some_and(|s| s.healthy(&self.target, &self.bootstrap.organization))
    }

    /// The saved result belongs to this server and organization.
    pub fn sites_summary(&self) -> Option<&Summary> {
        self.sites.as_ref().filter(|summary| {
            summary.target == self.target
                && summary.organization == self.bootstrap.organization
                && !summary.deployed.is_empty()
        })
    }

    /// Read the summary again after the deployment screen closes.
    pub fn reload_sites(&mut self) {
        self.sites = Summary::load(&self.sites_path());
    }

    /// The worker's context, bound to the live tunnel `endpoint`.
    pub(super) fn sites_context(&self, endpoint: Endpoint) -> Option<Context> {
        self.sites_available().then(|| Context {
            image: self
                .sites
                .as_ref()
                .map(|s| s.image.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(IMAGE)
                .to_owned(),
            endpoint,
            target: self.target.clone(),
            organization: self.bootstrap.organization.clone(),
            url: self
                .discovery
                .as_ref()
                .filter(|d| d.target == self.target)
                .and_then(|d| d.known("dashboard_url"))
                .unwrap_or_default()
                .to_owned(),
            key: self.permanent_destination(),
            identity: self.permanent_identity(),
            summary: self.sites_path(),
        })
    }

    /// The same server, organization, Step 7 key and dashboard URL from the
    /// saved settings, for managed initialization outside this wizard; bound
    /// to a tunnel the caller started and verified.
    pub(super) fn managed_context(&self, endpoint: Endpoint) -> Result<Context, String> {
        if !super::pangolin_key::valid_identifier(&self.bootstrap.organization) {
            return Err("Pangolin Step 6 has no verified organization. Complete it first.".to_owned());
        }
        if !self.permanent_destination().is_file() {
            return Err("Pangolin Step 7 has not saved the HomeStack organization key. Complete it first.".to_owned());
        }
        let url = self
            .discovery
            .as_ref()
            .filter(|d| d.target == self.target)
            .and_then(|d| d.known("dashboard_url"))
            .unwrap_or_default();
        if !valid_url(url) {
            return Err("Pangolin Step 3 discovery has no valid HTTPS app.dashboard_url. Discover again.".to_owned());
        }
        Ok(Context {
            image: String::new(),
            endpoint,
            target: self.target.clone(),
            organization: self.bootstrap.organization.clone(),
            url: url.to_owned(),
            key: self.permanent_destination(),
            identity: self.permanent_identity(),
            summary: self.sites_path(),
        })
    }

    /// F7: the last deployment, as it ended.
    #[must_use]
    pub fn sites_report(&self, offset: i64) -> String {
        let mut lines = vec![
            format!("Organization: {}", self.bootstrap.organization),
            format!("Pangolin server: {}", self.target),
            format!(
                "Image: {}",
                self.sites
                    .as_ref()
                    .map(|s| s.image.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(IMAGE)
            ),
        ];
        match self.sites.as_ref().filter(|s| s.target == self.target) {
            Some(summary) if !summary.deployed.is_empty() => {
                let date = super::pangolin::Report {
                    target: String::new(),
                    at: summary.at.clone(),
                    passed: true,
                    detail: String::new(),
                    duration_ms: 0,
                }
                .local_date(offset);
                lines.push(format!("Pangolin URL: {}", summary.url));
                lines.push(format!("Last deployment: {date} (historical, not monitoring)"));
                lines.push(String::new());
                for d in &summary.deployed {
                    lines.push(format!(
                        "{}: Site {} (id {}), CT {} {}, Pangolin {}",
                        d.node, d.site, d.site_id, d.ctid, d.container, d.connection
                    ));
                }
            }
            _ => lines.push("No deployment yet.".to_owned()),
        }
        lines.push(String::new());
        lines.push("Credentials: one per Site, saved under the HomeStack secrets directory (mode 0600) and inside its own container only. They are never displayed, logged or rotated by a deployment.".to_owned());
        lines.join("\n")
    }
}
