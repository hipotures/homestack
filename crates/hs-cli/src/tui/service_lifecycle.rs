//! Service maintenance inside the existing review/apply screen: growing a
//! disk or removing instances, through the deploy token only.
use std::sync::mpsc::Receiver;

use hs_deploy::config::DeployConfig;

use super::install::{Event, Found, Mark, Setting};
use super::installing::{Command, Setup};
use crate::install_flow::Answer;
use crate::services::{deploy, instance, lifecycle};
use lifecycle::{Action, Choice};

fn setting(title: &str, note: &str, choices: Vec<(String, String)>, multiple: bool, value: Option<String>) -> Setting {
    Setting {
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
                settings = vec![
                    setting(
                        "Operation",
                        "For image updates, select the container in the main list and press Enter; to move one, press g there.",
                        Action::ALL
                            .iter()
                            .map(|a| (a.label().to_owned(), String::new()))
                            .collect(),
                        false,
                        None,
                    ),
                    setting(
                        "Instances",
                        "The containers of this service in the cluster; select the ones to change.",
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
                        "Resize only: the new total size of the root disk; a disk only grows. Empty: unchanged.",
                        Vec::new(),
                        false,
                        Some(String::new()),
                    ),
                    setting(
                        "Home (GiB)",
                        "Resize only: the new total size of the home volume at /home/app. Empty: unchanged.",
                        Vec::new(),
                        false,
                        Some(String::new()),
                    ),
                    setting(
                        "Environment",
                        "Environment only: KEY=value entries separated by spaces change or add a variable; KEY= \
                         removes one; variables not named stay as they are. No secrets: the container \
                         configuration is readable in Proxmox. A running container restarts to take the change; \
                         its data in /home/app stays.",
                        Vec::new(),
                        false,
                        Some(match instances.as_slice() {
                            [only] => lifecycle::shown_environment(only),
                            _ => String::new(),
                        }),
                    ),
                    setting(
                        "Confirmation",
                        "Remove only: type the names of the selected containers, separated by spaces. The container \
                         and its home volume are deleted for good.",
                        Vec::new(),
                        false,
                        Some(String::new()),
                    ),
                ];
                settings[0].picked = vec![0];
                if instances.len() == 1 {
                    settings[1].picked = vec![0];
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
                let action = selected(0)
                    .first()
                    .and_then(|label| Action::ALL.iter().find(|a| a.label() == label))
                    .copied()
                    .unwrap_or(Action::Resize);
                let choice = Choice {
                    action,
                    ctids: selected(1).iter().filter_map(|id| id.parse().ok()).collect(),
                    root_gib: text(2),
                    home_gib: text(3),
                    environment: text(4),
                    confirmation: text(5),
                };
                let plan =
                    setup
                        .handle
                        .block_on(lifecycle::plan(&pve, &source.repository, &source.manifest.id, &choice));
                tell(Event::Reviewed {
                    sections: vec![(action.label().to_owned(), plan.lines.clone())],
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
