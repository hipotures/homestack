//! Service maintenance inside the existing review/apply screen: growing a
//! disk or removing instances, through the deploy token only.
use std::sync::mpsc::Receiver;

use hs_deploy::config::DeployConfig;

use super::install::{Event, Found, Mark, Setting};
use super::installing::{Command, Setup};
use crate::install_flow::Answer;
use crate::services::{deploy, instance, lifecycle};
use lifecycle::Choice;

fn setting(title: &str, note: &str, choices: Vec<(String, String)>, multiple: bool, value: Option<String>) -> Setting {
    Setting {
        list: false,
        title: title.to_owned(),
        note: note.to_owned(),
        choices,
        multiple,
        picked: Vec::new(),
        text: value,
    }
}

pub(super) fn work(
    setup: &Setup,
    source: &deploy::Source,
    commands: &Receiver<Command>,
    tell: &(dyn Fn(Event) + Send + Sync),
) {
    let config = match DeployConfig::load(&setup.token_dir.join("deploy.toml")) {
        Ok(config) => config,
        Err(error) => {
            tell(Event::Failed(error));
            return;
        }
    };
    let pve = match hs_deploy::deploy::deploy_client(&config) {
        Ok(pve) => pve,
        Err(error) => {
            tell(Event::Failed(error));
            return;
        }
    };
    let mut settings: Vec<Setting> = Vec::new();
    let mut reviewed: Option<lifecycle::Plan> = None;
    let mut shown = String::new();
    while let Ok(command) = commands.recv() {
        match command {
            Command::Cancel => return,
            Command::Connect { .. } | Command::Rescan { .. } => {
                reviewed = None;
                tell(Event::Busy("Reading the service's containers".to_owned()));
                let found = setup
                    .handle
                    .block_on(instance::of_service(&pve, &source.repository, &source.manifest.id));
                let instances = match found {
                    Ok(instances) => instances,
                    Err(error) => {
                        tell(Event::Failed(error));
                        continue;
                    }
                };
                // One instance: the settings show what it has now.
                let only = match instances.as_slice() {
                    [only] => Some(only),
                    _ => None,
                };
                shown = only.map(lifecycle::shown_environment).unwrap_or_default();
                settings = vec![
                    setting(
                        "Instances",
                        "The containers of this service in the cluster; select the ones to change. Change a \
                         setting below and the review shows what is done; nothing else changes. Image updates: \
                         Enter on the container in the main list; moving it: g there.",
                        instances
                            .iter()
                            .map(|i| {
                                (
                                    i.ctid.to_string(),
                                    format!(
                                        "{} · {} · {} · {}",
                                        i.node,
                                        i.hostname,
                                        i.address,
                                        if i.running { "running" } else { "stopped" }
                                    ),
                                )
                            })
                            .collect(),
                        true,
                        None,
                    ),
                    setting(
                        "Root (GiB)",
                        "The size of the root disk. A larger number grows it, online; a disk never shrinks.",
                        Vec::new(),
                        false,
                        Some(only.map(|i| i.root_gib.to_string()).unwrap_or_default()),
                    ),
                    setting(
                        "Home (GiB)",
                        "The size of the home volume at /home/app. A larger number grows it, online; its data \
                         stays; a volume never shrinks.",
                        Vec::new(),
                        false,
                        Some(only.map(|i| i.home_gib.to_string()).unwrap_or_default()),
                    ),
                    Setting {
                        list: true,
                        ..setting(
                            "Environment",
                            "The container's variables, one per row. In the box: - removes the highlighted one, + \
                             adds one: type KEY=value and Enter; to change a value, remove it and add it anew. No \
                             secrets: the configuration is readable in Proxmox. A running container restarts to \
                             read the change; its data in /home/app stays.",
                            shown
                                .split_whitespace()
                                .map(|entry| (entry.to_owned(), String::new()))
                                .collect(),
                            false,
                            None,
                        )
                    },
                    setting(
                        "Remove",
                        "Type the names of the selected containers, separated by spaces, to remove them together \
                         with their home volumes, for good. Empty: nothing is removed.",
                        Vec::new(),
                        false,
                        Some(String::new()),
                    ),
                ];
                if only.is_some() {
                    settings[0].picked = vec![0];
                }
                tell(Event::Connected(Found {
                    root: "Service · deploy token".to_owned(),
                    check: vec![(
                        Mark::Ok,
                        "identity".to_owned(),
                        format!(
                            "{} / {} · {} instance{}",
                            source.repository,
                            source.manifest.id,
                            instances.len(),
                            if instances.len() == 1 { "" } else { "s" }
                        ),
                    )],
                    settings: settings.clone(),
                    domain: None,
                }));
            }
            Command::Review { answers, .. } => {
                tell(Event::Busy("Reading the selected containers".to_owned()));
                let selected = |at: usize| -> Vec<String> {
                    match (settings.get(at), answers.get(at)) {
                        (Some(setting), Some(Answer::Picked(indices))) => indices
                            .iter()
                            .filter_map(|i| setting.choices.get(*i))
                            .map(|(v, _)| v.clone())
                            .collect(),
                        _ => Vec::new(),
                    }
                };
                let text = |at: usize| match answers.get(at) {
                    Some(Answer::Text(value)) => value.trim().to_owned(),
                    _ => String::new(),
                };
                let choice = Choice {
                    ctids: selected(0).iter().filter_map(|id| id.parse().ok()).collect(),
                    root_gib: text(1),
                    home_gib: text(2),
                    environment: text(3),
                    shown_environment: shown.clone(),
                    remove: text(4),
                };
                let plan =
                    setup
                        .handle
                        .block_on(lifecycle::plan(&pve, &source.repository, &source.manifest.id, &choice));
                tell(Event::Reviewed {
                    sections: vec![(
                        if plan.remove { "Remove" } else { "Changes" }.to_owned(),
                        plan.lines.clone(),
                    )],
                    problems: plan.problems.clone(),
                });
                reviewed = Some(plan);
            }
            Command::Apply => {
                let Some(plan) = reviewed.take() else { continue };
                let say = |line: &str| tell(Event::Line(line.to_owned()));
                match setup
                    .handle
                    .block_on(lifecycle::apply(&pve, &source.manifest, &plan, &say))
                {
                    Ok(lines) => tell(Event::Done { ok: true, lines }),
                    Err(error) => tell(Event::Done {
                        ok: false,
                        lines: vec![error],
                    }),
                }
            }
        }
    }
}
