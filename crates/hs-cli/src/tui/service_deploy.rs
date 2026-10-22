//! The worker of the install screen in Service mode: the deployment of a
//! manifest-defined service through the deploy token
//! (`crate::services::deploy`). It reads the cluster and the published
//! releases, plans the settings and applies the reviewed plan.

use std::sync::mpsc::Receiver;

use hs_deploy::config::DeployConfig;
use hs_pve::PveClient;

use super::install::{Event, Found, Mark, Setting};
use super::installing::{Command, Setup};
use super::reinstall::local_time;
use crate::install_flow::Answer;
use crate::services::deploy::{self, Choice, Cluster, Plan, Source};
use crate::services::manifest::MOUNT;
use crate::services::registry::{self, Deployment, Stage};

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
}

/// The registry's record of the service, when it has one.
fn record(setup: &Setup, source: &Source) -> Result<Option<Deployment>, String> {
    let registry = registry::load(&registry::path_for(&setup.client_path))?;
    Ok(registry.deployment(&source.repository, &source.manifest.id).cloned())
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
                match setup.handle.block_on(open(setup, &source, kept.as_ref())) {
                    Ok((opened, found)) => {
                        session = Some(opened);
                        tell(Event::Connected(found));
                    }
                    Err(error) => tell(Event::Failed(error)),
                }
            }
            Command::Review { answers, .. } => {
                let Some(s) = session.as_mut() else { continue };
                tell(Event::Busy(
                    "Checking the plan against the cluster and the registry".to_owned(),
                ));
                let choice = choice(&s.settings, &answers);
                let plan = match record(setup, &source) {
                    Ok(record) => setup.handle.block_on(deploy::plan(
                        &s.config,
                        &s.pve,
                        &source,
                        &s.cluster,
                        &choice,
                        record.as_ref(),
                    )),
                    Err(error) => Plan {
                        problems: vec![error],
                        ..Plan::default()
                    },
                };
                tell(Event::Reviewed {
                    sections: sections(&source, &s.cluster, &plan, setup.offset),
                    problems: plan.problems.clone(),
                });
                s.reviewed = Some(plan);
            }
            Command::Apply => {
                let Some(s) = session.as_mut() else { continue };
                let Some(plan) = s.reviewed.take() else { continue };
                let say = |line: &str| tell(Event::Line(line.to_owned()));
                let path = registry::path_for(&setup.client_path);
                let (ok, lines) = setup
                    .handle
                    .block_on(deploy::apply(&s.pve, &path, &source, &plan, &say));
                tell(Event::Done { ok, lines });
            }
            Command::Cancel => return,
        }
    }
}

/// The deploy token's client, the cluster, and the settings: those of
/// `kept` (a rescan), else the unfinished run's, else defaults.
async fn open(setup: &Setup, source: &Source, kept: Option<&Choice>) -> Result<(Session, Found), String> {
    let config = DeployConfig::load(&setup.token_dir.join("deploy.toml"))
        .map_err(|error| format!("{error}. Install HomeStack before deploying services."))?;
    let pve = hs_deploy::deploy::deploy_client(&config)?;
    let record = record(setup, source)?;
    if !source.manifest.multi_node && record.as_ref().is_some_and(|r| r.operation.is_none()) {
        return Err(format!(
            "{} is already deployed; updating or removing it is not available yet.",
            source.manifest.id
        ));
    }
    let cluster = deploy::survey(&config, &pve, source).await?;
    if cluster.releases.is_empty() {
        return Err(format!(
            "{} has no published release (a numeric X.Y.Z tag); dev images are not offered.",
            source.manifest.image_repository
        ));
    }
    let mut resumed = record.as_ref().filter(|r| r.operation.is_some()).map(resumed_choice);
    // A single instance gets its CTID filled in: the first free one.
    if kept.is_none() && resumed.is_none() && !source.manifest.multi_node {
        let ctid = deploy::automatic(&config, &pve, 1).await?;
        resumed = Some(Choice {
            ctids: ctid.iter().map(u32::to_string).collect(),
            ..Choice::default()
        });
    }
    let settings = settings(source, &cluster, kept.or(resumed.as_ref()), setup.offset);
    let mut check = vec![
        (
            Mark::Ok,
            "access".to_owned(),
            format!("deploy token, pool {}", config.pool),
        ),
        (Mark::Ok, "nodes".to_owned(), cluster.nodes.join(", ")),
    ];
    if let Some(record) = &record
        && record.operation.is_some()
    {
        check.push((
            Mark::Note,
            "unfinished".to_owned(),
            format!(
                "a deployment stopped at {}; its settings are kept",
                record.operation.as_ref().map_or("", |o| o.step.as_str())
            ),
        ));
    }
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
        },
        found,
    ))
}

/// The settings of an unfinished run.
fn resumed_choice(record: &Deployment) -> Choice {
    let first = record.instances.first();
    Choice {
        version: record.image_version.clone(),
        nodes: record.instances.iter().map(|i| i.node.clone()).collect(),
        root_storages: record
            .instances
            .iter()
            .map(|i| format!("{}:{}", i.node, i.root_storage))
            .collect(),
        home_storages: record
            .instances
            .iter()
            .map(|i| format!("{}:{}", i.node, i.home_storage))
            .collect(),
        ctids: record
            .instances
            .iter()
            .map(|i| i.ctid.to_string())
            .collect::<Vec<_>>()
            .join(","),
        cores: first.map_or_else(String::new, |i| i.cores.to_string()),
        memory_mib: first.map_or_else(String::new, |i| i.memory_mib.to_string()),
        root_gib: first.map_or_else(String::new, |i| i.root_gib.to_string()),
        home_gib: first.map_or_else(String::new, |i| i.home_gib.to_string()),
        environment: record
            .environment
            .iter()
            .map(|key| format!("{key}="))
            .collect::<Vec<_>>()
            .join(" "),
    }
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
    let (title, note) = if manifest.multi_node {
        (
            "Nodes",
            "Tick one storage on each node that gets an instance: its disk and its home volume are made there, \
             new volumes of the sizes below; nothing on the storage is changed. Instances are independent; nothing \
             is replicated or balanced between them.",
        )
    } else {
        (
            "Node",
            "The node and storage of the only instance (multi_node: false): its disk and its home volume are made \
             there, new volumes of the sizes below; nothing on the storage is changed.",
        )
    };
    vec![
        Setting {
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
            title: title.to_owned(),
            note: format!(
                "{note} Each instance is an unprivileged container in pool homestack-service with tag homestack-server, its \
                 health checked from this desktop after the start and again after one restart. One node at a time; \
                 the first failure stops the run and nothing is removed; deploying again resumes it. Other \
                 containers, the API servers, the Site connectors and Pangolin are not touched."
            ),
            multiple: manifest.multi_node,
            picked: kept.map(|k| picked(&placements, &k.root_storages)).unwrap_or_default(),
            choices: placements,
            text: None,
        },
        Setting {
            title: "CTID".to_owned(),
            note: "Empty: the first free CTIDs, shown in the review. Or one CTID per ticked node, in the order of \
                   the nodes, separated by commas. The address follows from it: 102240 is .102.240."
                .to_owned(),
            multiple: false,
            choices: Vec::new(),
            picked: Vec::new(),
            text: text(kept.map(|k| &k.ctids), ""),
        },
        Setting {
            title: "Cores".to_owned(),
            note: "CPU cores of each container.".to_owned(),
            multiple: false,
            choices: Vec::new(),
            picked: Vec::new(),
            text: text(kept.map(|k| &k.cores), "1"),
        },
        Setting {
            title: "Memory (MiB)".to_owned(),
            note: "Memory of each container, without swap.".to_owned(),
            multiple: false,
            choices: Vec::new(),
            picked: Vec::new(),
            text: text(kept.map(|k| &k.memory_mib), "512"),
        },
        Setting {
            title: "Disk (GiB)".to_owned(),
            note: "Size of each container's root disk; it is replaced with the image and holds no data.".to_owned(),
            multiple: false,
            choices: Vec::new(),
            picked: Vec::new(),
            text: text(kept.map(|k| &k.root_gib), "4"),
        },
        Setting {
            title: "Home (GiB)".to_owned(),
            note: format!("Size of each instance's home volume at {MOUNT}, where the service keeps its data."),
            multiple: false,
            choices: Vec::new(),
            picked: Vec::new(),
            text: text(kept.map(|k| &k.home_gib), "8"),
        },
        Setting {
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
    // `NODE:STORAGE` in the list's order (by node), as the CTIDs are given.
    let placements = picked(PLACEMENT);
    let mut nodes: Vec<String> = Vec::new();
    for placement in &placements {
        let node = placement.split_once(':').map_or(placement.as_str(), |(node, _)| node);
        if !nodes.iter().any(|known| known == node) {
            nodes.push(node.to_owned());
        }
    }
    Choice {
        version: picked(VERSION).into_iter().next().unwrap_or_default(),
        nodes,
        root_storages: placements.clone(),
        home_storages: placements,
        ctids: text(CTIDS),
        cores: text(CORES),
        memory_mib: text(MEMORY),
        root_gib: text(ROOT_GIB),
        home_gib: text(HOME_GIB),
        environment: text(ENVIRONMENT),
    }
}

/// The review: what is made, nothing else.
fn sections(source: &Source, cluster: &Cluster, plan: &Plan, offset: i64) -> Vec<(String, Vec<String>)> {
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
        let resumed = if plan.resumed.contains(&i.ctid) {
            match i.stage {
                Stage::Ready => " (already running, kept)",
                _ => " (made by the stopped run, reused)",
            }
        } else {
            ""
        };
        lines.push(format!(
            "{}  CT {}  {}  disk {} GiB + home {} GiB on {}  {} CPU  {} MiB{resumed}",
            i.node, i.ctid, i.address, i.root_gib, i.home_gib, i.root_storage, i.cores, i.memory_mib
        ));
    }
    for (key, value) in &plan.environment {
        lines.push(format!("{key}={value}"));
    }
    lines.push(format!("then: {}", deploy::health_text(manifest)));
    vec![("Deploy".to_owned(), lines)]
}
