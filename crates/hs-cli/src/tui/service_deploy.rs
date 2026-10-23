//! The worker of the install screen in Service mode: one Deployment of a
//! manifest-defined service through the deploy token. The settings start
//! from what the cluster has (its instances) or what the manifest declares;
//! the review shows only what changes: new instances on the ticked nodes
//! (`crate::services::deploy`), the selected instances' image
//! (`crate::services::update`), sizes and environment
//! (`crate::services::lifecycle`), or with Del their removal.

use std::sync::mpsc::Receiver;

use hs_deploy::config::DeployConfig;
use hs_deploy::node::show_size;
use hs_pve::PveClient;

use super::install::{Check, Event, Found, Mark, Setting};
use super::installing::{Command, Setup};
use super::pangolin_sites::managed::{Initializer, Pangolin};
use super::reinstall::local_time;
use super::service_pangolin;
use crate::install_flow::Answer;
use crate::services::deploy::{self, Cluster, SiteInit, SiteReady, Source};
use crate::services::dns::Dns;
use crate::services::instance::{self, Instance};
use crate::services::manifest::{Manifest, Placement, show_mib};
use crate::services::{lifecycle, update};

// The settings by title: Instances only once there are some, Home only
// for a service with persistence.
const INSTANCES: &str = "Instances";
const VERSION: &str = "Version";
const NODES: &str = "Nodes";
const CTIDS: &str = "New CTIDs";
const CORES: &str = "Cores";
const MEMORY: &str = "Memory (MiB)";
const ROOT: &str = "Root";
const HOME: &str = "Home";
const ENVIRONMENT: &str = "Environment";
const HOSTNAME: &str = "Hostname";
const DOMAIN: &str = "Domain";
const DNS_SERVER: &str = "DNS server";

struct Session {
    config: DeployConfig,
    pve: PveClient,
    cluster: Cluster,
    instances: Vec<Instance>,
    settings: Vec<Setting>,
    /// The environment the settings started from.
    shown: String,
    reviewed: Option<Reviewed>,
    /// A Pangolin Site connector: the verified Pangolin context and root.
    site: Option<Initializer>,
    /// Another service published through Pangolin: its verified context.
    pangolin: Option<Pangolin>,
    catalog: Option<service_pangolin::Catalog>,
    /// The local DNS the instances' names go into, or why it is unusable.
    dns: Result<Option<Dns>, String>,
    /// The name, domain and DNS server the settings started from.
    naming: Naming,
}

/// How the instances are named and resolve names: read from an instance,
/// else the manifest's host name and the local DNS or the deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Naming {
    hostname: String,
    domain: String,
    nameserver: String,
}

impl Session {
    /// The verified Pangolin context, when one is open.
    fn ready(&self) -> Option<&dyn SiteReady> {
        match (&self.site, &self.pangolin) {
            (Some(site), _) => Some(site.pangolin() as &dyn SiteReady),
            (None, Some(pangolin)) => Some(pangolin as &dyn SiteReady),
            (None, None) => None,
        }
    }

    fn context(&self) -> Option<&Pangolin> {
        self.site.as_ref().map(Initializer::pangolin).or(self.pangolin.as_ref())
    }
}

enum Reviewed {
    Remove(lifecycle::Plan),
    Change(Box<Changes>),
}

/// The reviewed parts of a deployment, applied in this order.
struct Changes {
    update: Option<update::Plan>,
    change: Option<lifecycle::Plan>,
    create: Option<deploy::Plan>,
    attach: Option<service_pangolin::Plan>,
}

/// The review's sections, its problems and what applying it does.
type Review = (Vec<(String, Vec<String>)>, Vec<String>, Reviewed);

/// What the operator left in the settings.
struct Chosen {
    selected: Vec<u32>,
    deploy: deploy::Choice,
}

pub(super) fn work(setup: &Setup, commands: &Receiver<Command>, tell: &(dyn Fn(Event) + Send + Sync)) {
    let Some(source) = setup.service.clone() else {
        tell(Event::Failed("No service was chosen.".to_owned()));
        return;
    };
    let mut session: Option<Session> = None;
    while let Ok(command) = commands.recv() {
        match command {
            Command::Connect { .. } | Command::Rescan { .. } => {
                let kept = match &command {
                    Command::Rescan { answers, .. } => session.as_ref().map(|s| (s.settings.clone(), answers.clone())),
                    _ => None,
                };
                let previous = session.take();
                match connect(setup, &source, previous, kept, tell) {
                    Ok((opened, found)) => {
                        session = Some(opened);
                        tell(Event::Connected(found));
                    }
                    Err(error) => tell(Event::Failed(error)),
                }
            }
            Command::Review { answers, .. } => {
                let Some(s) = session.as_mut() else { continue };
                if service_pangolin::requested(&s.settings, &answers) && s.catalog.is_none() {
                    // The resources and Sites are read once attach is chosen.
                    tell(Event::Busy("Loading Pangolin resources and Sites".to_owned()));
                    let loaded = s
                        .context()
                        .ok_or_else(|| "The Pangolin context is unavailable".to_owned())
                        .and_then(|p| service_pangolin::load(p, &setup.stop));
                    match loaded {
                        Ok(catalog) => {
                            s.catalog = Some(catalog);
                            let previous = s.settings.clone();
                            let end = service_pangolin::start(&s.settings).unwrap_or(s.settings.len());
                            s.settings.truncate(end);
                            s.settings
                                .extend(service_pangolin::settings(&source, s.catalog.as_ref()));
                            restore(&mut s.settings, &previous, &answers);
                            tell(Event::Connected(found(&s.config, &s.cluster, &s.settings)));
                        }
                        Err(error) => tell(Event::Reviewed {
                            sections: Vec::new(),
                            problems: vec![error],
                        }),
                    }
                    continue;
                }
                tell(Event::Busy("Checking the plan against the cluster".to_owned()));
                let (sections, problems, reviewed) = review(setup, &source, s, &answers);
                tell(Event::Reviewed { sections, problems });
                s.reviewed = Some(reviewed);
            }
            Command::Remove { answers } => {
                let Some(s) = session.as_mut() else { continue };
                tell(Event::Busy("Reading what would be removed".to_owned()));
                let chosen = chosen(&s.settings, &answers, s.instances.len());
                let mut plan = lifecycle::Plan::default();
                if chosen.selected.is_empty() {
                    plan.problems
                        .push("Tick the instances to remove under Instances first".to_owned());
                } else {
                    let targets: Vec<Instance> = s
                        .instances
                        .iter()
                        .filter(|i| chosen.selected.contains(&i.ctid))
                        .cloned()
                        .collect();
                    let choice = lifecycle::Choice {
                        ctids: chosen.selected.clone(),
                        remove: lifecycle::names(&targets),
                        ..lifecycle::Choice::default()
                    };
                    plan = setup.handle.block_on(lifecycle::plan(
                        &s.pve,
                        &source.repository,
                        &source.manifest.id,
                        &choice,
                    ));
                    if source.is_site() {
                        match s.ready() {
                            Some(site) => {
                                for target in &plan.targets {
                                    match site.removal(target.ctid) {
                                        Ok(line) => plan.lines.push(line),
                                        Err(error) => plan.problems.push(error),
                                    }
                                }
                            }
                            None => plan
                                .problems
                                .push("Removing a Site requires the verified Pangolin context".to_owned()),
                        }
                    }
                }
                tell(Event::Reviewed {
                    sections: vec![("Remove".to_owned(), plan.lines.clone())],
                    problems: plan.problems.clone(),
                });
                s.reviewed = Some(Reviewed::Remove(plan));
            }
            Command::Apply => {
                let Some(s) = session.as_mut() else { continue };
                let Some(reviewed) = s.reviewed.take() else { continue };
                let (ok, lines) = apply(setup, &source, s, reviewed, tell);
                tell(Event::Done { ok, lines });
            }
            Command::Cancel => return,
        }
    }
}

/// The deploy token's client, the cluster, the service's instances, the
/// Pangolin context it needs, and the settings (those of `kept`, a rescan).
fn connect(
    setup: &Setup,
    source: &Source,
    previous: Option<Session>,
    kept: Option<(Vec<Setting>, Vec<Answer>)>,
    tell: &(dyn Fn(Event) + Send + Sync),
) -> Result<(Session, Found), String> {
    tell(Event::Busy(
        "Reading the cluster, the instances and the releases".to_owned(),
    ));
    let config = DeployConfig::load(&setup.token_dir.join("deploy.toml"))
        .map_err(|error| format!("{error}. Install HomeStack before deploying services."))?;
    let pve = hs_deploy::deploy::deploy_client(&config)?;
    let cluster = setup.handle.block_on(deploy::survey(&config, &pve, source))?;
    if cluster.releases.is_empty() {
        return Err(format!(
            "{} has no published release (a numeric X.Y.Z tag); dev images are not offered.",
            source.manifest.image_repository
        ));
    }
    let instances = setup
        .handle
        .block_on(instance::of_service(&pve, &source.repository, &source.manifest.id))?;
    let (site, pangolin, catalog) = match previous {
        Some(p) => (
            p.site.filter(Initializer::alive),
            p.pangolin.filter(Pangolin::alive),
            p.catalog,
        ),
        None => (None, None, None),
    };
    // A Site or a service published through Pangolin needs its verified
    // context from the start: what Pangolin has is part of the state.
    let (site, pangolin) = if source.is_site() {
        let site = match site {
            Some(site) => site,
            None => Initializer::open(open_pangolin(setup, tell)?, config.clone())?,
        };
        (Some(site), None)
    } else if source.manifest.pangolin.is_some() {
        let pangolin = match pangolin {
            Some(pangolin) => pangolin,
            None => open_pangolin(setup, tell)?,
        };
        (None, Some(pangolin))
    } else {
        (None, None)
    };
    let shown = instances.first().map(lifecycle::shown_environment).unwrap_or_default();
    let dns = Dns::load(&setup.token_dir);
    let local = dns.as_ref().ok().and_then(Option::as_ref);
    let first = instances.first();
    let naming = Naming {
        hostname: first.map_or_else(
            || source.manifest.deployment.hostname.clone(),
            |i| base_name(&i.hostname),
        ),
        domain: first
            .and_then(|i| i.searchdomain.clone())
            .or_else(|| local.map(|dns| dns.zone.clone()))
            .unwrap_or_else(|| config.domain.clone()),
        nameserver: first
            .and_then(|i| i.nameserver.clone())
            .or_else(|| local.map(|dns| dns.server.clone()))
            .or_else(|| config.nameserver.map(|ip| ip.to_string()))
            .unwrap_or_default(),
    };
    // The CTIDs an empty New CTIDs gives, one per node at most.
    let automatic = setup
        .handle
        .block_on(deploy::automatic(&config, &pve, cluster.nodes.len()))
        .unwrap_or_default();
    let mut settings = settings(source, &cluster, &instances, &automatic, &naming, &shown, setup.offset);
    settings.extend(service_pangolin::settings(source, catalog.as_ref()));
    if let Some((previous, answers)) = kept {
        restore(&mut settings, &previous, &answers);
    }
    let found = found(&config, &cluster, &settings);
    Ok((
        Session {
            config,
            pve,
            cluster,
            instances,
            settings,
            shown,
            reviewed: None,
            site,
            pangolin,
            catalog,
            dns,
            naming,
        },
        found,
    ))
}

fn found(config: &DeployConfig, cluster: &Cluster, settings: &[Setting]) -> Found {
    Found {
        root: format!("PVE {}", cluster.version),
        check: vec![
            (
                Mark::Ok,
                "access".to_owned(),
                format!("deploy token, pool {}", config.pool),
            ),
            (Mark::Ok, "nodes".to_owned(), cluster.nodes.join(", ")),
        ],
        settings: settings.to_vec(),
        domain: None,
    }
}

/// The verified Pangolin context through its SSH tunnel.
fn open_pangolin(setup: &Setup, tell: &(dyn Fn(Event) + Send + Sync)) -> Result<Pangolin, String> {
    tell(Event::Busy(
        "Opening the Pangolin API tunnel (touch your key or give its PIN if it asks)".to_owned(),
    ));
    Pangolin::open(
        &setup.token_dir,
        std::sync::Arc::clone(&setup.system),
        setup.ssh_prefix.clone(),
        setup.sounds.clone(),
        &setup.stop,
    )
}

fn gib(bytes: u64) -> u64 {
    bytes / (1024 * 1024 * 1024)
}

/// The release tag of `image`.
fn tag(image: &str) -> &str {
    image.rsplit_once(':').map_or("", |(_, tag)| tag)
}

fn setting(title: &str, note: &str, choices: Vec<(String, String)>, multiple: bool, text: Option<String>) -> Setting {
    Setting {
        list: false,
        check: None,
        hint: None,
        title: title.to_owned(),
        note: note.to_owned(),
        multiple,
        choices,
        picked: Vec::new(),
        text,
    }
}

/// The settings: the instances (none ticked), then the values of the first
/// instance, or the manifest's for a service not deployed yet.
fn settings(
    source: &Source,
    cluster: &Cluster,
    instances: &[Instance],
    automatic: &[u32],
    naming: &Naming,
    shown: &str,
    offset: i64,
) -> Vec<Setting> {
    let manifest = &source.manifest;
    let first = instances.first();
    let releases: Vec<(String, String)> = cluster
        .releases
        .iter()
        .map(|release| {
            let made = release
                .created
                .as_deref()
                .and_then(|at| local_time(at, offset))
                .unwrap_or_else(|| "—".to_owned());
            (tag(&release.image).to_owned(), made)
        })
        .collect();
    let version = first
        .and_then(|i| releases.iter().position(|(tag_, _)| tag_ == tag(&i.image)))
        .unwrap_or(0);
    let mut rows: Vec<&deploy::NodeStorage> = cluster.storages.iter().collect();
    rows.sort_by(|a, b| (&a.node, &a.storage).cmp(&(&b.node, &b.storage)));
    let placements: Vec<(String, String)> = rows
        .iter()
        .map(|s| {
            (
                format!("{}:{}", s.node, s.storage),
                format!("{}/{} GiB free", gib(s.available), gib(s.total)),
            )
        })
        .collect();
    let resources = manifest.resources;
    let home = match (first, &manifest.persistence) {
        (Some(i), _) if i.home_mib > 0 => Some(show_size(i.home_mib)),
        (_, Some(persistence)) => Some(show_mib(persistence.size_mib)),
        _ => None,
    };
    let mut version_setting = setting(
        VERSION,
        "A published release of the image, newest first; dev images are never offered. Ticked instances \
         with another version are made again from it, their home kept.",
        releases,
        false,
        None,
    );
    version_setting.picked = vec![version];
    let mut nodes = setting(
        NODES,
        "The nodes that get a new instance, one storage each; its container and home volume are made there. \
         A new service starts with the manifest's instances and placement ticked.",
        placements,
        true,
        None,
    );
    if instances.is_empty() {
        nodes.picked = default_nodes(manifest, &rows);
    }
    let mut settings = Vec::new();
    if !instances.is_empty() {
        settings.push(setting(
            INSTANCES,
            "The service's containers in the cluster. Tick the ones the changes below apply to; Del removes the \
             ticked ones with their homes.",
            instances
                .iter()
                .map(|i| {
                    (
                        i.ctid.to_string(),
                        format!(
                            "{} {} {} {}",
                            i.hostname,
                            i.address,
                            if i.running { "running" } else { "stopped" },
                            i.node
                        ),
                    )
                })
                .collect(),
            true,
            None,
        ));
    }
    settings.extend([
        version_setting,
        nodes,
        checked(
            setting(
                CTIDS,
                "Empty: the first free CTIDs. Or one CTID per ticked node, in the nodes' order, separated by commas.",
                Vec::new(),
                false,
                Some(String::new()),
            ),
            check_ctids,
        ),
        checked(
            setting(
                HOSTNAME,
                "The instances' name: alone for one instance, numbered (-1, -2…) for several. It is the \
                 container's host name and, with the domain, its name in the local DNS.",
                Vec::new(),
                false,
                Some(naming.hostname.clone()),
            ),
            check_hostname,
        ),
        checked(
            setting(
                DOMAIN,
                "The DNS domain of the names, also the containers' search domain.",
                Vec::new(),
                false,
                Some(naming.domain.clone()),
            ),
            check_domain,
        ),
        checked(
            setting(
                DNS_SERVER,
                "The DNS server the containers use.",
                Vec::new(),
                false,
                Some(naming.nameserver.clone()),
            ),
            check_nameserver,
        ),
        checked(
            setting(
                CORES,
                "CPU cores of each container.",
                Vec::new(),
                false,
                Some(first.map_or(resources.cores, |i| i.cores).to_string()),
            ),
            check_cores,
        ),
        checked(
            setting(
                MEMORY,
                "Memory of each container, without swap.",
                Vec::new(),
                false,
                Some(first.map_or(resources.memory_mib, |i| i.memory_mib).to_string()),
            ),
            check_memory,
        ),
        checked(
            setting(
                ROOT,
                "The root disk, a number with M or G; it holds the image and no data. It only grows.",
                Vec::new(),
                false,
                Some(first.map_or_else(|| show_mib(resources.rootfs_mib), |i| show_size(i.root_mib))),
            ),
            check_root,
        ),
    ]);
    if let Some(home) = home {
        settings.push(checked(
            setting(
                HOME,
                "The home volume at /home/app, a number with M or G; it keeps the service's data and only grows.",
                Vec::new(),
                false,
                Some(home),
            ),
            check_home,
        ));
    }
    settings.push(Setting {
        list: true,
        check: Some(check_entry),
        ..setting(
            ENVIRONMENT,
            "The containers' variables, one per entry (KEY=value). No secrets: the configuration is readable \
             in Proxmox. A running container restarts to read a change.",
            shown
                .split_whitespace()
                .map(|entry| (entry.to_owned(), String::new()))
                .collect(),
            false,
            None,
        )
    });
    if let Some(ctids) = settings.iter_mut().find(|setting| setting.title == CTIDS) {
        ctids.hint = Some(if automatic.is_empty() {
            "auto: the first free".to_owned()
        } else {
            let shown: Vec<String> = automatic.iter().map(u32::to_string).collect();
            format!("auto: {}", shown.join(", "))
        });
    }
    settings
}

/// `setting` with the check of its typed value.
fn checked(mut setting: Setting, check: Check) -> Setting {
    setting.check = Some(check);
    setting
}

/// A size written `<number>M` or `<number>G` from `min` to `max` MiB.
fn size_in(text: &str, min: u32, max: u32) -> Result<(), String> {
    let text = text.trim();
    let mib = [("M", 1_u64), ("G", 1024)].iter().find_map(|(unit, factor)| {
        let number = text.strip_suffix(unit)?;
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        number.parse::<u64>().ok()?.checked_mul(*factor)
    });
    match mib {
        Some(mib) if (u64::from(min)..=u64::from(max)).contains(&mib) => Ok(()),
        _ => Err(format!(
            "A size from {} to {}, e.g. 512M or 2G",
            show_mib(min),
            show_mib(max)
        )),
    }
}

fn number_in(text: &str, min: u32, max: u32) -> Result<(), String> {
    match text.trim().parse::<u32>() {
        Ok(number) if (min..=max).contains(&number) => Ok(()),
        _ => Err(format!("A whole number from {min} to {max}")),
    }
}

fn check_cores(text: &str) -> Result<(), String> {
    number_in(text, 1, 64)
}

fn check_memory(text: &str) -> Result<(), String> {
    number_in(text, 64, 1_048_576)
}

fn check_root(text: &str) -> Result<(), String> {
    size_in(text, 64, 1024 * 1024)
}

fn check_home(text: &str) -> Result<(), String> {
    size_in(text, 1, 16_384 * 1024)
}

/// Empty, or CTIDs separated by commas.
fn check_ctids(text: &str) -> Result<(), String> {
    let wrong = text
        .split(',')
        .map(str::trim)
        .filter(|ctid| !ctid.is_empty())
        .any(|ctid| ctid.parse::<u32>().map_or(true, |ctid| ctid < 1000));
    if wrong {
        Err("Empty for automatic CTIDs, or CTIDs from 1000 separated by commas; the CTID gives the address: 102010 is .102.10".to_owned())
    } else {
        Ok(())
    }
}

/// A DNS label: a-z, 0-9 and inner -.
fn check_hostname(text: &str) -> Result<(), String> {
    if is_label(text.trim()) {
        Ok(())
    } else {
        Err("A DNS label: a-z, 0-9 and inner -, up to 63 characters".to_owned())
    }
}

fn is_label(text: &str) -> bool {
    (1..=63).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !text.starts_with('-')
        && !text.ends_with('-')
}

fn check_domain(text: &str) -> Result<(), String> {
    let text = text.trim();
    if text.split('.').count() >= 2 && text.split('.').all(is_label) {
        Ok(())
    } else {
        Err("A DNS domain such as example.com".to_owned())
    }
}

fn check_nameserver(text: &str) -> Result<(), String> {
    text.trim()
        .parse::<std::net::Ipv4Addr>()
        .map(|_| ())
        .map_err(|_| "An IPv4 address such as 192.0.2.53".to_owned())
}

/// `name` without a trailing instance number (`-1`, `-2`…).
fn base_name(name: &str) -> String {
    match name.rsplit_once('-') {
        Some((base, number))
            if !base.is_empty() && !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) =>
        {
            base.to_owned()
        }
        _ => name.to_owned(),
    }
}

/// One `KEY=value` entry.
fn check_entry(text: &str) -> Result<(), String> {
    match deploy::environment(text) {
        Ok(entries) if entries.len() == 1 => Ok(()),
        Ok(_) => Err("One KEY=value entry".to_owned()),
        Err(error) => Err(error),
    }
}

/// The manifest's instances as ticked rows: one node each with `spread`,
/// all on the first node with `single`; each node's storage with the most
/// free space.
fn default_nodes(manifest: &Manifest, rows: &[&deploy::NodeStorage]) -> Vec<usize> {
    let mut best: Vec<usize> = Vec::new();
    for (at, row) in rows.iter().enumerate() {
        match best.iter_mut().find(|known| rows[**known].node == row.node) {
            Some(known) if rows[*known].available < row.available => *known = at,
            Some(_) => {}
            None => best.push(at),
        }
    }
    let count = usize::try_from(manifest.deployment.instances).unwrap_or(1);
    match manifest.deployment.placement {
        Placement::Spread => best.into_iter().take(count).collect(),
        Placement::Single => best.into_iter().take(1).collect(),
    }
}

/// The answers as what the operator chose.
fn chosen(settings: &[Setting], answers: &[Answer], existing: usize) -> Chosen {
    let at = |title: &str| settings.iter().position(|setting| setting.title == title);
    let picked = |title: &str| -> Vec<String> {
        let Some(index) = at(title) else { return Vec::new() };
        match (settings.get(index), answers.get(index)) {
            (Some(setting), Some(Answer::Picked(picked))) => picked
                .iter()
                .filter_map(|at| setting.choices.get(*at))
                .map(|(value, _)| value.clone())
                .collect(),
            _ => Vec::new(),
        }
    };
    let text = |title: &str| match at(title).and_then(|index| answers.get(index)) {
        Some(Answer::Text(text)) => text.trim().to_owned(),
        _ => String::new(),
    };
    Chosen {
        selected: picked(INSTANCES).iter().filter_map(|id| id.parse().ok()).collect(),
        deploy: deploy::Choice {
            version: picked(VERSION).into_iter().next().unwrap_or_default(),
            // `NODE:STORAGE` in the list's order (by node), as the CTIDs are given.
            placements: picked(NODES),
            ctids: text(CTIDS),
            cores: text(CORES),
            memory_mib: text(MEMORY),
            root_mib: text(ROOT),
            home_mib: text(HOME),
            environment: text(ENVIRONMENT),
            hostname: text(HOSTNAME),
            domain: text(DOMAIN),
            nameserver: text(DNS_SERVER),
            existing,
        },
    }
}

/// Review the changes to the selected instances, the new instances and a
/// Pangolin attachment; only what changes is listed.
fn review(setup: &Setup, source: &Source, s: &mut Session, answers: &[Answer]) -> Review {
    let chosen = chosen(&s.settings, answers, s.instances.len());
    let attach_requested = service_pangolin::requested(&s.settings, answers);
    let mut sections = Vec::new();
    let mut problems = Vec::new();
    let (mut update_plan, mut change, mut create, mut attach) = (None, None, None, None);
    if chosen.selected.is_empty() && chosen.deploy.placements.is_empty() {
        problems.push(if s.instances.is_empty() {
            "Tick the nodes for the new instances under Nodes".to_owned()
        } else {
            "Tick instances to change under Instances, or nodes for new ones under Nodes".to_owned()
        });
    }
    let renamed = chosen.deploy.hostname != s.naming.hostname
        || chosen.deploy.domain != s.naming.domain
        || chosen.deploy.nameserver != s.naming.nameserver;
    if renamed && !chosen.selected.is_empty() {
        problems.push(
            "Hostname, Domain and DNS server apply to new instances; untick the instances to change them, or keep their values"
                .to_owned(),
        );
    }
    if !chosen.selected.is_empty() {
        let targets: Vec<&Instance> = s
            .instances
            .iter()
            .filter(|i| chosen.selected.contains(&i.ctid))
            .collect();
        let choice = lifecycle::Choice {
            ctids: chosen.selected.clone(),
            cores: chosen.deploy.cores.clone(),
            memory_mib: chosen.deploy.memory_mib.clone(),
            root_mib: chosen.deploy.root_mib.clone(),
            home_mib: chosen.deploy.home_mib.clone(),
            environment: chosen.deploy.environment.clone(),
            shown_environment: s.shown.clone(),
            remove: String::new(),
        };
        let mut plan = setup.handle.block_on(lifecycle::plan(
            &s.pve,
            &source.repository,
            &source.manifest.id,
            &choice,
        ));
        if source.is_site() && lifecycle::restarts(&plan) {
            match s.ready() {
                Some(site) => {
                    for target in plan.targets.iter().filter(|t| t.running) {
                        if let Err(error) = site.identity(target.ctid) {
                            plan.problems.push(error);
                        }
                    }
                }
                None => plan
                    .problems
                    .push("Restarting a Site requires the verified Pangolin context".to_owned()),
            }
        }
        let other_version: Vec<u32> = targets
            .iter()
            .filter(|i| tag(&i.image) != chosen.deploy.version)
            .map(|i| i.ctid)
            .collect();
        if !other_version.is_empty() {
            if plan.lines.is_empty() {
                let up = setup.handle.block_on(update::plan(
                    &s.pve,
                    &s.config,
                    source,
                    &other_version,
                    &chosen.deploy.version,
                    "",
                    s.ready(),
                ));
                sections.push(("Update".to_owned(), up.lines.clone()));
                problems.extend(up.problems.clone());
                update_plan = Some(up);
            } else {
                problems.push("Change the version in a separate deployment from sizes and the environment".to_owned());
            }
        }
        problems.extend(plan.problems.clone());
        if !plan.lines.is_empty() {
            sections.push(("Change".to_owned(), plan.lines.clone()));
            change = Some(plan);
        }
    }
    if !chosen.deploy.placements.is_empty() {
        let site = s.site.as_mut().map(|site| site as &mut dyn SiteInit);
        let plan = setup.handle.block_on(deploy::plan(
            &s.config,
            &s.pve,
            source,
            &s.cluster,
            &chosen.deploy,
            site,
            s.dns.as_ref().ok().and_then(Option::as_ref),
        ));
        if let Err(error) = &s.dns {
            problems.push(error.clone());
        }
        sections.extend(new_sections(source, &plan));
        problems.extend(plan.problems.clone());
        create = Some(plan);
    }
    if attach_requested {
        if !sections.is_empty() {
            problems.push("Attach to Pangolin in a separate deployment from other changes".to_owned());
        } else {
            let targets: Vec<Instance> = s
                .instances
                .iter()
                .filter(|i| chosen.selected.contains(&i.ctid))
                .cloned()
                .collect();
            let checked = s
                .context()
                .ok_or_else(|| "The Pangolin context is unavailable".to_owned())
                .and_then(|p| {
                    service_pangolin::review(
                        source,
                        p,
                        &setup.stop,
                        s.catalog.as_ref(),
                        &s.settings,
                        answers,
                        &targets,
                    )
                });
            match checked {
                Ok(checked) => {
                    sections.push(("Pangolin attachment".to_owned(), checked.lines()));
                    attach = Some(checked);
                }
                Err(error) => problems.push(error),
            }
        }
    }
    if sections.is_empty() && problems.is_empty() {
        problems.push("Nothing changes: the selected instances already have these values".to_owned());
    }
    (
        sections,
        problems,
        Reviewed::Change(Box::new(Changes {
            update: update_plan,
            change,
            create,
            attach,
        })),
    )
}

/// Apply the reviewed changes in order: the image, sizes and environment,
/// new instances, then the Pangolin attachment; the first failure stops.
fn apply(
    setup: &Setup,
    source: &Source,
    s: &mut Session,
    reviewed: Reviewed,
    tell: &(dyn Fn(Event) + Send + Sync),
) -> (bool, Vec<String>) {
    let say = |line: &str| tell(Event::Line(line.to_owned()));
    let mut lines = Vec::new();
    let fail = |mut lines: Vec<String>, error: String| {
        lines.push(error);
        (false, lines)
    };
    match reviewed {
        Reviewed::Remove(plan) => {
            match setup
                .handle
                .block_on(lifecycle::apply(&s.pve, source, &plan, s.ready(), &say))
            {
                Ok(done) => {
                    lines.extend(done);
                    let ok = forget(setup, source, s, &plan.targets, &mut lines);
                    (ok, lines)
                }
                Err(error) => fail(lines, error),
            }
        }
        Reviewed::Change(changes) => {
            let Changes {
                update: update_plan,
                change,
                create,
                attach,
            } = *changes;
            if let Some(plan) = update_plan {
                match setup
                    .handle
                    .block_on(update::apply(&s.pve, source, &plan, s.ready(), &say))
                {
                    Ok(done) => lines.extend(done),
                    Err(error) => return fail(lines, error),
                }
            }
            if let Some(plan) = change {
                match setup
                    .handle
                    .block_on(lifecycle::apply(&s.pve, source, &plan, s.ready(), &say))
                {
                    Ok(done) => lines.extend(done),
                    Err(error) => return fail(lines, error),
                }
            }
            if let Some(plan) = create {
                let site = s.site.as_mut().map(|site| site as &mut dyn SiteInit);
                let dns = s.dns.as_ref().ok().and_then(Option::as_ref);
                let (ok, done) = setup
                    .handle
                    .block_on(deploy::apply(&s.pve, source, &plan, site, dns, &say));
                lines.extend(done);
                if !ok {
                    return (false, lines);
                }
                // Clients reach new API instances by their names only.
                if source.is_api() {
                    let urls: Vec<String> = plan.instances.iter().map(|i| format!("https://{}", i.fqdn)).collect();
                    match client_urls(&setup.client_path, &urls, &[]) {
                        Ok(line) => lines.push(line),
                        Err(error) => return fail(lines, error),
                    }
                }
            }
            if let Some(plan) = attach {
                let result = match s.context() {
                    Some(pangolin) => {
                        setup
                            .handle
                            .block_on(service_pangolin::apply(&s.pve, pangolin, &setup.stop, &plan))
                    }
                    None => Err("The Pangolin context is unavailable; review again".to_owned()),
                };
                match result {
                    Ok(done) => lines.extend(done),
                    Err(error) => return fail(lines, error),
                }
            }
            (true, lines)
        }
    }
}

/// The review of new instances: what is made, nothing else.
fn new_sections(source: &Source, plan: &deploy::Plan) -> Vec<(String, Vec<String>)> {
    let version = tag(&plan.image);
    let mut lines = vec![format!("{} {version}", source.manifest.name)];
    for i in &plan.instances {
        if i.resume.is_some() {
            lines.push(format!("CT {}: resume initialization", i.ctid));
        }
        let home = if i.home_mib > 0 {
            format!(" + home {}", show_size(i.home_mib))
        } else {
            String::new()
        };
        lines.push(format!(
            "{}  CT {}  {}  {}  root {}{home} on {}  {} CPU  {} MiB",
            i.node,
            i.ctid,
            i.fqdn,
            i.address,
            show_size(i.root_mib),
            i.root_storage,
            i.cores,
            i.memory_mib
        ));
    }
    for (key, value) in &plan.environment {
        lines.push(format!("{key}={value}"));
    }
    let mut sections = vec![("New".to_owned(), lines)];
    if !plan.sites.is_empty() {
        sections.push(("Pangolin".to_owned(), plan.sites.clone()));
    }
    sections
}

/// Preserve edits across F5, matching choices by their stable values. Recreated
/// Sites have new IDs, so a stale selection is cleared rather than reused.
fn restore(settings: &mut [Setting], previous: &[Setting], answers: &[Answer]) {
    for (at, setting) in settings.iter_mut().enumerate() {
        let Some(old) = previous.get(at).filter(|old| old.title == setting.title) else {
            continue;
        };
        match answers.get(at) {
            Some(Answer::Text(text)) if setting.list => {
                setting.choices = text.split_whitespace().map(|v| (v.to_owned(), String::new())).collect();
            }
            Some(Answer::Text(text)) if setting.text.is_some() => setting.text = Some(text.clone()),
            Some(Answer::Picked(indices)) => {
                let values: Vec<_> = indices
                    .iter()
                    .filter_map(|i| old.choices.get(*i))
                    .map(|(v, _)| v)
                    .collect();
                setting.picked = setting
                    .choices
                    .iter()
                    .enumerate()
                    .filter_map(|(i, (value, _))| values.contains(&value).then_some(i))
                    .collect();
            }
            _ => {}
        }
    }
}

/// After removing `targets`: their names leave the local DNS and, for the
/// API, the client's servers. Whether that worked.
fn forget(setup: &Setup, source: &Source, s: &Session, targets: &[Instance], lines: &mut Vec<String>) -> bool {
    let dns = s.dns.as_ref().ok().and_then(Option::as_ref);
    let mut ok = true;
    let mut gone = Vec::new();
    for target in targets {
        let Some(domain) = &target.searchdomain else {
            continue;
        };
        let fqdn = format!("{}.{domain}", target.hostname);
        if let Some(dns) = dns.filter(|dns| dns.covers(&fqdn)) {
            match setup.handle.block_on(dns.remove(&fqdn)) {
                Ok(()) => lines.push(format!("{fqdn} removed from the local DNS")),
                Err(error) => {
                    ok = false;
                    lines.push(format!("{fqdn}: {error}"));
                }
            }
        }
        gone.push(format!("https://{fqdn}"));
    }
    if source.is_api() && !gone.is_empty() {
        match client_urls(&setup.client_path, &[], &gone) {
            Ok(line) => lines.push(line),
            Err(error) => {
                ok = false;
                lines.push(error);
            }
        }
    }
    ok
}

/// Add and remove server URLs in the client's `[server] urls`; the rest of
/// `client.toml` stays as it is.
fn client_urls(path: &std::path::Path, add: &[String], remove: &[String]) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|_| format!("{} is not valid TOML", path.display()))?;
    let urls = document["server"]["urls"]
        .as_array_mut()
        .ok_or_else(|| format!("{} has no [server] urls", path.display()))?;
    let mut kept: Vec<String> = urls
        .iter()
        .filter_map(|url| url.as_str().map(str::to_owned))
        .filter(|url| !remove.contains(url))
        .collect();
    for url in add {
        if !kept.contains(url) {
            kept.push(url.clone());
        }
    }
    if kept.is_empty() {
        return Err("The client would have no server left; client.toml is unchanged".to_owned());
    }
    *urls = kept.iter().map(String::as_str).collect();
    let temporary = path.with_extension("toml.new");
    std::fs::write(&temporary, document.to_string())
        .and_then(|()| std::fs::set_permissions(&temporary, std::fs::metadata(path)?.permissions()))
        .and_then(|()| std::fs::rename(&temporary, path))
        .map_err(|error| format!("Cannot write {}: {error}", path.display()))?;
    Ok(format!("client servers: {}", kept.join(", ")))
}
