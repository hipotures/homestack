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
use crate::services::deploy::{self, Choice, Cluster, Plan, Source, TAG};
use crate::services::manifest::MOUNT;
use crate::services::registry::{self, Deployment, Stage};

const VERSION: usize = 0;
const NODES: usize = 1;
const ROOT: usize = 2;
const HOME: usize = 3;
const CTIDS: usize = 4;
const CORES: usize = 5;
const MEMORY: usize = 6;
const ROOT_GIB: usize = 7;
const HOME_GIB: usize = 8;
const ENVIRONMENT: usize = 9;

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
    if record.as_ref().is_some_and(|r| r.operation.is_none()) {
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
    let resumed = record.as_ref().map(resumed_choice);
    let settings = settings(source, &cluster, kept.or(resumed.as_ref()), setup.offset);
    let mut check = vec![
        (
            Mark::Ok,
            "access".to_owned(),
            format!("deploy token, pool {}", config.pool),
        ),
        (Mark::Ok, "nodes".to_owned(), cluster.nodes.join(", ")),
    ];
    if let Some(record) = &record {
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
        root_storages: record.instances.iter().map(|i| i.root_storage.clone()).collect(),
        home_storages: record.instances.iter().map(|i| i.home_storage.clone()).collect(),
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
    let nodes: Vec<(String, String)> = cluster.nodes.iter().map(|n| (n.clone(), String::new())).collect();
    let mut storage_ids: Vec<&str> = cluster.storages.iter().map(|s| s.storage.as_str()).collect();
    storage_ids.sort_unstable();
    storage_ids.dedup();
    let storages: Vec<(String, String)> = storage_ids
        .iter()
        .map(|id| {
            let free: Vec<String> = cluster
                .storages
                .iter()
                .filter(|s| s.storage == *id)
                .map(|s| format!("{} {} GiB free", s.node, gib(s.available)))
                .collect();
            ((*id).to_owned(), free.join(", "))
        })
        .collect();
    let storage_pick = |wanted: Option<&Vec<String>>| {
        wanted
            .map(|w| picked(&storages, w))
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| if storages.is_empty() { Vec::new() } else { vec![0] })
    };
    let text = |value: Option<&String>, default: &str| {
        Some(
            value
                .filter(|v| !v.is_empty())
                .cloned()
                .unwrap_or_else(|| default.to_owned()),
        )
    };
    let nodes_note = if manifest.multi_node {
        "The nodes that get an instance: one container per ticked node, each with its own home volume. Instances \
         are independent; nothing is replicated or balanced between them."
    } else {
        "The node that gets the service. Its manifest allows exactly one instance (multi_node: false); ticking \
         more is refused in the review."
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
            title: "Nodes".to_owned(),
            note: nodes_note.to_owned(),
            multiple: true,
            picked: kept.map(|k| picked(&nodes, &k.nodes)).unwrap_or_default(),
            choices: nodes,
            text: None,
        },
        Setting {
            title: "Container disk".to_owned(),
            note: "Where each container's disposable root disk is made; each node uses the first ticked storage it \
                   has. Only storages the deploy token may use are offered."
                .to_owned(),
            multiple: true,
            picked: storage_pick(kept.map(|k| &k.root_storages)),
            choices: storages.clone(),
            text: None,
        },
        Setting {
            title: "Home volume".to_owned(),
            note: format!(
                "Where each instance's own persistent volume is made, mounted at {MOUNT} before the first start; \
                 each node uses the first ticked storage it has."
            ),
            multiple: true,
            picked: storage_pick(kept.map(|k| &k.home_storages)),
            choices: storages,
            text: None,
        },
        Setting {
            title: "CTID".to_owned(),
            note: "One CTID per ticked node, in the order of the nodes, separated by commas. The address follows \
                   from it: 102240 is .102.240 in the servers' network."
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
    // The nodes in the list's order, as the CTIDs are given.
    let mut nodes = picked(NODES);
    if let Some(setting) = settings.get(NODES) {
        nodes.sort_by_key(|node| setting.choices.iter().position(|(value, _)| value == node));
    }
    Choice {
        version: picked(VERSION).into_iter().next().unwrap_or_default(),
        nodes,
        root_storages: picked(ROOT),
        home_storages: picked(HOME),
        ctids: text(CTIDS),
        cores: text(CORES),
        memory_mib: text(MEMORY),
        root_gib: text(ROOT_GIB),
        home_gib: text(HOME_GIB),
        environment: text(ENVIRONMENT),
    }
}

/// The review.
fn sections(source: &Source, cluster: &Cluster, plan: &Plan, offset: i64) -> Vec<(String, Vec<String>)> {
    let manifest = &source.manifest;
    let mut sections = vec![(
        "Service".to_owned(),
        vec![
            format!("{} ({}) from {}", manifest.name, manifest.id, source.repository),
            format!(
                "{} at {} @ {} (blob {})",
                source.path,
                source.branch,
                source.commit.get(..12).unwrap_or(&source.commit),
                source.blob.get(..12).unwrap_or(&source.blob)
            ),
        ],
    )];
    let mut release = vec![plan.image.clone()];
    if let Some(resolved) = &plan.resolved {
        release.push(format!("digest {}", resolved.digest));
        if resolved.platform != resolved.digest {
            release.push(format!("linux/amd64 image {}", resolved.platform));
        }
        let made = resolved
            .created
            .as_deref()
            .or_else(|| {
                cluster
                    .releases
                    .iter()
                    .find(|r| r.image == plan.image)
                    .and_then(|r| r.created.as_deref())
            })
            .and_then(|at| local_time(at, offset));
        if let Some(made) = made {
            release.push(format!("made {made}"));
        }
    }
    for node in &plan.pulls {
        release.push(format!("pulled into {} on {node}", plan.template_storage));
    }
    sections.push(("Release".to_owned(), release));
    let mut instances = Vec::new();
    for i in &plan.instances {
        instances.push(format!(
            "{}  CT {}  {}  {}/{} via {}, bridge {}",
            i.node, i.ctid, i.hostname, i.address, plan.cidr, plan.gateway, plan.bridge
        ));
        instances.push(format!(
            "      root {} {} GiB · home {} {} GiB at {MOUNT} (mp0, its own volume) · {} core{} · {} MiB",
            i.root_storage,
            i.root_gib,
            i.home_storage,
            i.home_gib,
            i.cores,
            if i.cores == 1 { "" } else { "s" },
            i.memory_mib
        ));
        if plan.resumed.contains(&i.ctid) {
            instances.push(match i.stage {
                Stage::Ready => "      ready in the unfinished run: kept".to_owned(),
                _ => "      made by the unfinished run and still this service's: reused, then checked".to_owned(),
            });
        }
    }
    if !instances.is_empty() {
        sections.push((
            format!(
                "Containers (unprivileged, pool {}, tag {TAG}, MAC assigned by Proxmox)",
                plan.instances.first().map_or("", |i| i.pool.as_str())
            ),
            instances,
        ));
    }
    sections.push((
        "Environment (kept in the container configuration; no secrets)".to_owned(),
        if plan.environment.is_empty() {
            vec!["none; the manifest declares no variables, the application's documentation lists them".to_owned()]
        } else {
            plan.environment
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect()
        },
    ));
    sections.push((
        "Health".to_owned(),
        vec![
            deploy::health_text(manifest),
            "checked from this desktop for up to 180 s after the start, then again after one restart".to_owned(),
        ],
    ));
    sections.push((
        "Ownership and recovery".to_owned(),
        vec![
            "description: service, repository, manifest revision, image and digest".to_owned(),
            "services.toml records each instance before its container is made and after every step".to_owned(),
            "one node at a time; the first failure stops the run and nothing is removed".to_owned(),
            "deploying again resumes a stopped run; its containers are reused only when they are this service's"
                .to_owned(),
        ],
    ));
    sections.push((
        "Kept, never changed".to_owned(),
        vec![
            "every other container, VM and volume, the HomeStack API servers and the Site connectors".to_owned(),
            "no public exposure: Pangolin resources and DNS are not touched".to_owned(),
        ],
    ));
    sections
}
