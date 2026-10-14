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

/// Ask each question in turn and put the answers in `settings`; `false`
/// when the user gives up.
fn choose(ctx: &mut Context, questions: &[hs_install::Question], settings: &mut Settings) -> bool {
    for question in questions {
        let items: Vec<(String, String)> = question
            .choices
            .iter()
            .map(|choice| (choice.value.clone(), choice.detail.clone()))
            .collect();
        let title = format!("{}: {}", question.option, question.text);
        let Some(picked) = ctx.console.pick(&title, question.multiple, &items) else {
            return false;
        };
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
    true
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

/// Run `install` or `uninstall`; the exit code.
pub fn run(args: &Args, install: &InstallArgs, command: Install, ctx: &mut Context) -> Result<i32, AppError> {
    let mut root = open_root(args, install, ctx)?;
    let access = hs_install::probe(root.as_mut()).map_err(failure)?;
    let mut settings = settings(ctx, args, install);
    let mut chosen = false;
    let plan = loop {
        match hs_install::make_plan(root.as_mut(), &access, command, &settings) {
            Ok(plan) => break plan,
            // On a terminal the user chooses from lists.
            Err(InstallError::Missing(questions)) if ctx.stdin_tty && !args.json => {
                if !choose(ctx, &questions, &mut settings) {
                    let text = ctx.style.paint(CANCELLED, Color::Bold);
                    ctx.console.out(&format!("{text}\n"));
                    return Ok(0);
                }
                chosen = true;
            }
            // Options the user has not given are not an error: show what
            // can be chosen, one choice a line.
            Err(InstallError::Missing(questions)) => {
                if args.json {
                    let missing: Vec<Value> = questions
                        .iter()
                        .map(|question| {
                            json!({
                                "option": question.option,
                                "text": question.text,
                                "multiple": question.multiple,
                                "choices": question.choices.iter().map(|choice| json!({
                                    "value": choice.value, "detail": choice.detail,
                                })).collect::<Vec<_>>(),
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
    if chosen {
        let line = same_as(command, install, &settings);
        ctx.console.out(&format!("\nSame as: {line}\n"));
    }

    let access_line = access_line(root.as_ref(), &access);
    let steps = plan.steps.len();
    if steps > 0 && !args.yes {
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
            let text = ctx.style.paint(CANCELLED, Color::Bold);
            ctx.console.out(&format!("{text}\n"));
            return Ok(0);
        }
    } else if !args.json {
        ctx.console.out(&format!("{access_line}\n{}", plan.text()));
    }

    let mut lines: Vec<String> = Vec::new();
    let applied = if steps > 0 {
        let mut progress = |line: &str| {
            if !args.json {
                lines.push(format!("{line}\n"));
            }
        };
        let result = hs_install::apply(root.as_mut(), &plan, &settings, &mut progress);
        for line in lines.drain(..) {
            ctx.console.out(&line);
        }
        Some(result.map_err(failure)?)
    } else {
        None
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
                "plan": plan_json(&plan),
            }),
        );
    } else {
        for file in &token_files {
            ctx.console
                .out(&format!("Token secret written to {file} (mode 0600).\n"));
        }
    }
    Ok(code)
}
