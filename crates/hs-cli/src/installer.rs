//! `homestack install` and `homestack uninstall` (phase 13c): root on one
//! node is reached once, over SSH or through a Herdr pane; the access is
//! checked (root, Proxmox VE, a single node or a cluster); the plan is shown
//! and asked about; then it is applied, and the secrets of new tokens are
//! written to files on this desktop. Nothing else of the product uses root.

use hs_install::{Access, Command as Install, InstallError, Plan, Root, Settings};
use serde_json::{Value, json};

use crate::app::Context;
use crate::args::{Args, InstallArgs};
use crate::form::{Field, Form, Outcome as FormOutcome};
use crate::root_herdr::HerdrRoot;
use crate::root_ssh::SshRoot;
use crate::servers::{Files, ServerPlan, ensure_api_token, plan_servers, write_private};
use crate::touch::Sounds;
use crate::view::Color;
use crate::wizard::{Answer, Item, Outcome, Step, Summary, Wizard};
use crate::{AppError, config, expand_home};

const CONFIRM_TTY: &str = "Interactive confirmation requires a TTY; use --yes for automation";
const CANCELLED: &str = "Cancelled. No changes were made.";
/// The draft's key of the server domain.
const DRAFT_DOMAIN: &str = "server-domain";

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
fn same_as(command: Install, install: &InstallArgs, settings: &Settings, domain: Option<&str>) -> String {
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
    match domain {
        Some(domain) => {
            words.extend(["--server-domain".to_owned(), domain.to_owned()]);
            if let Some(image) = &install.server_image {
                words.extend(["--server-image".to_owned(), image.clone()]);
            }
        }
        None if command == Install::Install && !settings.test => words.push("--no-servers".to_owned()),
        None => {}
    }
    if install.force {
        words.push("--force".to_owned());
    }
    if let Some(dir) = &install.token_dir {
        words.extend(["--token-dir".to_owned(), dir.display().to_string()]);
    }
    words.join(" ")
}

/// The rows of the installer's form, in a fixed order: how root is reached,
/// then every setting; a setting given on the command line is shown only.
fn form_fields(root_line: &str, settings: &Settings, questions: &[hs_install::Question]) -> Vec<Field> {
    let given = [
        ("--storages", "Workspace disks", settings.storages.join(", ")),
        (
            "--snippet-storage",
            "Cloud-Init snippets",
            settings.snippet_storage.clone().unwrap_or_default(),
        ),
        (
            "--service-storages",
            "Server disks",
            settings.service_storages.join(", "),
        ),
        (
            "--template-storage",
            "Server image",
            settings.template_storage.clone().unwrap_or_default(),
        ),
        (
            "--gold",
            "Gold VM",
            settings.gold.map(|vmid| vmid.to_string()).unwrap_or_default(),
        ),
    ];
    let mut fields = vec![Field::fixed("Root access", root_line)];
    for (option, label, value) in given {
        match questions.iter().find(|question| question.option == option) {
            Some(question) => {
                let items = question
                    .choices
                    .iter()
                    .map(|choice| Item::new(&choice.value, &choice.detail))
                    .collect();
                fields.push(Field::choose(
                    label,
                    &question.note,
                    question.multiple,
                    items,
                    &question.defaults,
                ));
            }
            None => fields.push(Field::fixed(label, &format!("{value}  (given on the command line)"))),
        }
    }
    fields
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

/// `path` with the home directory written as `~`.
fn tilde(path: &std::path::Path, home: &std::path::Path) -> String {
    path.strip_prefix(home)
        .map_or_else(|_| path.display().to_string(), |rest| format!("~/{}", rest.display()))
}

/// What an ACL path is: its kind (in the order shown) and its name.
fn place(path: &str) -> (u8, &'static str, String) {
    match path.split('/').collect::<Vec<_>>().as_slice() {
        ["", "nodes"] => (0, "whole cluster", "nodes".to_owned()),
        ["", "sdn"] => (0, "whole cluster", "networks".to_owned()),
        ["", "storage"] => (0, "whole cluster", "storages".to_owned()),
        ["", "pool", pool] => (1, "pools", (*pool).to_owned()),
        ["", "storage", id] => (2, "storages", (*id).to_owned()),
        ["", "sdn", "zones", _, bridge] => (3, "networks", (*bridge).to_owned()),
        _ => (4, "other", path.to_owned()),
    }
}

/// The plan's steps grouped by what they change, a short line each.
fn grouped_changes(plan: &Plan, home: &std::path::Path) -> Vec<(String, Vec<String>)> {
    use hs_install::Action;
    let mut account = Vec::new();
    let mut roles: std::collections::BTreeMap<&str, Vec<String>> = std::collections::BTreeMap::new();
    let mut pools: std::collections::BTreeMap<&str, Vec<String>> = std::collections::BTreeMap::new();
    let mut gold = Vec::new();
    // Who -> (places given, places taken).
    let mut permissions: std::collections::BTreeMap<String, Permissions> = std::collections::BTreeMap::new();
    let mut entries = 0usize;
    let mut snippets: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for step in &plan.steps {
        match &step.action {
            Action::Pveum(args) => {
                let words: Vec<&str> = args.iter().map(String::as_str).collect();
                match words.as_slice() {
                    ["user", "add", user, ..] => account.push(format!("new user {user}")),
                    ["user", "delete", user] => account.push(format!("remove user {user}")),
                    ["user", "token", "delete", _, token] => account.push(format!("remove token {token}")),
                    ["user", "token", "modify", _, token, ..] => {
                        account.push(format!("token {token}: separate its privileges"));
                    }
                    ["role", "add", role, ..] => roles.entry("new").or_default().push((*role).to_owned()),
                    ["role", "modify", role, ..] => roles.entry("changed").or_default().push((*role).to_owned()),
                    ["role", "delete", role] => roles.entry("removed").or_default().push((*role).to_owned()),
                    ["pool", "add", pool, ..] => pools.entry("new").or_default().push((*pool).to_owned()),
                    ["pool", "delete", pool] => pools.entry("removed").or_default().push((*pool).to_owned()),
                    ["pool", "modify", pool, "--vms", vmid, rest @ ..] => {
                        if rest.contains(&"--delete") {
                            gold.push(format!("take VM {vmid} out of pool {pool}"));
                        } else {
                            gold.push(format!("put VM {vmid} in pool {pool}"));
                        }
                    }
                    ["acl", action, path, _, who, "--roles", roles_given] => {
                        let short = who
                            .strip_prefix("homestack@pve!")
                            .map_or_else(|| "the user homestack@pve".to_owned(), |token| format!("token {token}"));
                        let count = roles_given.split(',').count();
                        entries += count;
                        let slot = permissions.entry(short).or_default();
                        let (order, kind, name) = place(path);
                        let list = if *action == "modify" {
                            slot.given.entry((order, kind)).or_default()
                        } else {
                            slot.taken.entry((order, kind)).or_default()
                        };
                        if !list.contains(&name) {
                            list.push(name);
                        }
                    }
                    _ => account.push(step.description.clone()),
                }
            }
            Action::Pvesh(_) => gold.push(step.description.replace("tag Gold", "tag VM")),
            Action::CreateToken {
                token, file, replaced, ..
            } => {
                let mut line = format!("new token {token}, secret to {}", tilde(file, home));
                if let Some(old) = replaced {
                    let name = old
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    account.push(line.clone());
                    line = format!("old file kept as {name}");
                }
                account.push(line);
            }
            Action::WriteSnippet { node, name, .. } => {
                snippets.entry(format!("write {name}")).or_default().push(node.clone());
            }
            Action::RemoveSnippet { node, name, .. } => {
                snippets.entry(format!("remove {name}")).or_default().push(node.clone());
            }
        }
    }
    let mut sections = Vec::new();
    if !account.is_empty() {
        sections.push(("Account and tokens".to_owned(), account));
    }
    for (title, group) in [("Roles", roles), ("Pools", pools)] {
        if group.is_empty() {
            continue;
        }
        let counts: Vec<String> = group
            .iter()
            .map(|(kind, names)| format!("{} {kind}", names.len()))
            .collect();
        let lines = group
            .iter()
            .flat_map(|(kind, names)| wrap_names(&format!("{kind}: "), names))
            .collect();
        sections.push((format!("{title} ({})", counts.join(", ")), lines));
    }
    if !gold.is_empty() {
        let heading = plan
            .gold
            .as_ref()
            .map_or_else(|| "Gold".to_owned(), |g| format!("Gold {g}"));
        sections.push((heading, gold));
    }
    if !permissions.is_empty() {
        let lines = permissions
            .iter()
            .flat_map(|(who, slot)| {
                let mut lines = vec![format!("{who}:")];
                for (heading, map) in [("", &slot.given), ("taken away from ", &slot.taken)] {
                    for ((_, kind), names) in map {
                        lines.extend(wrap_names(&format!("    {heading}{kind:<14} "), names));
                    }
                }
                lines
            })
            .collect();
        sections.push((format!("Permissions ({entries} entries)"), lines));
    }
    if !snippets.is_empty() {
        let lines = snippets
            .iter()
            .map(|(what, nodes)| format!("{what} on {}", nodes.join(", ")))
            .collect();
        sections.push(("Cloud-Init snippets".to_owned(), lines));
    }
    sections
}

/// The summary of a plan: where root comes from, the choices, warnings and
/// every change.
fn summary_of(access_line: &str, plan: &Plan, home: &std::path::Path) -> Summary {
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
    let _ = access_line;
    let mut sections = vec![("Your choices".to_owned(), chosen)];
    if !plan.taken.is_empty() {
        sections.push(("Chosen without asking".to_owned(), plan.taken.clone()));
    }
    let mut notices = plan.warnings.clone();
    notices.extend(plan.node_errors.iter().cloned());
    if !notices.is_empty() {
        sections.push(("Warnings (they do not stop the install)".to_owned(), notices));
    }
    if plan.steps.is_empty() {
        sections.push((
            "Changes in Proxmox".to_owned(),
            vec!["Nothing to change: HomeStack is installed as chosen.".to_owned()],
        ));
    } else {
        sections.extend(grouped_changes(plan, home));
    }
    Summary {
        sections,
        problems: Vec::new(),
    }
}

/// The row of the form for the servers.
fn servers_field(install: &InstallArgs, want: bool, draft_domain: &str) -> Field {
    if !want {
        let why = if install.test {
            "none for the test part"
        } else {
            "not deployed  (given on the command line)"
        };
        return Field::fixed("Servers", why);
    }
    match &install.server_domain {
        Some(domain) => Field::fixed("Server domain", &format!("{domain}  (given on the command line)")),
        None => Field::text(
            "Server domain",
            "The servers are named apiN.<domain>, one per node (api1 on the first node, ...). Each name must \
resolve on this desktop to the server's address in the workspace network; the certificate *.<domain> comes \
from Let's Encrypt.",
            draft_domain,
        ),
    }
}

/// The confirmation's section of the servers, and their problems.
fn servers_section(summary: &mut Summary, servers: &ServerPlan) {
    let mut lines = servers.lines.clone();
    lines.extend(servers.warnings.iter().map(|warning| format!("warning: {warning}")));
    lines.push("Then: homestack status".to_owned());
    summary.sections.push(("Servers".to_owned(), lines));
    summary.problems.extend(servers.problems.iter().cloned());
}

/// Run `install` or `uninstall`; the exit code.
pub async fn run(args: &Args, given: &InstallArgs, command: Install, ctx: &mut Context) -> Result<i32, AppError> {
    let mut install = given.clone();
    let want_servers = command == Install::Install && !install.test && !install.no_servers;
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
            || install.template_storage.is_none()
            || want_servers && install.server_domain.is_none());
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
    let files = Files {
        dir: settings.token_dir.clone(),
        home: ctx.home.clone(),
    };
    let servers = std::sync::Arc::clone(&ctx.servers);

    let (plan, server_plan, domain) = if interactive {
        let mut questions = hs_install::questions(root.as_mut(), &access, &settings).map_err(failure)?;
        let draft = draft_path(&settings);
        let kept = load_draft(&draft);
        apply_draft(&mut questions, &kept);
        let draft_domain = kept
            .get(DRAFT_DOMAIN)
            .and_then(|values| values.first())
            .cloned()
            .unwrap_or_default();
        let root_line = format!(
            "{}  \u{2014}  {}, Proxmox VE {}, {}",
            root.describe(),
            access.host,
            access.version,
            if access.topology.cluster {
                format!("cluster of {} nodes", access.topology.nodes.len())
            } else {
                "single node".to_owned()
            }
        );
        let mut fields = form_fields(&root_line, &settings, &questions);
        fields.push(servers_field(install, want_servers, &draft_domain));
        let mut form = Form::new("HomeStack install", fields);
        let home = ctx.home.clone();
        let mut last: Option<(Plan, Option<ServerPlan>, Option<String>)> = None;
        let mut summarize = |answers: &[Answer]| {
            let chosen = answered(&settings, &questions, answers);
            let domain = typed_domain(install, answers);
            match hs_install::make_plan(root.as_mut(), &access, command, &chosen) {
                Ok(plan) => {
                    let mut summary = summary_of(&access_line, &plan, &home);
                    let server_plan = want_servers.then(|| {
                        plan_servers(
                            &plan,
                            domain.as_deref().unwrap_or_default(),
                            install.server_image.as_deref(),
                            &files,
                            servers.as_ref(),
                        )
                    });
                    if let Some(server_plan) = &server_plan {
                        servers_section(&mut summary, server_plan);
                    }
                    last = Some((plan, server_plan, domain));
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
        let outcome = ctx.console.form(&mut form, &mut summarize);
        let answers = form.answers();
        save_draft(&draft, &questions, &answers, typed_domain(install, &answers).as_deref());
        match (outcome, last) {
            (FormOutcome::Install(_), Some((plan, server_plan, domain))) => {
                let line = same_as(
                    command,
                    install,
                    &plan.settings,
                    domain.as_deref().filter(|_| want_servers),
                );
                ctx.console.out(&format!("Same as: {line}\n"));
                (plan, server_plan, domain)
            }
            _ => return cancelled(ctx),
        }
    } else {
        let no_domain = want_servers && install.server_domain.is_none();
        let first = hs_install::make_plan(root.as_mut(), &access, command, &settings);
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
        let server_plan = domain
            .as_deref()
            .map(|domain| plan_servers(&plan, domain, install.server_image.as_deref(), &files, servers.as_ref()));
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
        (plan, server_plan, domain)
    };
    let _ = &domain;

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
    if !interactive && changes > 0 && !args.yes {
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
    } else if !interactive && !args.json {
        ctx.console.out(&format!(
            "{access_line}\n{}{}",
            plan.text(),
            servers_text.unwrap_or_default()
        ));
    }
    let code = finish(ctx, args, root.as_mut(), &plan, command, &access_line)?;
    let Some(server_plan) = server_plan else {
        return Ok(code);
    };
    if code != 0 {
        return Err(AppError::new(
            "The Proxmox part did not finish, so the servers were not deployed; run this again",
        ));
    }
    drop(root);
    let client_path = args
        .config
        .clone()
        .unwrap_or_else(|| config::default_path(&ctx.home, ctx.config_home.as_deref()));
    let client_path = expand_home(&client_path.display().to_string(), &ctx.home);
    deploy_servers(ctx, args, &files, &server_plan, &client_path).await
}

/// The domain the form was given, or the command line's.
fn typed_domain(install: &InstallArgs, answers: &[Answer]) -> Option<String> {
    if let Some(domain) = &install.server_domain {
        return Some(domain.clone());
    }
    answers.iter().rev().find_map(|answer| match answer {
        Answer::Text(text) if !text.is_empty() => Some(text.clone()),
        _ => None,
    })
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
    let Some(config) = &server_plan.config else {
        return Err(AppError::new(
            "The servers cannot be deployed: the deployment is incomplete",
        ));
    };
    let say_out = |ctx: &mut Context, line: &str| {
        if !args.json {
            ctx.console.out(&format!("{line}\n"));
        }
    };
    say_out(ctx, "\nServers:");
    if ensure_api_token(&files.api_token()).map_err(AppError::new)? {
        say_out(
            ctx,
            &format!("  new client token in {}", tilde(&files.api_token(), &files.home)),
        );
    }
    for (path, text) in [
        (files.server_toml(), server_plan.server_toml.as_str()),
        (files.deploy_toml(), server_plan.deploy_toml.as_str()),
    ] {
        if let Some(kept) = write_private(&path, text).map_err(AppError::new)? {
            say_out(
                ctx,
                &format!("  the old {} is kept as {}", tilde(&path, &files.home), kept.display()),
            );
        }
    }

    // The deployment says what it does as it goes.
    let (sender, receiver) = std::sync::mpsc::channel::<String>();
    let say = move |line: &str| {
        let _ = sender.send(line.to_owned());
    };
    let servers = std::sync::Arc::clone(&ctx.servers);
    let mut deploying = servers.deploy(config, &say);
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
    result.map_err(|error| {
        AppError::new(format!(
            "Deploying the servers failed: {error}\nThe Proxmox part is installed; run homestack install again to retry."
        ))
    })?;

    let (urls, ca_file) = servers.client_server(config);
    let token_file = tilde(&files.api_token(), &files.home);
    let text = crate::servers::client_toml(&server_plan.old_config, &urls, ca_file, &token_file)
        .map_err(|error| AppError::new(format!("client.toml: {error}")))?;
    if let Some(kept) = write_private(client_path, &text).map_err(AppError::new)? {
        say_out(ctx, &format!("  the old client.toml is kept as {}", kept.display()));
    }
    say_out(ctx, &format!("  {} written", tilde(client_path, &files.home)));
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

/// Where the choices of the form are kept between runs.
fn draft_path(settings: &Settings) -> std::path::PathBuf {
    settings.token_dir.join("install-draft.toml")
}

/// The kept choices, by option name without dashes; nothing when the file
/// is missing or cannot be read.
fn load_draft(path: &std::path::Path) -> std::collections::BTreeMap<String, Vec<String>> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return std::collections::BTreeMap::new();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return std::collections::BTreeMap::new();
    };
    table
        .into_iter()
        .filter_map(|(key, value)| {
            let values: Vec<String> = value
                .as_array()?
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect();
            Some((key, values))
        })
        .collect()
}

/// Start each question from the kept choices that are still possible.
fn apply_draft(questions: &mut [hs_install::Question], draft: &std::collections::BTreeMap<String, Vec<String>>) {
    for question in questions {
        let Some(values) = draft.get(question.option.trim_start_matches('-')) else {
            continue;
        };
        let picked: Vec<usize> = question
            .choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| values.contains(&choice.value))
            .map(|(index, _)| index)
            .collect();
        if !picked.is_empty() {
            question.defaults = picked;
        }
    }
}

/// Keep the choices of the form (mode 0600); a failure only loses them.
fn save_draft(path: &std::path::Path, questions: &[hs_install::Question], answers: &[Answer], domain: Option<&str>) {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut table = toml::Table::new();
    for (question, answer) in questions.iter().zip(answers) {
        let Answer::Picked(picked) = answer else { continue };
        let values: Vec<toml::Value> = picked
            .iter()
            .filter_map(|index| question.choices.get(*index))
            .map(|choice| toml::Value::String(choice.value.clone()))
            .collect();
        table.insert(
            question.option.trim_start_matches('-').to_owned(),
            toml::Value::Array(values),
        );
    }
    if let Some(domain) = domain {
        table.insert(
            DRAFT_DOMAIN.to_owned(),
            toml::Value::Array(vec![toml::Value::String(domain.to_owned())]),
        );
    }
    let text = format!("# The last choices of `homestack install`; the next run starts from them.\n{table}");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
    {
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        let _ = file.write_all(text.as_bytes());
    }
}

/// `names` after `lead`, comma-separated, in lines of at most 80 characters.
fn wrap_names(lead: &str, names: &[String]) -> Vec<String> {
    let indent = " ".repeat(lead.chars().count());
    let mut lines = vec![lead.to_owned()];
    for (index, name) in names.iter().enumerate() {
        let piece = if index + 1 < names.len() {
            format!("{name}, ")
        } else {
            name.clone()
        };
        let current = lines.last().map_or(0, |line| line.chars().count());
        if current + piece.chars().count() > 80 && current > indent.len() {
            lines.push(indent.clone());
        }
        if let Some(line) = lines.last_mut() {
            line.push_str(&piece);
        }
    }
    lines.into_iter().map(|line| line.trim_end().to_owned()).collect()
}

/// What one account or token is given and loses, by kind of place.
#[derive(Default)]
struct Permissions {
    given: std::collections::BTreeMap<(u8, &'static str), Vec<String>>,
    taken: std::collections::BTreeMap<(u8, &'static str), Vec<String>>,
}
