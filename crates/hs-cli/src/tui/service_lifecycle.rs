//! Service maintenance inside the existing review/apply screen: growing a
//! disk or removing instances, through the deploy token only.
use std::sync::mpsc::Receiver;

use hs_deploy::config::DeployConfig;

use super::install::{Event, Found, Mark, Setting};
use super::installing::{Command, Setup};
use super::pangolin_sites::managed::Pangolin;
use super::service_pangolin;
use crate::install_flow::Answer;
use crate::services::deploy::SiteReady;
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
    let mut attachment: Option<service_pangolin::Plan> = None;
    let mut catalog = None;
    // A Site restart is verified by its own Site online, through this context.
    let mut pangolin: Option<Pangolin> = None;
    let mut shown = String::new();
    while let Ok(command) = commands.recv() {
        match command {
            Command::Cancel => return,
            Command::Connect { .. } | Command::Rescan { .. } => {
                reviewed = None;
                attachment = None;
                let kept = match &command {
                    Command::Rescan { answers, .. } => Some(answers),
                    _ => None,
                };
                let previous_settings = settings.clone();
                if kept.is_some_and(|answers| service_pangolin::requested(answers)) {
                    let loaded =
                        site_context(setup, &mut pangolin, tell).and_then(|p| service_pangolin::load(p, &setup.stop));
                    match loaded {
                        Ok(loaded) => catalog = Some(loaded),
                        Err(error) => {
                            tell(Event::Failed(error));
                            continue;
                        }
                    }
                } else {
                    catalog = None;
                }
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
                        "Root",
                        "The size of the root disk, a number with M or G (e.g. 2G). A larger size grows it, online; a disk never shrinks.",
                        Vec::new(),
                        false,
                        Some(only.map(|i| hs_deploy::node::show_size(i.root_mib)).unwrap_or_default()),
                    ),
                    setting(
                        "Home",
                        "The size of the home volume at /home/app, a number with M or G (e.g. 10M). A larger size grows it, online; its data \
                         stays; a volume never shrinks.",
                        Vec::new(),
                        false,
                        Some(only.map(|i| hs_deploy::node::show_size(i.home_mib)).unwrap_or_default()),
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
                settings.extend(service_pangolin::settings(source, catalog.as_ref()));
                if let Some(answers) = kept {
                    restore(&mut settings, &previous_settings, answers);
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
                attachment = None;
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
                    root_mib: text(1),
                    home_mib: text(2),
                    environment: text(3),
                    shown_environment: shown.clone(),
                    remove: text(4),
                    attach: service_pangolin::requested(&answers),
                };
                let mut plan =
                    setup
                        .handle
                        .block_on(lifecycle::plan(&pve, &source.repository, &source.manifest.id, &choice));
                let restart = lifecycle::restarts(&plan) && plan.targets.iter().any(|t| t.running);
                if source.is_site() && (restart || plan.remove) {
                    match site_context(setup, &mut pangolin, tell) {
                        Ok(site) => {
                            for target in plan.targets.iter().filter(|t| plan.remove || t.running) {
                                let checked = if plan.remove {
                                    site.removal(target.ctid)
                                } else {
                                    site.identity(target.ctid)
                                };
                                match checked {
                                    Ok(line) => plan.lines.push(line),
                                    Err(error) => plan.problems.push(error),
                                }
                            }
                        }
                        Err(error) => plan.problems.push(error),
                    }
                }
                let mut sections = vec![(
                    if plan.remove { "Remove" } else { "Changes" }.to_owned(),
                    plan.lines.clone(),
                )];
                if choice.attach && plan.problems.is_empty() {
                    if !plan.lines.is_empty() {
                        plan.problems
                            .push("Save other service changes separately before attaching to Pangolin".to_owned());
                    } else if catalog.is_none() {
                        tell(Event::Busy("Loading Pangolin resources and Sites".to_owned()));
                        let loaded = site_context(setup, &mut pangolin, tell)
                            .and_then(|p| service_pangolin::load(p, &setup.stop));
                        match loaded {
                            Ok(loaded) => {
                                reviewed = None;
                                catalog = Some(loaded);
                                let previous = settings.clone();
                                settings.truncate(service_pangolin::ACTION);
                                settings.extend(service_pangolin::settings(source, catalog.as_ref()));
                                restore(&mut settings, &previous, &answers);
                                tell(Event::Connected(Found {
                                    root: "Service · deploy token".to_owned(),
                                    check: vec![(
                                        Mark::Ok,
                                        "Pangolin".to_owned(),
                                        "Choose a resource, Site, port and protocol, then review".to_owned(),
                                    )],
                                    settings: settings.clone(),
                                    domain: None,
                                }));
                                continue;
                            }
                            Err(error) => plan.problems.push(error),
                        }
                    } else {
                        let checked = site_context(setup, &mut pangolin, tell).and_then(|p| {
                            service_pangolin::review(
                                p,
                                &setup.stop,
                                catalog.as_ref(),
                                &settings,
                                &answers,
                                &plan.targets,
                            )
                        });
                        match checked {
                            Ok(checked) => {
                                sections.push(("Pangolin attachment".to_owned(), checked.lines()));
                                attachment = Some(checked);
                            }
                            Err(error) => plan.problems.push(error),
                        }
                    }
                }
                tell(Event::Reviewed {
                    sections,
                    problems: plan.problems.clone(),
                });
                reviewed = Some(plan);
            }
            Command::Apply => {
                let Some(plan) = reviewed.take() else { continue };
                if !plan.problems.is_empty() {
                    attachment = None;
                    tell(Event::Done {
                        ok: false,
                        lines: vec!["Resolve the review problems first".to_owned()],
                    });
                    continue;
                }
                if let Some(attachment) = attachment.take() {
                    let result = match pangolin.as_ref() {
                        Some(pangolin) => {
                            setup
                                .handle
                                .block_on(service_pangolin::apply(&pve, pangolin, &setup.stop, &attachment))
                        }
                        None => Err("The Pangolin context is unavailable; review again".to_owned()),
                    };
                    tell(match result {
                        Ok(lines) => Event::Done { ok: true, lines },
                        Err(error) => Event::Done {
                            ok: false,
                            lines: vec![error],
                        },
                    });
                    continue;
                }
                let say = |line: &str| tell(Event::Line(line.to_owned()));
                match setup.handle.block_on(lifecycle::apply(
                    &pve,
                    source,
                    &plan,
                    pangolin.as_ref().map(|p| p as &dyn SiteReady),
                    &say,
                )) {
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

/// The verified Pangolin context, opened once and kept while its tunnel lives.
fn site_context<'a>(
    setup: &Setup,
    pangolin: &'a mut Option<Pangolin>,
    tell: &(dyn Fn(Event) + Send + Sync),
) -> Result<&'a Pangolin, String> {
    if pangolin.as_ref().is_none_or(|p| !p.alive()) {
        *pangolin = None;
        tell(Event::Busy(
            "Opening the Pangolin API tunnel (touch your key or give its PIN if it asks)".to_owned(),
        ));
        *pangolin = Some(Pangolin::open(
            &setup.token_dir,
            std::sync::Arc::clone(&setup.system),
            setup.ssh_prefix.clone(),
            setup.sounds.clone(),
            &setup.stop,
        )?);
    }
    pangolin
        .as_ref()
        .ok_or_else(|| "The Pangolin context is unavailable".to_owned())
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
