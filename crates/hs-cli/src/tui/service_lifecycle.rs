//! Common-managed service lifecycle inside the existing review/apply screen.
use std::collections::BTreeMap;
use std::sync::mpsc::Receiver;

use hs_deploy::config::DeployConfig;
use hs_install::Root;

use super::install::{Event, Found, Mark, Method, Setting};
use super::installing::{Command, Setup};
use crate::install_flow::Answer;
use crate::services::{deploy, lifecycle, registry};
use lifecycle::{Action, Choice};

const ACTIONS: &[Action] = &[
    Action::Recovery,
    Action::Resize,
    Action::Uninstall,
    Action::Reconnect,
    Action::DeleteData,
];
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
fn record(setup: &Setup, source: &deploy::Source) -> Result<registry::Deployment, String> {
    registry::load(&registry::path_for(&setup.client_path))?
        .deployment(&source.repository, &source.manifest.id)
        .cloned()
        .ok_or("Service is no longer registered".to_owned())
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
    let mut reviewed: Option<(lifecycle::Plan, String)> = None;
    while let Ok(command) = commands.recv() {
        match command {
            Command::Cancel => return,
            Command::Connect { .. } | Command::Rescan { .. } => {
                reviewed = None;
                tell(Event::Busy(
                    "Reading registered instances and published service releases".to_owned(),
                ));
                let opened = (|| {
                    let record = record(setup, source)?;
                    let releases = setup
                        .handle
                        .block_on(hs_core::registry::recent(&source.image("latest"), 50, &[]));
                    let mut versions = Vec::new();
                    let mut check = vec![(
                        Mark::Ok,
                        "identity".to_owned(),
                        format!("{} / {}", source.repository, source.manifest.id),
                    )];
                    match releases {
                        Ok(releases) => {
                            versions = releases
                                .into_iter()
                                .map(|release| {
                                    (
                                        release.image.rsplit_once(':').map_or("", |(_, v)| v).to_owned(),
                                        release
                                            .created
                                            .as_deref()
                                            .and_then(|at| super::reinstall::local_time(at, setup.offset))
                                            .unwrap_or_else(|| "—".to_owned()),
                                    )
                                })
                                .collect()
                        }
                        Err(error) => check.push((Mark::Note, "releases".to_owned(), error)),
                    }
                    settings = vec![
                        setting(
                            "Operation",
                            "For image updates, select the container in the main list and press Enter. These maintenance operations require a separate review.",
                            ACTIONS.iter().map(|a| (a.label().to_owned(), String::new())).collect(),
                            false,
                            None,
                        ),
                        setting(
                            "Instances",
                            "Select the exact instances to affect; nothing selects all nodes automatically. Uninstall retains home. Delete data is a separate permanent action for retained homes with absent containers.",
                            record
                                .instances
                                .iter()
                                .filter(|i| i.stage != registry::Stage::Removed)
                                .map(|i| {
                                    (
                                        i.ctid.to_string(),
                                        format!("{} · {} · {} · {:?}", i.node, i.hostname, i.address, i.stage),
                                    )
                                })
                                .collect(),
                            true,
                            None,
                        ),
                        setting(
                            "Version",
                            "Reconnect only: a published numeric SemVer image of this service.",
                            versions,
                            false,
                            None,
                        ),
                        setting(
                            "Root (GiB)",
                            "Resize only. Absolute target size; no shrinking or storage changes. Actual capacity must be known.",
                            Vec::new(),
                            false,
                            Some(String::new()),
                        ),
                        setting(
                            "Home (GiB)",
                            "Resize only. Absolute size of the existing /home/app volume. No replacement or formatting.",
                            Vec::new(),
                            false,
                            Some(String::new()),
                        ),
                        setting(
                            "Optional backup",
                            "Empty: replace only the root; no backup is requested and VM.Backup is not required. If you choose a backup storage, take a consistent stopped backup including home before replacement. A data-changing upgrade requires a verified backup.",
                            Vec::new(),
                            false,
                            Some(String::new()),
                        ),
                        setting(
                            "Storage maintenance SSH",
                            "Update and its recovery use only the Proxmox API. Other storage maintenance requires verified root on each chosen node through existing hardware-backed SSH: node=SSH-alias entries separated by spaces. No fallback keys or passwords.",
                            Vec::new(),
                            false,
                            Some(
                                record
                                    .instances
                                    .iter()
                                    .map(|i| format!("{}={}", i.node, i.node))
                                    .collect::<std::collections::BTreeSet<_>>()
                                    .into_iter()
                                    .collect::<Vec<_>>()
                                    .join(" "),
                            ),
                        ),
                        setting(
                            "Environment",
                            "Reconnect only: re-enter the application's required non-secret KEY=value settings. Values are not retained in the deployment registry. Secrets cannot be supplied here.",
                            Vec::new(),
                            false,
                            Some(String::new()),
                        ),
                        setting(
                            "Delete confirmation",
                            &format!(
                                "Delete data only: type DELETE {}/{}. This permanently removes only the selected, verified retained home volumes.",
                                source.repository, source.manifest.id
                            ),
                            Vec::new(),
                            false,
                            Some(String::new()),
                        ),
                    ];
                    settings[0].picked = vec![0];
                    Ok::<_, String>(Found {
                        root: "Common-managed service · deploy token".to_owned(),
                        check,
                        settings: settings.clone(),
                        domain: None,
                    })
                })();
                match opened {
                    Ok(found) => tell(Event::Connected(found)),
                    Err(error) => tell(Event::Failed(error)),
                }
            }
            Command::Review { answers, .. } => {
                reviewed = None;
                tell(Event::Busy(
                    "Reconciling the selected identities and building the review plan".to_owned(),
                ));
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
                let operation = selected(0)
                    .first()
                    .and_then(|label| ACTIONS.iter().find(|a| a.label() == label))
                    .copied();
                let choice = Choice {
                    action: operation,
                    ctids: selected(1).iter().filter_map(|id| id.parse().ok()).collect(),
                    version: selected(2).first().cloned().unwrap_or_default(),
                    root_gib: text(3),
                    home_gib: text(4),
                    backup_storage: text(5),
                    environment: text(7),
                    confirmation: text(8),
                };
                match record(setup, source) {
                    Ok(record) => {
                        let mut plan = setup
                            .handle
                            .block_on(lifecycle::plan(&pve, &config, source, &record, &choice));
                        let targets = text(6);
                        if plan.replacement.is_none()
                            && matches!(
                                operation,
                                Some(Action::Resize | Action::Uninstall | Action::Reconnect | Action::DeleteData)
                            )
                        {
                            match ssh_targets(&targets) {
                                Ok(targets) => {
                                    for o in &plan.observed {
                                        if !targets.contains_key(&o.instance.node) {
                                            plan.problems
                                                .push(format!("Give verified root SSH access for {}", o.instance.node));
                                        }
                                    }
                                }
                                Err(error) => plan.problems.push(error),
                            }
                        }
                        tell(Event::Reviewed {
                            sections: vec![(
                                operation.unwrap_or(Action::Recovery).label().to_owned(),
                                plan.lines.clone(),
                            )],
                            problems: plan.problems.clone(),
                        });
                        reviewed = Some((plan, targets));
                    }
                    Err(error) => tell(Event::Failed(error)),
                }
            }
            Command::Apply => {
                let Some((plan, targets)) = reviewed.take() else {
                    continue;
                };
                let result = (|| {
                    if !plan.problems.is_empty() {
                        return Err("Plan has unresolved problems".to_owned());
                    }
                    let mut roots: BTreeMap<String, Box<dyn Root>> = BTreeMap::new();
                    if plan.replacement.is_none()
                        && matches!(
                            plan.journal.action,
                            Action::Resize
                                | Action::Uninstall
                                | Action::Reconnect
                                | Action::DeleteData
                                | Action::Recovery
                        )
                    {
                        let targets = ssh_targets(&targets)?;
                        for observed in &plan.observed {
                            let node = &observed.instance.node;
                            if !roots.contains_key(node) {
                                tell(Event::Busy(format!("Verifying root SSH access on {node}")));
                                let root = setup
                                    .open(Method::Ssh, targets.get(node).ok_or("root target missing")?)
                                    .map_err(|e| e.to_string())?;
                                roots.insert(node.clone(), root);
                            }
                        }
                    }
                    let say = |line: &str| tell(Event::Line(line.to_owned()));
                    setup.handle.block_on(lifecycle::apply(
                        &pve,
                        &registry::path_for(&setup.client_path),
                        &config,
                        source,
                        &plan,
                        &mut roots,
                        &say,
                    ))
                })();
                match result {
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
fn ssh_targets(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut targets = BTreeMap::new();
    for entry in text.split_whitespace() {
        let (node, target) = entry.split_once('=').ok_or("Root SSH entries must be node=SSH-alias")?;
        if node.is_empty() || !node.bytes().all(|c| c.is_ascii_alphanumeric() || b"-._".contains(&c)) {
            return Err("Invalid root node name".to_owned());
        }
        crate::root_ssh::target(target).map_err(|e| e.to_string())?;
        if targets.insert(node.to_owned(), target.to_owned()).is_some() {
            return Err(format!("Duplicate SSH target for {node}"));
        }
    }
    Ok(targets)
}
