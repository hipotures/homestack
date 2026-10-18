//! `homestack install` and `homestack uninstall` (phase 13c): root on one
//! node is reached once, over SSH or through a Herdr pane; the access is
//! checked (root, Proxmox VE, a single node or a cluster); the plan is shown
//! and asked about; then it is applied, and the secrets of new tokens are
//! written to files on this desktop. Nothing else of the product uses root.

use hs_install::{Command as Install, InstallError, Plan, Root, Settings};
use serde_json::{Value, json};

use crate::app::Context;
use crate::args::{Args, InstallArgs};
use crate::install_flow::{self, Session};
use crate::root_herdr::HerdrRoot;
use crate::root_ssh::SshRoot;
use crate::servers::{Files, ServerPlan};
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
        bridge: install.bridge.clone(),
        adopt: install.adopt.clone(),
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

/// The options `install` cannot go without, those not given: how root is
/// reached, the storages and the servers' domain. Their choices come from
/// the cluster, so they are listed once root is reached.
fn missing_options(command: Install, install: &InstallArgs, want_servers: bool) -> String {
    let mut lines = vec![(
        "--ssh USER@HOST or --herdr NAME",
        "how root on a Proxmox node is reached (once, for the install)",
    )];
    if command == Install::Install {
        let given = [
            (
                install.storages.is_empty(),
                "--storages IDS",
                "storages for the workspaces' disks",
            ),
            (
                install.snippet_storage.is_none(),
                "--snippet-storage ID",
                "storage for the Cloud-Init snippets",
            ),
            (
                install.service_storages.is_empty(),
                "--service-storages IDS",
                "storages for the servers' disks",
            ),
            (
                install.template_storage.is_none(),
                "--template-storage ID",
                "storage for the server image",
            ),
            (
                want_servers && install.server_domain.is_none(),
                "--server-domain DOMAIN or --no-servers",
                "the servers' names apiN.<domain>, or the Proxmox part only",
            ),
        ];
        lines.extend(
            given
                .iter()
                .filter(|(missing, ..)| *missing)
                .map(|(_, option, what)| (*option, *what)),
        );
    }
    let width = lines.iter().map(|(option, _)| option.len()).max().unwrap_or(0);
    let mut text = format!(
        "homestack {} needs more options; nothing was changed:\n",
        command.name()
    );
    for (option, what) in lines {
        text.push_str(&format!("  {option:<width$}  {what}\n"));
    }
    text.push_str("With --ssh or --herdr, the choices of the other options are read from the cluster and listed.");
    text
}

/// Run `install` or `uninstall`; the exit code.
pub async fn run(args: &Args, install: &InstallArgs, command: Install, ctx: &mut Context) -> Result<i32, AppError> {
    let want_servers = command == Install::Install && !install.test && !install.no_servers;
    if install.ssh.is_none() && install.herdr.is_none() {
        return Err(AppError::new(missing_options(command, install, want_servers)));
    }
    let root = open_root(args, install, ctx)?;
    let settings = settings(ctx, args, install);
    let files = Files {
        dir: settings.token_dir.clone(),
        home: ctx.home.clone(),
    };
    let mut session = Session::start(
        root,
        command,
        settings.clone(),
        want_servers,
        install.server_image.clone(),
        files.clone(),
        std::sync::Arc::clone(&ctx.servers),
    )?;
    let access_line = session.access_line.clone();

    let (plan, server_plan) = {
        let no_domain = want_servers && install.server_domain.is_none();
        let first = session.plan(&settings);
        let plan = match first {
            Ok(plan) => plan,
            // Options the user has not given are not an error: show what
            // can be chosen, one choice a line.
            Err(InstallError::Missing(questions)) => {
                if args.json {
                    let mut missing: Vec<Value> = questions
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
                    if no_domain {
                        missing.push(domain_missing_json());
                    }
                    emit(
                        ctx,
                        &json!({"ok": false, "command": command.name(), "missing": missing,
                                "error": hs_install::describe_missing(&questions)}),
                    );
                } else {
                    let mut text = hs_install::describe_missing(&questions);
                    if no_domain {
                        text.push_str(&format!("\n{DOMAIN_MISSING}\n"));
                    }
                    ctx.console.out(&format!("{text}\n"));
                }
                return Ok(1);
            }
            Err(error) => return Err(failure(error)),
        };
        if no_domain {
            if args.json {
                emit(
                    ctx,
                    &json!({"ok": false, "command": command.name(), "missing": [domain_missing_json()],
                            "error": DOMAIN_MISSING}),
                );
            } else {
                ctx.console.out(&format!("{DOMAIN_MISSING}\n"));
            }
            return Ok(1);
        }
        let domain = install.server_domain.clone().filter(|_| want_servers);
        let server_plan = domain.as_deref().map(|domain| session.server_plan(&plan, domain));
        if let Some(server_plan) = &server_plan
            && !server_plan.problems.is_empty()
        {
            let message = format!(
                "The servers cannot be deployed; nothing changed:\n{}",
                server_plan
                    .problems
                    .iter()
                    .map(|problem| format!("  - {problem}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            return Err(AppError::new(message));
        }
        (plan, server_plan)
    };

    let steps = plan.steps.len();
    let servers_text = server_plan.as_ref().map(|server_plan| {
        let mut text = String::from("Servers:\n");
        for line in &server_plan.lines {
            text.push_str(&format!("  {line}\n"));
        }
        for warning in &server_plan.warnings {
            text.push_str(&format!("  warning: {warning}\n"));
        }
        text.push_str("  Then: homestack status\n");
        text
    });
    let changes = steps + usize::from(server_plan.is_some());
    if changes > 0 && !args.yes {
        if args.json {
            emit(
                ctx,
                &json!({
                    "ok": false,
                    "confirmation_required": true,
                    "message": format!("Apply these {steps} changes?"),
                    "access": access_line,
                    "plan": plan_json(&plan),
                    "servers": server_plan.as_ref().map(|s| s.lines.clone()),
                }),
            );
            return Ok(3);
        }
        ctx.console.out(&format!(
            "{access_line}\n{}{}",
            plan.text(),
            servers_text.clone().unwrap_or_default()
        ));
        if !ctx.stdin_tty {
            return Err(AppError::new(CONFIRM_TTY));
        }
        let question = if server_plan.is_some() {
            format!("Apply these {steps} changes and deploy the servers?")
        } else {
            format!("Apply these {steps} changes?")
        };
        if !confirm(ctx, &question) {
            return cancelled(ctx);
        }
    } else if !args.json {
        ctx.console.out(&format!(
            "{access_line}\n{}{}",
            plan.text(),
            servers_text.unwrap_or_default()
        ));
    }
    let code = finish(ctx, args, &mut session, &plan, command, &access_line)?;
    let Some(server_plan) = server_plan else {
        return Ok(code);
    };
    if code != 0 {
        return Err(AppError::new(
            "The Proxmox part did not finish, so the servers were not deployed; run this again",
        ));
    }
    drop(session);
    let client_path = args
        .config
        .clone()
        .unwrap_or_else(|| config::default_path(&ctx.home, ctx.config_home.as_deref()));
    let client_path = expand_home(&client_path.display().to_string(), &ctx.home);
    deploy_servers(ctx, args, &files, &server_plan, &client_path).await
}

const DOMAIN_MISSING: &str = "--server-domain is required: the servers are named apiN.<domain> (one per node, \
each name resolving to its address); or --no-servers for the Proxmox part only. Nothing was changed.";

fn domain_missing_json() -> Value {
    json!({
        "option": "--server-domain",
        "title": "Domain of the servers (apiN.<domain>)",
        "note": "or --no-servers for the Proxmox part only",
        "multiple": false,
        "choices": [],
        "defaults": [],
    })
}

/// After the Proxmox part: the servers, `client.toml`, and `homestack
/// status`, whose exit code is the install's.
async fn deploy_servers(
    ctx: &mut Context,
    args: &Args,
    files: &Files,
    server_plan: &ServerPlan,
    client_path: &std::path::Path,
) -> Result<i32, AppError> {
    let say_out = |ctx: &mut Context, line: &str| {
        if !args.json {
            ctx.console.out(&format!("{line}\n"));
        }
    };
    say_out(ctx, "\nServers:");
    // The deployment says what it does as it goes.
    let (sender, receiver) = std::sync::mpsc::channel::<String>();
    let say = move |line: &str| {
        let _ = sender.send(line.to_owned());
    };
    let servers = std::sync::Arc::clone(&ctx.servers);
    let mut deploying = Box::pin(install_flow::deploy(
        files,
        server_plan,
        client_path,
        servers.as_ref(),
        &say,
    ));
    let result = loop {
        tokio::select! {
            result = &mut deploying => break result,
            () = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
        }
        while let Ok(line) = receiver.try_recv() {
            say_out(ctx, &format!("  {line}"));
        }
    };
    drop(deploying);
    while let Ok(line) = receiver.try_recv() {
        say_out(ctx, &format!("  {line}"));
    }
    result?;
    say_out(ctx, "\nThe last check: homestack status\n");
    match crate::app::status_of(client_path, ctx).await {
        Ok(0) => {
            say_out(ctx, "\nHomeStack is installed: the client reaches the servers.");
            Ok(0)
        }
        Ok(code) => Err(AppError::new(format!(
            "homestack status ended with {code}: the install is not finished"
        ))),
        Err(error) => Err(AppError::new(format!(
            "homestack status failed: {error}\nThe install is not finished: the client does not reach the servers."
        ))),
    }
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
    session: &mut Session,
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
        let result = session.apply(plan, &mut progress);
        for line in lines.drain(..) {
            ctx.console.out(&line);
        }
        result?
    };
    let token_files: Vec<String> = applied
        .as_ref()
        .map(|done| done.token_files.iter().map(|file| file.display().to_string()).collect())
        .unwrap_or_default();
    let code = i32::from(!plan.node_errors.is_empty());
    let client_path = install_flow::client_path(args.config.as_deref(), &ctx.home, ctx.config_home.as_deref());
    if let Some(archived) = install_flow::retire_client(plan, &client_path)?
        && !args.json
    {
        ctx.console.out(&format!(
            "Inactive client configuration kept as {}\n",
            archived.display()
        ));
    }
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
