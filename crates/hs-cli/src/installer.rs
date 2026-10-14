//! `homestack install` and `homestack uninstall` (phase 13c): root on one
//! node is reached once, over SSH or through a Herdr pane; the access is
//! checked (root, Proxmox VE, a single node or a cluster); the plan is shown
//! and asked about; then it is applied, and the secrets of new tokens are
//! written to files on this desktop. Nothing else of the product uses root.

use hs_install::{Access, Command as Install, InstallError, Plan, Root, Settings};
use serde_json::{Value, json};

use crate::app::Context;
use crate::args::{Args, InstallArgs};
use crate::root_herdr::HerdrRoot;
use crate::root_ssh::SshRoot;
use crate::touch::Sounds;
use crate::view::Color;
use crate::wizard::{Answer, Item, Outcome, Step, Summary, Wizard};
use crate::{AppError, config, expand_home};

const CONFIRM_TTY: &str = "Interactive confirmation requires a TTY; use --yes for automation";
const CANCELLED: &str = "Cancelled. No changes were made.";

fn failure(error: InstallError) -> AppError {
    AppError::new(error.to_string())
}

/// The cue settings of `client.toml` when there is one; the installer runs
/// before the configuration exists.
fn sounds(ctx: &Context, args: &Args) -> Sounds {
    let path = args
        .config
        .clone()
        .unwrap_or_else(|| config::default_path(&ctx.home, ctx.config_home.as_deref()));
    config::load(&expand_home(&path.display().to_string(), &ctx.home))
        .map(|loaded| Sounds::from_table(loaded.client.sounds.as_ref()))
        .unwrap_or_default()
}

fn open_root(args: &Args, install: &InstallArgs, ctx: &mut Context) -> Result<Box<dyn Root>, AppError> {
    if let Some(name) = &install.herdr {
        ctx.console
            .err(&format!("Looking for the Herdr tab or pane {name:?}...\n"));
        return Ok(Box::new(HerdrRoot::open(ctx.system.clone(), name)?));
    }
    let target = install.ssh.clone().unwrap_or_default();
    ctx.console.err(&format!(
        "Opening root SSH to {target} (touch the security key if it blinks)...\n"
    ));
    let sounds = sounds(ctx, args);
    Ok(Box::new(SshRoot::open(
        ctx.system.clone(),
        &ctx.home,
        &target,
        install.identity.as_deref(),
        &sounds,
    )?))
}

fn settings(ctx: &Context, args: &Args, install: &InstallArgs) -> Settings {
    let token_dir = install.token_dir.clone().unwrap_or_else(|| {
        let file = args
            .config
            .clone()
            .unwrap_or_else(|| config::default_path(&ctx.home, ctx.config_home.as_deref()));
        file.parent()
            .map_or_else(|| ctx.home.clone(), std::path::Path::to_path_buf)
    });
    Settings {
        test: install.test,
        storages: install.storages.clone(),
        snippet_storage: install.snippet_storage.clone(),
        service_storages: install.service_storages.clone(),
        template_storage: install.template_storage.clone(),
        gold: install.gold,
        force: install.force,
        token_dir: expand_home(&token_dir.display().to_string(), &ctx.home),
    }
}

fn plan_json(plan: &Plan) -> Value {
    json!({
        "command": plan.command.name(),
        "part": plan.part,
        "host": plan.host,
        "version": plan.version,
        "topology": {
            "cluster": plan.topology.cluster,
            "quorate": plan.topology.quorate,
            "nodes": plan.topology.nodes.iter().map(|node| json!({
                "name": node.name, "ip": node.ip, "online": node.online, "local": node.local,
            })).collect::<Vec<_>>(),
        },
        "warnings": plan.warnings,
        "infos": plan.infos,
        "kept": plan.kept,
        "notes": plan.notes,
        "node_errors": plan.node_errors,
        "steps": plan.steps.iter().map(|step| json!({
            "description": step.description, "command": step.command_line(),
        })).collect::<Vec<_>>(),
    })
}

fn access_line(root: &dyn Root, access: &Access) -> String {
    format!(
        "Root access: {}; {} is Proxmox VE {}, {}.",
        root.describe(),
        access.host,
        access.version,
        access.topology.describe()
    )
}

fn emit(ctx: &mut Context, value: &Value) {
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    ctx.console.out(&format!("{text}\n"));
}

/// Ask "question [y/n] (n): " until the answer is yes or no.
fn confirm(ctx: &mut Context, question: &str) -> bool {
    loop {
        let Some(answer) = ctx.console.ask(&format!("{question} [y/n] (n): ")) else {
            return false;
        };
        match answer.trim().to_lowercase().as_str() {
            "" | "n" | "no" => return false,
            "y" | "yes" => return true,
            _ => ctx.console.out("Please enter Y or N\n"),
        }
    }
}

/// The command line that gives the same answers without being asked.
fn same_as(command: Install, install: &InstallArgs, settings: &Settings) -> String {
    let mut words = vec!["homestack".to_owned(), command.name().to_owned()];
    if let Some(ssh) = &install.ssh {
        words.extend(["--ssh".to_owned(), ssh.clone()]);
    }
    if let Some(herdr) = &install.herdr {
        words.extend(["--herdr".to_owned(), herdr.clone()]);
    }
    if let Some(identity) = &install.identity {
        words.extend(["--identity".to_owned(), identity.clone()]);
    }
    if settings.test {
        words.push("--test".to_owned());
    }
    if !settings.storages.is_empty() {
        words.extend(["--storages".to_owned(), settings.storages.join(",")]);
    }
    if let Some(id) = &settings.snippet_storage {
        words.extend(["--snippet-storage".to_owned(), id.clone()]);
    }
    if !settings.service_storages.is_empty() {
        words.extend(["--service-storages".to_owned(), settings.service_storages.join(",")]);
    }
    if let Some(id) = &settings.template_storage {
        words.extend(["--template-storage".to_owned(), id.clone()]);
    }
    if let Some(vmid) = settings.gold {
        words.extend(["--gold".to_owned(), vmid.to_string()]);
    }
    if install.force {
        words.push("--force".to_owned());
    }
    if let Some(dir) = &install.token_dir {
        words.extend(["--token-dir".to_owned(), dir.display().to_string()]);
    }
    words.join(" ")
}

/// The steps of the wizard for the options not given.
fn steps_of(questions: &[hs_install::Question]) -> Vec<Step> {
    questions
        .iter()
        .map(|question| {
            let items = question
                .choices
                .iter()
                .map(|choice| Item::new(&choice.value, &choice.detail))
                .collect();
            Step::pick(
                &question.title,
                &question.note,
                question.multiple,
                items,
                &question.defaults,
            )
        })
        .collect()
}

/// `settings` with the answers to `questions` put in.
fn answered(settings: &Settings, questions: &[hs_install::Question], answers: &[Answer]) -> Settings {
    let mut settings = settings.clone();
    for (question, answer) in questions.iter().zip(answers) {
        let Answer::Picked(picked) = answer else { continue };
        let values: Vec<String> = picked
            .iter()
            .filter_map(|index| question.choices.get(*index))
            .map(|choice| choice.value.clone())
            .collect();
        match question.option {
            "--storages" => settings.storages = values,
            "--snippet-storage" => settings.snippet_storage = values.into_iter().next(),
            "--service-storages" => settings.service_storages = values,
            "--template-storage" => settings.template_storage = values.into_iter().next(),
            "--gold" => settings.gold = values.first().and_then(|vmid| vmid.parse().ok()),
            _ => {}
        }
    }
    settings
}

/// The summary of a plan: where root comes from, the choices, warnings and
/// every change.
fn summary_of(access_line: &str, plan: &Plan) -> Summary {
    let settings = &plan.settings;
    let mut chosen = vec![
        format!("Workspace disks:     {}", settings.storages.join(", ")),
        format!(
            "Cloud-Init snippets: {}",
            settings.snippet_storage.clone().unwrap_or_default()
        ),
        format!("Server disks:        {}", settings.service_storages.join(", ")),
        format!(
            "Server image:        {}",
            settings.template_storage.clone().unwrap_or_default()
        ),
    ];
    if let Some(gold) = &plan.gold {
        chosen.push(format!("Gold:                {gold}"));
    }
    let mut sections = vec![
        ("Root access".to_owned(), vec![access_line.to_owned()]),
        ("Your choices".to_owned(), chosen),
    ];
    if !plan.taken.is_empty() {
        sections.push(("Chosen without asking".to_owned(), plan.taken.clone()));
    }
    let mut notices = plan.warnings.clone();
    notices.extend(plan.node_errors.iter().cloned());
    if !notices.is_empty() {
        sections.push(("Warnings (they do not stop the install)".to_owned(), notices));
    }
    let changes = if plan.steps.is_empty() {
        vec!["Nothing to change: HomeStack is installed as chosen.".to_owned()]
    } else {
        plan.steps.iter().map(|step| step.description.clone()).collect()
    };
    sections.push((format!("Changes in Proxmox ({})", plan.steps.len()), changes));
    Summary {
        sections,
        problems: Vec::new(),
    }
}

/// Run `install` or `uninstall`; the exit code.
pub fn run(args: &Args, given: &InstallArgs, command: Install, ctx: &mut Context) -> Result<i32, AppError> {
    let mut install = given.clone();
    // The default is the interactive installer: what is not given is asked,
    // and the last screen confirms. Giving every option is the manual way.
    let interactive = ctx.stdin_tty
        && !args.json
        && !args.yes
        && command == Install::Install
        && (install.ssh.is_none() && install.herdr.is_none()
            || install.storages.is_empty()
            || install.snippet_storage.is_none()
            || install.service_storages.is_empty()
            || install.template_storage.is_none());
    if install.ssh.is_none() && install.herdr.is_none() {
        if !ctx.stdin_tty {
            return Err(AppError::new(
                "Root on a node is needed: give --ssh USER@HOST or --herdr NAME, or run this on a terminal to be asked",
            ));
        }
        if !ask_access(ctx, &mut install)? {
            return cancelled(ctx);
        }
    }
    let install = &install;
    let mut root = open_root(args, install, ctx)?;
    let access = hs_install::probe(root.as_mut()).map_err(failure)?;
    let access_line = access_line(root.as_ref(), &access);
    let settings = settings(ctx, args, install);
    let first = hs_install::make_plan(root.as_mut(), &access, command, &settings);

    let plan = if interactive {
        let questions = match &first {
            Err(InstallError::Missing(questions)) => questions.clone(),
            Ok(_) => Vec::new(),
            Err(error) => return Err(failure(error.clone())),
        };
        let mut wizard = Wizard::new(steps_of(&questions), true);
        let mut last: Option<Plan> = None;
        let mut summarize = |answers: &[Answer]| {
            let chosen = answered(&settings, &questions, answers);
            match hs_install::make_plan(root.as_mut(), &access, command, &chosen) {
                Ok(plan) => {
                    let summary = summary_of(&access_line, &plan);
                    last = Some(plan);
                    summary
                }
                Err(error) => {
                    last = None;
                    Summary {
                        sections: Vec::new(),
                        problems: error.to_string().lines().map(str::to_owned).collect(),
                    }
                }
            }
        };
        let outcome = ctx.console.wizard(&mut wizard, &mut summarize);
        match (outcome, last) {
            (Outcome::Done(_), Some(plan)) => {
                let line = same_as(command, install, &plan.settings);
                ctx.console.out(&format!("Same as: {line}\n"));
                plan
            }
            _ => return cancelled(ctx),
        }
    } else {
        match first {
            Ok(plan) => plan,
            // Options the user has not given are not an error: show what
            // can be chosen, one choice a line.
            Err(InstallError::Missing(questions)) => {
                if args.json {
                    let missing: Vec<Value> = questions
                        .iter()
                        .map(|question| {
                            json!({
                                "option": question.option,
                                "title": question.title,
                                "note": question.note,
                                "multiple": question.multiple,
                                "choices": question.choices.iter().map(|choice| json!({
                                    "value": choice.value, "detail": choice.detail,
                                })).collect::<Vec<_>>(),
                                "defaults": question.defaults.iter()
                                    .filter_map(|i| question.choices.get(*i))
                                    .map(|choice| choice.value.clone())
                                    .collect::<Vec<_>>(),
                            })
                        })
                        .collect();
                    emit(
                        ctx,
                        &json!({"ok": false, "command": command.name(), "missing": missing,
                                "error": hs_install::describe_missing(&questions)}),
                    );
                } else {
                    ctx.console
                        .out(&format!("{}\n", hs_install::describe_missing(&questions)));
                }
                return Ok(1);
            }
            Err(error) => return Err(failure(error)),
        }
    };

    let steps = plan.steps.len();
    if !interactive && steps > 0 && !args.yes {
        if args.json {
            emit(
                ctx,
                &json!({
                    "ok": false,
                    "confirmation_required": true,
                    "message": format!("Apply these {steps} changes?"),
                    "access": access_line,
                    "plan": plan_json(&plan),
                }),
            );
            return Ok(3);
        }
        ctx.console.out(&format!("{access_line}\n{}", plan.text()));
        if !ctx.stdin_tty {
            return Err(AppError::new(CONFIRM_TTY));
        }
        if !confirm(ctx, &format!("Apply these {steps} changes?")) {
            return cancelled(ctx);
        }
    } else if !interactive && !args.json {
        ctx.console.out(&format!("{access_line}\n{}", plan.text()));
    }
    finish(ctx, args, root.as_mut(), &plan, command, &access_line)
}

fn cancelled(ctx: &mut Context) -> Result<i32, AppError> {
    let text = ctx.style.paint(CANCELLED, Color::Bold);
    ctx.console.out(&format!("{text}\n"));
    Ok(0)
}

/// Apply the plan and report.
fn finish(
    ctx: &mut Context,
    args: &Args,
    root: &mut dyn Root,
    plan: &Plan,
    command: Install,
    access_line: &str,
) -> Result<i32, AppError> {
    let mut lines: Vec<String> = Vec::new();
    let applied = if plan.steps.is_empty() {
        if !args.json {
            ctx.console.out("Nothing to change.\n");
        }
        None
    } else {
        let mut progress = |line: &str| {
            if !args.json {
                lines.push(format!("{line}\n"));
            }
        };
        let result = hs_install::apply(root, plan, &plan.settings, &mut progress);
        for line in lines.drain(..) {
            ctx.console.out(&line);
        }
        Some(result.map_err(failure)?)
    };
    let token_files: Vec<String> = applied
        .as_ref()
        .map(|done| done.token_files.iter().map(|file| file.display().to_string()).collect())
        .unwrap_or_default();
    let code = i32::from(!plan.node_errors.is_empty());
    if args.json {
        emit(
            ctx,
            &json!({
                "ok": code == 0,
                "command": command.name(),
                "access": access_line,
                "applied_steps": applied.as_ref().map_or(0, |done| done.steps_done),
                "token_files": token_files,
                "plan": plan_json(plan),
            }),
        );
    } else {
        for file in &token_files {
            ctx.console
                .out(&format!("Token secret written to {file} (mode 0600).\n"));
        }
        for error in &plan.node_errors {
            ctx.console.out(&format!("error: {error}\n"));
        }
    }
    Ok(code)
}

/// Ask how to reach root and which node or tab: `false` when the user gives
/// up. The answer goes into `install`.
fn ask_access(ctx: &mut Context, install: &mut InstallArgs) -> Result<bool, AppError> {
    let mut nothing = |_: &[Answer]| Summary::default();
    loop {
        let mut way = Wizard::new(
            vec![Step::pick(
                "How to reach root on a Proxmox node",
                "Root on one node is needed once, only to install or remove HomeStack in Proxmox; afterwards \
everything goes through the API, and you can drop the key or close the tab.",
                false,
                vec![
                    Item::new(
                        "SSH",
                        "log in to a node as root over SSH (touch the security key when it blinks)",
                    ),
                    Item::new("Herdr", "use a Herdr tab in which you are already root on a node"),
                ],
                &[0],
            )],
            false,
        );
        let Outcome::Done(answers) = ctx.console.wizard(&mut way, &mut nothing) else {
            return Ok(false);
        };
        let ssh = answers.first() == Some(&Answer::Picked(vec![0]));
        let step = if ssh {
            Step::text(
                "Node to log in to",
                "The Proxmox node as USER@HOST, for example root@pve1.",
                "root@",
            )
        } else {
            let tabs = crate::root_herdr::tabs(ctx.system.as_ref())?;
            if tabs.is_empty() {
                return Err(AppError::new(
                    "Herdr has no tab with a single pane: open a tab, log in as root on the node there, and run this again",
                ));
            }
            let items = tabs
                .iter()
                .map(|tab| {
                    let title = if tab.title.is_empty() {
                        "(no title)"
                    } else {
                        tab.title.as_str()
                    };
                    Item::new(&tab.name, title)
                })
                .collect();
            Step::pick(
                "Herdr tab",
                "The tab whose shell is root on the Proxmox node; it must be at its prompt.",
                false,
                items,
                &[],
            )
        };
        let mut which = Wizard::new(vec![step.clone()], false);
        let Outcome::Done(answers) = ctx.console.wizard(&mut which, &mut nothing) else {
            // Esc: back to the way.
            continue;
        };
        match answers.first() {
            Some(Answer::Text(target)) => install.ssh = Some(target.clone()),
            Some(Answer::Picked(picked)) => {
                let tabs = crate::root_herdr::tabs(ctx.system.as_ref())?;
                install.herdr = picked.first().and_then(|i| tabs.get(*i)).map(|tab| tab.name.clone());
            }
            None => return Ok(false),
        }
        return Ok(install.ssh.is_some() || install.herdr.is_some());
    }
}
