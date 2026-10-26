//! The worker of the install screen in Service mode: one Deployment of a
//! manifest-defined service through the deploy token. The settings start
//! from what the cluster has (its instances) or what the manifest declares;
//! the review shows only what changes: new instances on the ticked nodes
//! (`crate::services::deploy`), the selected instances' image
//! (`crate::services::update`), sizes and environment
//! (`crate::services::lifecycle`), or with Del their removal.

use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use hs_deploy::config::DeployConfig;
use hs_deploy::node::show_size;
use hs_pve::PveClient;
use serde_json::Value;

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
use crate::services::{draft, lifecycle, public_dns, update};

// The settings by title: Instances only once there are some, Home only
// for a service with persistence.
const INSTANCES: &str = "Instances";
const VERSION: &str = "Version";
const REINSTALL: &str = "Reinstall image";
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
const COUNT: &str = "New instance count";
const SWAP: &str = "Swap (MiB)";

struct Session {
    config: DeployConfig,
    pve: PveClient,
    cluster: Cluster,
    instances: Vec<Instance>,
    settings: Vec<Setting>,
    /// Values read from the cluster before restoring the operator's edits.
    original_settings: Vec<Setting>,
    /// The environment the settings started from.
    shown: String,
    reviewed: Option<Reviewed>,
    /// Opened only when an operation needs the Pangolin API or Site initialization.
    site: Option<Initializer>,
    /// Another service published through Pangolin: its verified context.
    pangolin: Option<Pangolin>,
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
    problems: Vec<String>,
    application: Option<(String, String, std::collections::BTreeMap<String, serde_json::Value>)>,
    update: Option<update::Plan>,
    change: Option<lifecycle::Plan>,
    create: Option<deploy::Plan>,
    attach: Option<service_pangolin::Plan>,
    /// The public DNS record the published name still needs.
    record: Option<public_dns::Record>,
}

/// The review's sections, its problems and what applying it does.
type Review = (Vec<(String, Vec<String>)>, Vec<String>, Reviewed);

/// What the operator left in the settings.
struct Chosen {
    selected: Vec<u32>,
    reinstall: bool,
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
                s.reviewed = None;
                if let Err(error) = draft::save(&setup.token_dir, &source, &draft_of(&s.settings, &answers)) {
                    tell(Event::Line(error));
                }
                let chosen = chosen(&s.settings, &answers, s.instances.len());
                match switch_deployment(setup, &source, s, &chosen.selected, &answers) {
                    Ok(true) => {
                        tell(Event::Connected(found(&s.config, &s.cluster, &s.settings)));
                        continue;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        tell(Event::Reviewed {
                            sections: Vec::new(),
                            problems: vec![error],
                        });
                        continue;
                    }
                }
                let needs_pangolin = (source.is_site() && !chosen.deploy.placements.is_empty())
                    || service_pangolin::requested(&s.settings, &answers);
                if needs_pangolin && let Err(error) = ensure_pangolin(setup, &source, s, tell) {
                    tell(Event::Reviewed {
                        sections: Vec::new(),
                        problems: vec![error],
                    });
                    continue;
                }
                tell(Event::Busy("Checking the plan against the cluster".to_owned()));
                let (sections, problems, reviewed) = review(setup, &source, s, &answers, tell);
                tell(Event::Reviewed { sections, problems });
                s.reviewed = Some(reviewed);
            }
            Command::Remove { answers } => {
                let Some(s) = session.as_mut() else { continue };
                s.reviewed = None;
                tell(Event::Busy("Reading what would be removed".to_owned()));
                let chosen = chosen(&s.settings, &answers, s.instances.len());
                if source.is_site()
                    && !chosen.selected.is_empty()
                    && let Err(error) = ensure_pangolin(setup, &source, s, tell)
                {
                    tell(Event::Reviewed {
                        sections: Vec::new(),
                        problems: vec![error],
                    });
                    continue;
                }
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
                        &s.config.pool,
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
                let (ok, lines) = setup.handle.block_on(async {
                    tokio::select! {
                        biased;
                        () = async {
                            while !setup.stop.load(Ordering::SeqCst) {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                            }
                        } => (false, vec![
                            "Operation cancelled. Submitted Proxmox tasks may still finish; rescan before continuing.".to_owned()
                        ]),
                        result = apply(setup, &source, s, reviewed, tell) => result,
                    }
                });
                if ok {
                    draft::remove(&setup.token_dir, &source);
                }
                tell(Event::Done { ok, lines });
            }
            Command::Cancel => return,
        }
    }
}

/// Selecting another named deployment reloads that deployment's private
/// values before Review. Values are never inferred from another instance.
fn switch_deployment(
    setup: &Setup,
    source: &Source,
    session: &mut Session,
    selected: &[u32],
    answers: &[Answer],
) -> Result<bool, String> {
    if source.manifest.schema3.is_none() {
        return Ok(false);
    }
    let Some(target) = session.instances.iter().find(|i| selected.contains(&i.ctid)) else {
        return Ok(false);
    };
    let hostname = base_name(&target.hostname);
    let domain = target
        .searchdomain
        .as_deref()
        .ok_or("Selected deployment has no domain")?
        .to_owned();
    if session
        .instances
        .iter()
        .filter(|i| selected.contains(&i.ctid))
        .any(|i| base_name(&i.hostname) != hostname || i.searchdomain.as_deref() != Some(&domain))
    {
        return Err("Select instances from one named deployment at a time".into());
    }
    if session.naming.hostname == hostname && session.naming.domain == domain {
        return Ok(false);
    }
    let saved = crate::services::selections::load(&setup.token_dir, source, &hostname, &domain, true)?
        .ok_or("Private application selections are unavailable")?;
    let previous = session.settings.clone();
    restore(&mut session.settings, &previous, answers);
    session.shown = lifecycle::shown_environment(source, target);
    session.naming = Naming {
        hostname,
        domain,
        nameserver: target.nameserver.clone().unwrap_or_default(),
    };
    for setting in &mut session.settings {
        let value = match setting.title.as_str() {
            HOSTNAME => Some(session.naming.hostname.clone()),
            DOMAIN => Some(session.naming.domain.clone()),
            DNS_SERVER => Some(session.naming.nameserver.clone()),
            CORES => Some(target.cores.to_string()),
            MEMORY => Some(target.memory_mib.to_string()),
            ROOT => Some(show_size(target.root_mib)),
            HOME => Some(show_size(target.home_mib)),
            _ => None,
        };
        if let Some(value) = value {
            setting.text = Some(value);
        }
        if setting.title == VERSION {
            setting.picked = setting
                .choices
                .iter()
                .position(|(v, _)| v == tag(&target.image))
                .into_iter()
                .collect();
        }
        if setting.title == ENVIRONMENT {
            setting.choices = session
                .shown
                .split_whitespace()
                .map(|v| (v.into(), String::new()))
                .collect();
        }
        if let Some(input) = &setting.application_input {
            let value = saved
                .values
                .get(&input.id)
                .or(input.default.as_ref())
                .map(crate::services::schema3::display)
                .unwrap_or_default();
            if setting.text.is_some() {
                setting.text = Some(value);
            } else {
                setting.picked = setting
                    .choices
                    .iter()
                    .position(|(v, _)| v == &value)
                    .into_iter()
                    .collect();
            }
        }
    }
    session.original_settings = session.settings.clone();
    Ok(true)
}

/// The deploy token's client, cluster, service instances, any already open
/// Pangolin context, and the settings (those of `kept`, a rescan).
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
    let config = DeployConfig::load(&setup.token_dir.join("deploy.toml"))?;
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
    let (site, pangolin) = match previous {
        Some(p) => (p.site.filter(Initializer::alive), p.pangolin.filter(Pangolin::alive)),
        None => (None, None),
    };
    let shown = instances
        .first()
        .map(|instance| lifecycle::shown_environment(source, instance))
        .unwrap_or_default();
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
        .block_on(deploy::automatic(&config, &pve, &instances, cluster.nodes.len()))
        .unwrap_or_default();
    let mut settings = settings(source, &cluster, &instances, &automatic, &naming, &shown, setup.offset);
    if let Some(contract) = &source.manifest.schema3 {
        let saved = crate::services::selections::load(
            &setup.token_dir,
            source,
            &naming.hostname,
            &naming.domain,
            !instances.is_empty(),
        )?;
        for input in &contract.inputs {
            let value = saved
                .as_ref()
                .and_then(|s| s.values.get(&input.id))
                .or(input.default.as_ref());
            let mut row = setting(
                &input.title(),
                &input.description,
                Vec::new(),
                false,
                Some(value.map(crate::services::schema3::display).unwrap_or_default()),
            );
            row.application_input = Some(Box::new(input.clone()));
            if input.kind == crate::services::schema3::Kind::Secret {
                row.text = None;
                row.choices = vec![(
                    "unavailable".into(),
                    "Protected secret delivery is not configured".into(),
                )];
            }

            if matches!(
                input.kind,
                crate::services::schema3::Kind::Enum | crate::services::schema3::Kind::Boolean
            ) {
                let choices = if input.kind == crate::services::schema3::Kind::Boolean {
                    vec!["true".into(), "false".into()]
                } else {
                    input.choices.clone()
                };
                row.choices = choices.into_iter().map(|v| (v, String::new())).collect();
                if !input.required && input.default.is_none() {
                    row.choices.insert(0, (String::new(), "Omit optional value".into()));
                }

                row.picked = row
                    .choices
                    .iter()
                    .position(|(v, _)| row.text.as_deref() == Some(v))
                    .into_iter()
                    .collect();
                row.text = None;
            }
            settings.push(row);
        }
    }
    settings.extend(service_pangolin::settings(source));
    let original_settings = settings.clone();
    match kept {
        Some((previous, answers)) => restore(&mut settings, &previous, &answers),
        None => apply_draft(&mut settings, &draft::load(&setup.token_dir, source)),
    }
    let found = found(&config, &cluster, &settings);
    Ok((
        Session {
            config,
            pve,
            cluster,
            instances,
            settings,
            original_settings,
            shown,
            reviewed: None,
            site,
            pangolin,
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

/// Open the Pangolin API only for operations that actually use it.
fn ensure_pangolin(
    setup: &Setup,
    source: &Source,
    session: &mut Session,
    tell: &(dyn Fn(Event) + Send + Sync),
) -> Result<(), String> {
    if session.context().is_some_and(Pangolin::alive) {
        return Ok(());
    }
    let pangolin = open_pangolin(setup, tell)?;
    if source.is_site() {
        session.site = Some(Initializer::open(pangolin, session.config.clone())?);
    } else {
        session.pangolin = Some(pangolin);
    }
    Ok(())
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
        application_input: None,
        auto_text: false,
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
         are updated only when you change this version or enable Reinstall image. Existing persistent volumes are kept.",
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
    settings.push(version_setting);
    if !instances.is_empty() {
        settings.push(setting(
            REINSTALL,
            "Explicitly replace the selected instances' root disks, including the same image version. \
             Existing persistent volumes are kept. Disabled by default.",
            vec![("enabled".to_owned(), "replace selected root disks".to_owned())],
            true,
            None,
        ));
    }
    settings.extend([
        nodes,
        checked(
            setting(
                CTIDS,
                "Auto selects free CTIDs. Choose Manual to enter one CTID per new instance, separated by commas.",
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
    if manifest.schema3.is_some() {
        settings.push(checked(
            setting(
                COUNT,
                "Number of new containers; distributed across the selected nodes and storages in order.",
                Vec::new(),
                false,
                Some(manifest.deployment.instances.to_string()),
            ),
            check_count,
        ));
        settings.push(checked(
            setting(
                SWAP,
                "Swap limit per new container, in MiB; zero disables swap.",
                Vec::new(),
                false,
                Some(manifest.resources.swap_mib.to_string()),
            ),
            check_swap,
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
        ctids.auto_text = true;
        ctids.picked = vec![0];
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

fn check_count(text: &str) -> Result<(), String> {
    number_in(text, 1, 64)
}
fn check_swap(text: &str) -> Result<(), String> {
    number_in(text, 0, 1_048_576)
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
        reinstall: !picked(REINSTALL).is_empty(),
        deploy: deploy::Choice {
            application: settings
                .iter()
                .enumerate()
                .filter_map(|(index, setting)| {
                    let input = setting.application_input.as_ref()?;
                    let value = match answers.get(index) {
                        Some(Answer::Text(value)) => value.clone(),
                        Some(Answer::Picked(indices)) => indices
                            .first()
                            .and_then(|i| setting.choices.get(*i))
                            .map(|(v, _)| v.clone())
                            .unwrap_or_default(),
                        _ => String::new(),
                    };
                    Some((input.id.clone(), value))
                })
                .collect(),
            instance_count: text(COUNT),
            swap_mib: text(SWAP),
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
fn review(
    setup: &Setup,
    source: &Source,
    s: &mut Session,
    answers: &[Answer],
    tell: &(dyn Fn(Event) + Send + Sync),
) -> Review {
    let chosen = chosen(&s.settings, answers, s.instances.len());
    let attach_requested = service_pangolin::requested(&s.settings, answers);
    let mut sections = Vec::new();
    let mut problems = Vec::new();
    let application =
        source
            .manifest
            .schema3
            .as_ref()
            .and_then(|contract| match contract.values(&chosen.deploy.application) {
                Ok(values) => Some((chosen.deploy.hostname.clone(), chosen.deploy.domain.clone(), values)),
                Err(error) => {
                    problems.push(error);
                    None
                }
            });
    if let Some(contract) = &source.manifest.schema3 {
        match deploy::environment(&chosen.deploy.environment) {
            Ok(entries) if entries.iter().any(|(key, _)| contract.owns_environment(key)) => {
                problems.push("Application variables must be edited in Application configuration".into())
            }
            Err(error) => problems.push(error),
            _ => {}
        }
        for target in s.instances.iter().filter(|i| chosen.selected.contains(&i.ctid)) {
            if base_name(&target.hostname) != chosen.deploy.hostname
                || target.searchdomain.as_deref() != Some(chosen.deploy.domain.as_str())
            {
                problems.push("Select instances from the deployment whose hostname and domain are shown; different deployments require their own local configuration".into());
            }
            if let Err(error) = crate::services::selections::load(
                &setup.token_dir,
                source,
                &base_name(&target.hostname),
                target.searchdomain.as_deref().unwrap_or_default(),
                true,
            ) {
                problems.push(error);
            }
        }
    }
    let (mut update_plan, mut change, mut create, mut attach, mut record) = (None, None, None, None, None);
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
        // Displayed defaults also serve new instances. Existing instances only
        // receive values that the operator changed from the displayed defaults.
        let changed = |title: &str, value: &str| {
            let original = s
                .original_settings
                .iter()
                .find(|setting| setting.title == title)
                .and_then(|setting| setting.text.as_deref())
                .unwrap_or_default();
            if value.trim() == original.trim() {
                String::new()
            } else {
                value.to_owned()
            }
        };
        let choice = lifecycle::Choice {
            ctids: chosen.selected.clone(),
            cores: changed(CORES, &chosen.deploy.cores),
            memory_mib: changed(MEMORY, &chosen.deploy.memory_mib),
            root_mib: changed(ROOT, &chosen.deploy.root_mib),
            home_mib: changed(HOME, &chosen.deploy.home_mib),
            environment: chosen.deploy.environment.clone(),
            shown_environment: s.shown.clone(),
            remove: String::new(),
        };
        let mut plan = setup.handle.block_on(lifecycle::plan(
            &s.pve,
            &s.config.pool,
            &source.repository,
            &source.manifest.id,
            &choice,
        ));
        if let (Some(contract), Some((_, _, values))) = (&source.manifest.schema3, &application) {
            match contract.resolve(values) {
                Ok(environment) => {
                    for target in &plan.targets {
                        if environment.iter().any(|entry| !target.environment.contains(entry)) {
                            plan.lines.push(format!(
                                "CT {}: apply Application configuration; restart if running",
                                target.ctid
                            ));
                        }
                    }
                    plan.set.extend(environment);
                    plan.unset
                        .extend(contract.inputs.iter().filter_map(|input| match &input.target {
                            crate::services::schema3::Target::Environment { variable }
                                if !plan.set.iter().any(|(key, _)| key == variable) =>
                            {
                                Some(variable.clone())
                            }
                            _ => None,
                        }));
                    for target in &plan.targets {
                        if plan
                            .unset
                            .iter()
                            .any(|key| target.environment.iter().any(|(k, _)| k == key))
                        {
                            plan.lines.push(format!(
                                "CT {}: remove optional application variables; restart if running",
                                target.ctid
                            ));
                        }
                    }
                    if let Err(error) = crate::services::schema3::validate_environment(&plan.set) {
                        plan.problems.push(error);
                    }
                }
                Err(error) => plan.problems.push(error),
            }
        }
        if source.is_site() && lifecycle::restarts(&plan) {
            if let Err(error) = ensure_pangolin(setup, source, s, tell) {
                plan.problems.push(error);
            }
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
        let original_version = s
            .original_settings
            .iter()
            .find(|setting| setting.title == VERSION)
            .map(Setting::value)
            .unwrap_or_default();
        let replace_image = chosen.reinstall || chosen.deploy.version != original_version;
        let image_targets: Vec<u32> = s
            .instances
            .iter()
            .filter(|i| {
                chosen.selected.contains(&i.ctid)
                    && replace_image
                    && (chosen.reinstall || tag(&i.image) != chosen.deploy.version)
            })
            .map(|i| i.ctid)
            .collect();
        let changes = !plan.lines.is_empty();
        problems.extend(plan.problems.clone());
        if changes {
            sections.push(("Change".to_owned(), plan.lines.clone()));
            change = Some(plan);
        }
        if !image_targets.is_empty() {
            // Apply makes the change first; the image replacement then
            // checks the reconciled environment against the saved values.
            let up = setup.handle.block_on(update::plan(
                &s.pve,
                &s.config,
                source,
                &image_targets,
                &chosen.deploy.version,
                "",
                &setup.token_dir,
                changes,
            ));
            sections.push(("Update".to_owned(), up.lines.clone()));
            problems.extend(up.problems.clone());
            update_plan = Some(up);
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
    // Apply attaches last, after the instance is changed, restarted or
    // created, and checks again that it runs with the reviewed address.
    if attach_requested {
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
                    &targets,
                    create
                        .as_ref()
                        .map(|plan| plan.instances.as_slice())
                        .unwrap_or_default(),
                )
            });
        match checked {
            Ok(checked) => {
                sections.push(("Pangolin attachment".to_owned(), checked.lines()));
                match public_record(setup, s, checked.domain()) {
                    Ok(missing) => {
                        sections.push((
                            "Public DNS".to_owned(),
                            vec![format!(
                                "{} CNAME {} → {} in {}; wait for public DNS before Pangolin",
                                if missing.exists { "Verify" } else { "Add" },
                                missing.name(),
                                missing.target,
                                missing.provider_name()
                            )],
                        ));
                        record = Some(missing);
                    }
                    Err(error) => problems.push(error),
                }
                attach = Some(checked);
            }
            Err(error) => problems.push(error),
        }
    }
    if sections.is_empty() && problems.is_empty() {
        problems.push("Nothing changes: the selected instances already have these values".to_owned());
    }
    (
        sections,
        problems.clone(),
        Reviewed::Change(Box::new(Changes {
            problems: problems.clone(),
            application,
            update: update_plan,
            change,
            create,
            attach,
            record,
        })),
    )
}

/// Apply the reviewed changes in order: sizes and environment, the image,
/// new instances, then the Pangolin attachment; the first failure stops.
async fn apply(
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
        Reviewed::Remove(plan) => match lifecycle::apply(&s.pve, source, &plan, s.ready(), &say).await {
            Ok(done) => {
                lines.extend(done);
                let ok = forget(setup, source, s, &plan.targets, &mut lines).await;
                (ok, lines)
            }
            Err(error) => fail(lines, error),
        },
        Reviewed::Change(changes) => {
            let Changes {
                problems,
                application,
                update: update_plan,
                change,
                create,
                attach,
                record,
            } = *changes;
            if !problems.is_empty() {
                return fail(lines, "Resolve Review problems before Apply".into());
            }
            if source.manifest.schema3.is_some() {
                let targets = update_plan
                    .as_ref()
                    .map(|p| p.targets.as_slice())
                    .unwrap_or_default()
                    .iter()
                    .chain(change.as_ref().map(|p| p.targets.as_slice()).unwrap_or_default());
                for target in targets {
                    if let Err(error) = crate::services::selections::load(
                        &setup.token_dir,
                        source,
                        &base_name(&target.hostname),
                        target.searchdomain.as_deref().unwrap_or_default(),
                        true,
                    ) {
                        return fail(lines, error);
                    }
                }
            }
            if let Some((hostname, domain, values)) = application
                && let Err(error) =
                    crate::services::selections::save(&setup.token_dir, source, &hostname, &domain, &values)
            {
                return fail(lines, error);
            }
            if let Some(plan) = change {
                match lifecycle::apply(&s.pve, source, &plan, s.ready(), &say).await {
                    Ok(done) => lines.extend(done),
                    Err(error) => return fail(lines, error),
                }
            }
            if let Some(plan) = update_plan {
                match update::apply(&s.pve, source, &plan, &say).await {
                    Ok(done) => lines.extend(done),
                    Err(error) => return fail(lines, error),
                }
            }
            if let Some(plan) = create {
                let site = s.site.as_mut().map(|site| site as &mut dyn SiteInit);
                let dns = s.dns.as_ref().ok().and_then(Option::as_ref);
                let (ok, done) = deploy::apply(&s.pve, source, &plan, site, dns, &say).await;
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
            if let Some(record) = record {
                let current_target = match pangolin_host(s) {
                    Ok(target) => target,
                    Err(error) => return fail(lines, error),
                };
                if !current_target
                    .trim_end_matches('.')
                    .eq_ignore_ascii_case(&record.target)
                {
                    return fail(
                        lines,
                        "Pangolin's public address changed since review; review again".into(),
                    );
                }
                say(&format!(
                    "Preparing public DNS for {}; waiting for propagation",
                    record.name()
                ));
                match public_dns::apply(&setup.token_dir, &record).await {
                    Ok(line) => lines.push(line),
                    Err(error) => return fail(lines, error),
                }
            }
            if let Some(plan) = attach {
                let result = match s.context() {
                    Some(pangolin) => service_pangolin::apply(&s.pve, pangolin, &setup.stop, &plan).await,
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
    if let Some(resolved) = &plan.resolved {
        lines.push(format!(
            "Image digest: {} · linux/amd64 {}",
            resolved.digest, resolved.platform
        ));
    }
    for (key, value) in &plan.environment {
        if source.manifest.schema3.is_some() {
            lines.push(format!("{key}: configured"));
        } else {
            lines.push(format!("{key}={value}"));
        }
    }
    if source.manifest.schema3.is_some() {
        lines.push("Rootfs-local application data survives restarts and is discarded by image replacement.".into());
    }
    let mut sections = vec![("New".to_owned(), lines)];
    if !plan.sites.is_empty() {
        sections.push(("Pangolin".to_owned(), plan.sites.clone()));
    }
    sections
}

/// The form as a draft: each setting's text or chosen values, without
/// secrets.
fn draft_of(settings: &[Setting], answers: &[Answer]) -> draft::Values {
    settings
        .iter()
        .zip(answers)
        .filter(|(setting, _)| {
            setting
                .application_input
                .as_ref()
                .is_none_or(|input| input.kind != crate::services::schema3::Kind::Secret)
        })
        .map(|(setting, answer)| {
            let value = match answer {
                Answer::Text(text) => Value::from(text.clone()),
                Answer::Picked(indices) => indices
                    .iter()
                    .filter_map(|i| setting.choices.get(*i))
                    .map(|(value, _)| Value::from(value.clone()))
                    .collect(),
            };
            (setting.title.clone(), value)
        })
        .collect()
}

/// Fill the form from a saved draft, matching settings by title and choices
/// by their stable values; values the form no longer offers are skipped.
fn apply_draft(settings: &mut [Setting], draft: &draft::Values) {
    for setting in settings {
        match draft.get(&setting.title) {
            Some(Value::String(text)) if setting.list => {
                setting.choices = text.split_whitespace().map(|v| (v.to_owned(), String::new())).collect();
            }
            Some(Value::String(text)) if setting.text.is_some() => {
                setting.text = Some(text.clone());
                if setting.auto_text {
                    setting.picked = vec![usize::from(!text.is_empty())];
                }
            }
            Some(Value::Array(values)) => {
                setting.picked = setting
                    .choices
                    .iter()
                    .enumerate()
                    .filter_map(|(i, (value, _))| values.iter().any(|v| v == value.as_str()).then_some(i))
                    .collect();
            }
            _ => {}
        }
    }
}

fn pangolin_host(s: &Session) -> Result<String, String> {
    let (ctx, _) = s
        .context()
        .ok_or("The Pangolin context is unavailable")?
        .api_context()?;
    url::Url::parse(&ctx.url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .ok_or_else(|| "The Pangolin server has no public host name".to_owned())
}

/// Review the public DNS record needed by the Pangolin resource.
fn public_record(setup: &Setup, s: &Session, domain: &str) -> Result<public_dns::Record, String> {
    let target = pangolin_host(s)?;
    setup
        .handle
        .block_on(public_dns::review(&setup.token_dir, domain, &target))
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
            Some(Answer::Text(text)) if setting.text.is_some() => {
                setting.text = Some(text.clone());
                if setting.auto_text {
                    setting.picked = vec![usize::from(!text.is_empty())];
                }
            }
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
async fn forget(setup: &Setup, source: &Source, s: &Session, targets: &[Instance], lines: &mut Vec<String>) -> bool {
    let dns = s.dns.as_ref().ok().and_then(Option::as_ref);
    let mut ok = true;
    let mut gone = Vec::new();
    for target in targets {
        let Some(domain) = &target.searchdomain else {
            continue;
        };
        let fqdn = format!("{}.{domain}", target.hostname);
        if let Some(dns) = dns.filter(|dns| dns.covers(&fqdn)) {
            match dns.remove(&fqdn).await {
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
