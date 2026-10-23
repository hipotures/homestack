//! The worker of the install screen in Service mode: the deployment of a
//! manifest-defined service through the deploy token
//! (`crate::services::deploy`). It reads the cluster and the published
//! releases, plans the settings and applies the reviewed plan.

use std::sync::mpsc::Receiver;

use hs_deploy::config::DeployConfig;
use hs_pve::PveClient;

use super::install::{Event, Found, Mark, Setting};
use super::installing::{Command, Setup};
use super::pangolin_sites::managed::{Initializer, Pangolin};
use super::reinstall::local_time;
use crate::install_flow::Answer;
use crate::services::deploy::{self, Choice, Cluster, Plan, SiteInit, Source};
use crate::services::manifest::MOUNT;

const VERSION: usize = 0;
const PLACEMENT: usize = 1;
const CTIDS: usize = 2;
const CORES: usize = 3;
const MEMORY: usize = 4;
const ROOT_GIB: usize = 5;
const HOME_GIB: usize = 6;
const ENVIRONMENT: usize = 7;

struct Session {
    config: DeployConfig,
    pve: PveClient,
    cluster: Cluster,
    settings: Vec<Setting>,
    reviewed: Option<Plan>,
    /// A Pangolin Site connector: the verified Pangolin context and root.
    site: Option<Initializer>,
}

pub(super) fn work(setup: &Setup, commands: &Receiver<Command>, tell: &(dyn Fn(Event) + Send + Sync)) {
    let Some(source) = setup.service.clone() else {
        tell(Event::Failed("No service was chosen.".to_owned()));
        return;
    };
    if setup.service_manage {
        super::service_lifecycle::work(setup, &source, commands, tell);
        return;
    }
    let mut session: Option<Session> = None;
    while let Ok(command) = commands.recv() {
        match command {
            Command::Connect { .. } | Command::Rescan { .. } => {
                let answers = match &command {
                    Command::Rescan { answers, .. } => Some(answers.clone()),
                    _ => None,
                };
                tell(Event::Busy("Reading the cluster and the published releases".to_owned()));
                let kept = session
                    .as_ref()
                    .zip(answers.as_ref())
                    .map(|(s, answers)| choice(&s.settings, answers));
                let site = session.take().and_then(|s| s.site).filter(Initializer::alive);
                let opened =
                    setup
                        .handle
                        .block_on(open(setup, &source, kept.as_ref()))
                        .and_then(|(mut opened, found)| {
                            if source.is_site() {
                                opened.site = Some(match site {
                                    Some(site) => site,
                                    None => initializer(setup, &opened.config, tell)?,
                                });
                            }
                            Ok((opened, found))
                        });
                match opened {
                    Ok((opened, found)) => {
                        session = Some(opened);
                        tell(Event::Connected(found));
                    }
                    Err(error) => tell(Event::Failed(error)),
                }
            }
            Command::Review { answers, .. } => {
                let Some(s) = session.as_mut() else { continue };
                tell(Event::Busy("Checking the plan against the cluster".to_owned()));
                let choice = choice(&s.settings, &answers);
                let site = s.site.as_mut().map(|site| site as &mut dyn SiteInit);
                let plan = setup
                    .handle
                    .block_on(deploy::plan(&s.config, &s.pve, &source, &s.cluster, &choice, site));
                tell(Event::Reviewed {
                    sections: sections(&source, &s.cluster, &plan, s.site.as_ref(), setup.offset),
                    problems: plan.problems.clone(),
                });
                s.reviewed = Some(plan);
            }
            Command::Apply => {
                let Some(s) = session.as_mut() else { continue };
                let Some(plan) = s.reviewed.take() else { continue };
                let say = |line: &str| tell(Event::Line(line.to_owned()));
                let site = s.site.as_mut().map(|site| site as &mut dyn SiteInit);
                let (ok, lines) = setup.handle.block_on(deploy::apply(&s.pve, &source, &plan, site, &say));
                tell(Event::Done { ok, lines });
            }
            Command::Cancel => return,
        }
    }
}

/// The deploy token's client, the cluster, and the settings: those of
/// `kept` (a rescan), else defaults.
async fn open(setup: &Setup, source: &Source, kept: Option<&Choice>) -> Result<(Session, Found), String> {
    let config = DeployConfig::load(&setup.token_dir.join("deploy.toml"))
        .map_err(|error| format!("{error}. Install HomeStack before deploying services."))?;
    let pve = hs_deploy::deploy::deploy_client(&config)?;
    let cluster = deploy::survey(&config, &pve, source).await?;
    if cluster.releases.is_empty() {
        return Err(format!(
            "{} has no published release (a numeric X.Y.Z tag); dev images are not offered.",
            source.manifest.image_repository
        ));
    }
    let settings = settings(source, &cluster, kept, setup.offset);
    let check = vec![
        (
            Mark::Ok,
            "access".to_owned(),
            format!("deploy token, pool {}", config.pool),
        ),
        (Mark::Ok, "nodes".to_owned(), cluster.nodes.join(", ")),
    ];
    let found = Found {
        root: format!("deploy token · PVE {}", cluster.version),
        check,
        settings: settings.clone(),
        domain: None,
    };
    Ok((
        Session {
            config,
            pve,
            cluster,
            settings,
            reviewed: None,
            site: None,
        },
        found,
    ))
}

/// A Site connector's trusted context: its own verified Pangolin tunnel and
/// Step 7 key from the saved settings; the credential goes through the console.
fn initializer(
    setup: &Setup,
    config: &DeployConfig,
    tell: &(dyn Fn(Event) + Send + Sync),
) -> Result<Initializer, String> {
    tell(Event::Busy(
        "Opening the Pangolin API tunnel (touch your key or give its PIN if it asks)".to_owned(),
    ));
    let pangolin = Pangolin::open(
        &setup.token_dir,
        std::sync::Arc::clone(&setup.system),
        setup.ssh_prefix.clone(),
        setup.sounds.clone(),
        &setup.stop,
    )?;
    Initializer::open(pangolin, config.clone())
}

fn gib(bytes: u64) -> u64 {
    bytes / (1024 * 1024 * 1024)
}

#[allow(clippy::too_many_lines)]
fn settings(source: &Source, cluster: &Cluster, kept: Option<&Choice>, offset: i64) -> Vec<Setting> {
    let manifest = &source.manifest;
    let picked = |choices: &[(String, String)], wanted: &[String]| -> Vec<usize> {
        wanted
            .iter()
            .filter_map(|value| choices.iter().position(|(choice, _)| choice == value))
            .collect()
    };
    let releases: Vec<(String, String)> = cluster
        .releases
        .iter()
        .map(|release| {
            let tag = release.image.rsplit_once(':').map_or("", |(_, tag)| tag).to_owned();
            let made = release
                .created
                .as_deref()
                .and_then(|at| local_time(at, offset))
                .unwrap_or_else(|| "—".to_owned());
            (tag, made)
        })
        .collect();
    let version = kept
        .map(|k| picked(&releases, std::slice::from_ref(&k.version)))
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| vec![0]);
    // One row per node and storage, the nodes in order: the node gets an
    // instance whose disk and home volume are made on that storage.
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
    let text = |value: Option<&String>, default: &str| {
        Some(
            value
                .filter(|v| !v.is_empty())
                .cloned()
                .unwrap_or_else(|| default.to_owned()),
        )
    };
    vec![
        Setting {
            list: false,
            title: "Version".to_owned(),
            note: format!(
                "A published release of {}: numeric X.Y.Z tags only, newest first; dev images are never offered. \
                 The review resolves its digest and checks its linux/amd64 image.",
                manifest.image_repository
            ),
            multiple: false,
            choices: releases,
            picked: version,
            text: None,
        },
        Setting {
            list: false,
            title: "Nodes".to_owned(),
            note:
                "Tick one storage on each node that gets a new instance: its container, disk and home volume are made \
                   there, new volumes of the sizes below; nothing on the storage is changed. Instances are \
                   independent: each keeps its own data, nothing is shared or balanced between them. Each is an \
                   unprivileged container in pool homestack-service with tag homestack-server, its health checked \
                   from this desktop after the start and again after one restart. The first failure stops the run \
                   and nothing is removed. Existing containers are not touched."
                    .to_owned(),
            multiple: true,
            picked: kept.map(|k| picked(&placements, &k.placements)).unwrap_or_default(),
            choices: placements,
            text: None,
        },
        Setting {
            list: false,
            title: "CTID".to_owned(),
            note: "Empty: the first free CTIDs, shown in the review. Or one CTID per ticked node, in the order of \
                   the nodes, separated by commas. The address follows from it: 102240 is .102.240. To resume an \
                   interrupted API initialization, give its CTID and retain the same version, node and sizes."
                .to_owned(),
            multiple: false,
            choices: Vec::new(),
            picked: Vec::new(),
            text: text(kept.map(|k| &k.ctids), ""),
        },
        Setting {
            list: false,
            title: "Cores".to_owned(),
            note: "CPU cores of each container.".to_owned(),
            multiple: false,
            choices: Vec::new(),
            picked: Vec::new(),
            text: text(kept.map(|k| &k.cores), "1"),
        },
        Setting {
            list: false,
            title: "Memory (MiB)".to_owned(),
            note: "Memory of each container, without swap.".to_owned(),
            multiple: false,
            choices: Vec::new(),
            picked: Vec::new(),
            text: text(kept.map(|k| &k.memory_mib), "512"),
        },
        Setting {
            list: false,
            title: "Disk".to_owned(),
            note: "Size of each container's root disk, a number with M or G (e.g. 512M, 1G); it is replaced with \
                   the image and holds no data."
                .to_owned(),
            multiple: false,
            choices: Vec::new(),
            picked: Vec::new(),
            text: text(kept.map(|k| &k.root_mib), "1G"),
        },
        Setting {
            list: false,
            title: "Home".to_owned(),
            note: format!(
                "Size of each instance's home volume at {MOUNT}, where the service keeps its data, a number with M \
                 or G (e.g. 10M, 2G)."
            ),
            multiple: false,
            choices: Vec::new(),
            picked: Vec::new(),
            // A Site keeps only its credential there.
            text: text(kept.map(|k| &k.home_mib), if source.is_site() { "10M" } else { "1G" }),
        },
        Setting {
            list: false,
            title: "Environment".to_owned(),
            note: "KEY=value entries separated by spaces, as the application's documentation asks (the manifest \
                   declares none). They are kept in the container configuration, readable in Proxmox: no secrets."
                .to_owned(),
            multiple: false,
            choices: Vec::new(),
            picked: Vec::new(),
            text: text(kept.map(|k| &k.environment), ""),
        },
    ]
}

/// The answers as a choice.
fn choice(settings: &[Setting], answers: &[Answer]) -> Choice {
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
    let text = |index: usize| match answers.get(index) {
        Some(Answer::Text(text)) => text.trim().to_owned(),
        _ => String::new(),
    };
    Choice {
        version: picked(VERSION).into_iter().next().unwrap_or_default(),
        // `NODE:STORAGE` in the list's order (by node), as the CTIDs are given.
        placements: picked(PLACEMENT),
        ctids: text(CTIDS),
        cores: text(CORES),
        memory_mib: text(MEMORY),
        root_mib: text(ROOT_GIB),
        home_mib: text(HOME_GIB),
        environment: text(ENVIRONMENT),
    }
}

/// The review: what is made, nothing else.
fn sections(
    source: &Source,
    cluster: &Cluster,
    plan: &Plan,
    site: Option<&Initializer>,
    offset: i64,
) -> Vec<(String, Vec<String>)> {
    let manifest = &source.manifest;
    let version = plan.image.rsplit_once(':').map_or("", |(_, tag)| tag);
    let made = cluster
        .releases
        .iter()
        .find(|r| r.image == plan.image)
        .and_then(|r| r.created.as_deref())
        .and_then(|at| local_time(at, offset))
        .map(|made| format!(", made {made}"))
        .unwrap_or_default();
    let mut lines = vec![format!("{} {version}{made}", manifest.name)];
    for i in &plan.instances {
        if i.resume.is_some() {
            lines.push(format!(
                "Resume initialization in CT {} using its existing home; no container or volume is created",
                i.ctid
            ));
        }
        lines.push(format!(
            "{}  CT {}  {}  {}  disk {} + home {} on {}  {} CPU  {} MiB",
            i.node,
            i.ctid,
            i.hostname,
            i.address,
            hs_deploy::node::show_size(i.root_mib),
            hs_deploy::node::show_size(i.home_mib),
            i.root_storage,
            i.cores,
            i.memory_mib
        ));
    }
    for (key, value) in &plan.environment {
        lines.push(format!("{key}={value}"));
    }
    if source.is_site() {
        lines.extend(site.map(Initializer::describe));
        lines.extend(plan.sites.iter().cloned());
        lines.push(format!(
            "Site: HOMESTACK_MANAGED_HOME=1 before the first start; credential and journal saved on this desktop \
             before Pangolin is asked; the credential reaches {MOUNT}/pangolin-site/site.json only over root's \
             standard input; then CT running and exactly its Site online. Autostart is enabled only after that. \
             No Resource is created; no other Site or container is changed."
        ));
    } else {
        lines.push(format!("then: {}", deploy::health_text(manifest)));
    }
    if source.is_api() {
        lines.push("API: reuse the configured desktop certificate and credentials; initialize /home/app/homestack, verify authenticated HTTPS, remove TLS bootstrap values, reboot and verify stored configuration, PVE access and the guest home mount".to_owned());
    }
    vec![("Deploy".to_owned(), lines)]
}
