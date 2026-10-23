//! The install worker of the TUI (phase 13b step 3): a thread that holds
//! root and walks an `install_flow::Session` as the screen asks, telling
//! what it finds and does as `install::Event`s. ssh's own questions go
//! through the askpass bridge, not through here.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;

use hs_install::{Command as Install, Root, Settings};

use super::install::{Event, Found, Mark, Method, Mode, Setting};
use crate::install_flow::{self, Answer, Review, Session, Summary};
use crate::root_herdr::HerdrRoot;
use crate::root_ssh::SshRoot;
use crate::servers::{Files, Servers};
use crate::system::System;
use crate::touch::Sounds;
use crate::{AppError, config};

/// What the screen asks the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Connect {
        method: Method,
        target: String,
    },
    /// Plan the answers (the uninstall: what would be removed).
    Review {
        answers: Vec<Answer>,
        domain: Option<String>,
    },
    /// Read the cluster's choices again (F5, rescan cluster), keeping these
    /// answers and this domain.
    Rescan {
        answers: Vec<Answer>,
        domain: Option<String>,
    },
    /// Plan removing the selected instances of a service (Del).
    Remove {
        answers: Vec<Answer>,
    },
    /// Apply the last review.
    Apply,
    /// Let root go; the worker ends.
    Cancel,
}

/// What the worker needs from the desktop.
pub struct Setup {
    pub mode: Mode,
    pub system: Arc<dyn System>,
    pub home: PathBuf,
    /// Where the token secrets and the server files go.
    pub token_dir: PathBuf,
    pub client_path: PathBuf,
    pub servers: Arc<dyn Servers>,
    pub sounds: Sounds,
    /// The askpass bridge's words before `ssh`; empty: ssh as from a terminal.
    pub ssh_prefix: Vec<String>,
    /// Set by Esc while connecting: ssh is stopped.
    pub stop: Arc<AtomicBool>,
    pub server_image: Option<String>,
    pub server_domain: Option<String>,
    /// The runtime the deployment and the last check run on.
    pub handle: tokio::runtime::Handle,
    /// Sites mode: the Pangolin wizard's tunnel, organization and key.
    pub sites: Option<super::pangolin_sites::Context>,
    /// Service mode: the manifest revision to deploy.
    pub service: Option<crate::services::deploy::Source>,
    /// Seconds east of UTC, for the times shown.
    pub offset: i64,
}

impl Setup {
    fn settings(&self) -> Settings {
        let snippet_storage = (self.mode == Mode::Uninstall)
            .then(|| install_flow::configured_snippet_storage(&self.token_dir))
            .flatten();
        Settings {
            test: false,
            storages: Vec::new(),
            snippet_storage,
            service_storages: Vec::new(),
            template_storage: None,
            gold: None,
            bridge: None,
            adopt: Vec::new(),
            token_dir: self.token_dir.clone(),
        }
    }

    fn command(&self) -> Install {
        match self.mode {
            Mode::Install | Mode::Sites | Mode::Service => Install::Install,
            Mode::Uninstall => Install::Uninstall,
        }
    }

    pub(super) fn open(&self, method: Method, target: &str) -> Result<Box<dyn Root>, AppError> {
        match method {
            Method::Herdr => Ok(Box::new(HerdrRoot::open(Arc::clone(&self.system), target)?)),
            Method::Ssh if self.ssh_prefix.is_empty() => Ok(Box::new(SshRoot::open(
                Arc::clone(&self.system),
                &self.home,
                target,
                None,
                &self.sounds,
            )?)),
            Method::Ssh => Ok(Box::new(SshRoot::open_asking(
                Arc::clone(&self.system),
                &self.home,
                target,
                None,
                &self.sounds,
                self.ssh_prefix.clone(),
                Arc::clone(&self.stop),
            )?)),
        }
    }
}

/// Start the worker; it ends on [`Command::Cancel`], when `commands` closes,
/// or after an uninstall.
pub fn spawn(setup: Setup, commands: Receiver<Command>, tell: impl Fn(Event) + Send + Sync + 'static) {
    std::thread::spawn(move || {
        // The servers' DNS lookups and the deployment use the runtime.
        let _entered = setup.handle.enter();
        match setup.mode {
            Mode::Sites => super::pangolin_sites::work(&setup, &commands, &tell),
            Mode::Service => super::service_deploy::work(&setup, &commands, &tell),
            Mode::Install | Mode::Uninstall => work(&setup, &commands, &tell),
        }
    });
}

/// A review made from these answers and this domain.
type Kept = (Vec<Answer>, Option<String>, Review);

fn work(setup: &Setup, commands: &Receiver<Command>, tell: &(dyn Fn(Event) + Send + Sync)) {
    let mut session: Option<Session> = None;
    let mut last: Option<Review> = None;
    // The check's plan: the review of unchanged settings needs no new read.
    let mut kept: Option<Kept> = None;
    while let Ok(command) = commands.recv() {
        match command {
            // Removing instances belongs to a service deployment.
            Command::Remove { .. } => {}
            Command::Connect { method, target } => match connect(setup, method, &target, tell) {
                Ok((started, found, first)) => {
                    session = Some(started);
                    kept = first;
                    tell(Event::Connected(found));
                }
                Err(error) => tell(Event::Failed(error.0)),
            },
            Command::Rescan { answers, domain } => {
                let Some(session) = session.as_mut() else { continue };
                tell(Event::Busy("Rescanning the cluster".to_owned()));
                match session.rescan(&answers, domain.as_deref()) {
                    Ok(()) => {
                        let (found, first) = survey(setup, session, tell);
                        kept = first;
                        tell(Event::Connected(found));
                    }
                    Err(error) => tell(Event::Failed(error.0)),
                }
            }
            Command::Review { answers, domain } => {
                let Some(session) = session.as_mut() else { continue };
                let reused = kept
                    .take()
                    .filter(|(old_answers, old_domain, _)| *old_answers == answers && *old_domain == domain);
                let review = match reused {
                    Some((_, _, review)) => {
                        session.keep(&answers, domain.as_deref());
                        review
                    }
                    None => {
                        tell(Event::Busy("Reading the cluster and planning".to_owned()));
                        review(setup, session, &answers, domain.as_deref())
                    }
                };
                tell(Event::Reviewed {
                    sections: review.summary.sections.clone(),
                    problems: review.summary.problems.clone(),
                });
                last = Some(review);
            }
            Command::Apply => {
                let (Some(session), Some(review)) = (session.as_mut(), last.as_ref()) else {
                    continue;
                };
                let (ok, lines) = apply(setup, session, review, tell);
                tell(Event::Done { ok, lines });
            }
            Command::Cancel => return,
        }
    }
}

fn connect(
    setup: &Setup,
    method: Method,
    target: &str,
    tell: &(dyn Fn(Event) + Send + Sync),
) -> Result<(Session, Found, Option<Kept>), AppError> {
    tell(Event::Busy(format!(
        "Connecting to {target} (touch your key or give its PIN if it asks)"
    )));
    let root = setup.open(method, target)?;
    tell(Event::Busy("Connected; reading the cluster".to_owned()));
    let mut session = Session::start(
        root,
        setup.command(),
        setup.settings(),
        setup.mode == Mode::Install,
        setup.server_image.clone(),
        Files {
            dir: setup.token_dir.clone(),
            home: setup.home.clone(),
        },
        Arc::clone(&setup.servers),
    )?;
    if let Some(domain) = &setup.server_domain {
        domain.clone_into(&mut session.draft_domain);
    }
    let (found, first) = survey(setup, &mut session, tell);
    Ok((session, found, first))
}

/// What the screen shows of a session: the check, the settings and the
/// domain, with the plan of the kept choices for the install.
fn survey(setup: &Setup, session: &mut Session, tell: &(dyn Fn(Event) + Send + Sync)) -> (Found, Option<Kept>) {
    let access = session.access.clone();
    let mut first = None;
    let describe = session.describe();
    let mut check = vec![
        (
            Mark::Ok,
            "root".to_owned(),
            format!("{describe} answers, Proxmox VE {}", access.version),
        ),
        (Mark::Ok, "nodes".to_owned(), access.topology.describe()),
    ];
    match setup.mode {
        Mode::Install => {
            let answers: Vec<Answer> = session.questions.iter().map(crate::install_config::defaults).collect();
            let domain = session.draft_domain.clone();
            tell(Event::Busy("Planning with the last choices".to_owned()));
            let review = session.review(&answers, Some(domain.as_str()));
            check.extend(install_check(&review));
            first = Some((answers, Some(domain), review));
        }
        Mode::Uninstall | Mode::Sites | Mode::Service => match session.plan(&setup.settings()) {
            Ok(plan) => {
                check.push(if plan.steps.is_empty() {
                    (
                        Mark::Ok,
                        "Proxmox".to_owned(),
                        "nothing of HomeStack to remove".to_owned(),
                    )
                } else {
                    (
                        Mark::Add,
                        "Proxmox".to_owned(),
                        format!("{} changes to remove it", plan.steps.len()),
                    )
                });
            }
            Err(error) => check.push((Mark::Note, "plan".to_owned(), error.to_string())),
        },
    }
    let settings = session
        .questions
        .iter()
        .map(|question| Setting {
            list: false,
            title: question.title.clone(),
            note: question.note.clone(),
            multiple: question.multiple,
            choices: question
                .choices
                .iter()
                .map(|choice| (choice.value.clone(), choice.detail.clone()))
                .collect(),
            picked: question.defaults.clone(),
            text: crate::install_config::is_text(question).then(|| {
                question
                    .choices
                    .first()
                    .map_or_else(String::new, |choice| choice.value.clone())
            }),
        })
        .collect();
    let found = Found {
        root: format!("{describe} · PVE {}", access.version),
        check,
        settings,
        domain: (setup.mode == Mode::Install).then(|| session.draft_domain.clone()),
    };
    (found, first)
}

/// The check's lines of a first plan made from the kept choices.
fn install_check(review: &Review) -> Vec<(Mark, String, String)> {
    let mut lines = Vec::new();
    if let Some(plan) = &review.plan {
        if let Some(gold) = &plan.gold {
            lines.push((Mark::Ok, "Gold".to_owned(), gold.clone()));
        }
        lines.push(if plan.steps.is_empty() {
            (
                Mark::Ok,
                "Proxmox".to_owned(),
                "user, roles, pools and tokens are in place as chosen".to_owned(),
            )
        } else {
            (
                Mark::Add,
                "Proxmox".to_owned(),
                format!("{} changes to make", plan.steps.len()),
            )
        });
        for warning in &plan.warnings {
            lines.push((Mark::Note, "warning".to_owned(), warning.clone()));
        }
    }
    // The servers are a part of their own: "installed" above is Proxmox's.
    if let Some(config) = review.servers.as_ref().and_then(|servers| servers.config.as_ref()) {
        let names: Vec<String> = config
            .instances
            .iter()
            .map(|instance| format!("{}.{}", instance.name, config.domain))
            .collect();
        lines.push((
            Mark::Add,
            "servers".to_owned(),
            format!("{}: to deploy, in the last step after the review", names.join(", ")),
        ));
    }
    for problem in &review.summary.problems {
        lines.push((Mark::Note, "to fix".to_owned(), problem.clone()));
    }
    lines
}

fn review(setup: &Setup, session: &mut Session, answers: &[Answer], domain: Option<&str>) -> Review {
    if setup.mode == Mode::Install {
        let review = session.review(answers, domain);
        session.keep(answers, domain);
        return review;
    }
    match session.plan(&setup.settings()) {
        Ok(plan) => {
            let mut sections = vec![(
                "Removed from Proxmox".to_owned(),
                plan.steps
                    .iter()
                    .map(|step| step.description.clone())
                    .collect::<Vec<_>>(),
            )];
            let mut kept = plan.kept.clone();
            kept.push(
                "every VM and container with its tags (the next install puts them back in their pools), \
every disk and volume"
                    .to_owned(),
            );
            kept.push("~/.config/homestack on this desktop".to_owned());
            sections.push(("Kept, never removed".to_owned(), kept));
            if !plan.warnings.is_empty() {
                sections.push(("Warnings".to_owned(), plan.warnings.clone()));
            }
            Review {
                plan: Some(plan),
                servers: None,
                domain: None,
                summary: Summary {
                    sections,
                    problems: Vec::new(),
                },
            }
        }
        Err(error) => Review {
            plan: None,
            servers: None,
            domain: None,
            summary: Summary {
                sections: Vec::new(),
                problems: error.to_string().lines().map(str::to_owned).collect(),
            },
        },
    }
}

/// Apply `review`: Proxmox, then the servers and `client.toml`, then the
/// status through the new client. Whether it all went, and the end's lines.
fn apply(
    setup: &Setup,
    session: &mut Session,
    review: &Review,
    tell: &(dyn Fn(Event) + Send + Sync),
) -> (bool, Vec<String>) {
    let Some(plan) = &review.plan else {
        return (false, vec!["There is no plan to apply.".to_owned()]);
    };
    let mut progress = |line: &str| tell(Event::Line(line.to_owned()));
    let applied = match session.apply(plan, &mut progress) {
        Ok(applied) => applied,
        Err(error) => return (false, vec![error.0]),
    };
    let mut lines = Vec::new();
    lines.push(if plan.steps.is_empty() {
        "Proxmox: user, roles, pools and tokens were already in place as chosen".to_owned()
    } else {
        format!("Proxmox: {} changes made", plan.steps.len())
    });
    for file in applied.iter().flat_map(|done| done.token_files.iter()) {
        lines.push(format!("token secret written to {} (mode 0600)", file.display()));
    }
    for error in &plan.node_errors {
        lines.push(format!("error: {error}"));
    }
    if setup.mode == Mode::Uninstall {
        match install_flow::retire_client(plan, &setup.client_path) {
            Ok(Some(path)) => lines.push(format!("Inactive client configuration kept as {}", path.display())),
            Ok(None) => {}
            Err(error) => return (false, vec![error.0]),
        }
        lines.insert(0, "HomeStack is removed from Proxmox.".to_owned());
        return (plan.node_errors.is_empty(), lines);
    }
    let Some(server_plan) = &review.servers else {
        return (plan.node_errors.is_empty(), lines);
    };
    tell(Event::Line("Servers".to_owned()));
    let say = |line: &str| tell(Event::Line(line.to_owned()));
    let deployed = setup.handle.block_on(install_flow::deploy(
        &session.files,
        server_plan,
        &setup.client_path,
        session.servers().as_ref(),
        &say,
    ));
    if let Err(error) = deployed {
        lines.push(error.0);
        return (false, lines);
    }
    lines.push(format!(
        "Servers: {} deployed",
        server_plan.config.as_ref().map_or(0, |c| c.instances.len())
    ));
    tell(Event::Line(
        "The last check: the status through the new client".to_owned(),
    ));
    match setup.handle.block_on(status(&setup.client_path, &setup.home)) {
        Ok(summary) => {
            lines.push(format!("Check: {summary}"));
            (plan.node_errors.is_empty(), lines)
        }
        Err(error) => {
            lines.push(format!("The client does not reach the servers: {}", error.0));
            (false, lines)
        }
    }
}

/// `/v1/status` through the client of `client_path`, in a few words.
async fn status(client_path: &std::path::Path, home: &std::path::Path) -> Result<String, AppError> {
    let loaded = config::load(client_path)?;
    let client = hs_client::Client::new(loaded.client_config(home)?).map_err(crate::app::client_error)?;
    let status = client.get("/v1/status").await.map_err(crate::app::client_error)?;
    let nodes = status["summary"]["nodes"].as_array().map_or(0, Vec::len);
    let workspaces = status["summary"]["workspace_count"].as_u64().unwrap_or(0);
    Ok(format!(
        "the client reaches the servers: {nodes} nodes, {workspaces} workspaces"
    ))
}
