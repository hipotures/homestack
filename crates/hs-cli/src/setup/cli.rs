//! `homestack setup` on the command line (Python `setup_cli.run_setup`):
//! `setup list`, `setup status TARGET`, and `setup TARGET SELECTORS…` with
//! its plan, confirmation and run.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use hs_client::Client;
use serde_json::{Value, json};

use super::catalog::{
    Catalog, Github, discover_repositories, github_identity, load_catalog, parse_assignments, save_snapshot,
    select_entries,
};
use super::definitions::{SetupConfig, parse_setup};
use super::engine::{Connection, Observer, execute_plan, inspect_workspace_state};
use super::plan::{build_plan, expand_config_entries};
use super::repo::{inspect_repository, resolve_repository_argument, rotate_repository_key, setup_repository};
use super::workspace::{SshWorkspace, Workspace};
use super::{RepoSettings, Settings, Target, show};
use crate::app::{Context, client_error, is_vmid, valid_name};
use crate::args::{Args, Setup};
use crate::config::Loaded;
use crate::master::Master;
use crate::system::System;
use crate::touch::Sounds;
use crate::{AppError, fido};

const GOLD_TAG: &str = "homestack-gold";
const WORKSPACE_TAG: &str = "homestack-ws";
const ALIEN_WORKSPACE_TAG: &str = "homestack-alien";

/// The settings of a setup from `client.toml`; the workspace user comes
/// from the server when the target is resolved.
pub fn settings(ctx: &Context, loaded: &Loaded) -> Result<Settings, AppError> {
    settings_of(&ctx.home, ctx.state_home.as_deref(), loaded)
}

/// [`settings`] from the desktop's home and `$XDG_STATE_HOME` (the TUI's
/// worker has no [`Context`]).
pub fn settings_of(home: &Path, state_home: Option<&Path>, loaded: &Loaded) -> Result<Settings, AppError> {
    let setup = match &loaded.client.setup {
        Some(table) => parse_setup(table)?,
        None => SetupConfig::default(),
    };
    Ok(Settings {
        home: home.to_path_buf(),
        user: loaded.client.workspace_ssh.user.clone(),
        uid: 0,
        gid: 0,
        setup,
        repo: RepoSettings {
            owner: loaded.client.repo.owner.clone(),
            checkout_root: loaded.client.repo.checkout_root.clone(),
            sort: loaded.client.repo.sort.clone(),
        },
        config_path: loaded.path.clone(),
        state_home: state_home.map_or_else(|| home.join(".local").join("state"), Path::to_path_buf),
        keys: super::Keys {
            identity_files: loaded.client.workspace_ssh.identity_files.clone(),
            identities_only: loaded.client.workspace_ssh.identities_only,
        },
    })
}

/// The kind of security key that will be asked for, for prompts.
pub fn key_label(system: &dyn System, loaded: &Loaded) -> String {
    let preference = &loaded.client.workspace_ssh.fido_devices;
    if preference.is_empty() {
        return "security key".to_owned();
    }
    fido::list_devices(system)
        .and_then(|devices| fido::select(preference, &devices))
        .map_or_else(|_| "security key".to_owned(), |(entry, _)| fido::label(&entry))
}

/// A hint about extra keys when no `fido_devices` preference is set.
pub fn key_advice(system: &dyn System, loaded: &Loaded) -> Option<String> {
    if !loaded.client.workspace_ssh.fido_devices.is_empty() {
        return None;
    }
    fido::list_devices(system)
        .ok()
        .and_then(|devices| fido::advice(&devices))
}

/// The role tags a setup accepts (Python `require_setup_workspace_tag`):
/// a native or an alien workspace, never Gold; whether it is alien.
pub fn check_role(vmid: u32, tags: &[&str]) -> Result<bool, AppError> {
    if tags.contains(&GOLD_TAG) {
        return Err(AppError::new(format!(
            "Refusing Setup on Gold VM {vmid}: forbidden tag '{GOLD_TAG}'."
        )));
    }
    let roles: Vec<&&str> = tags
        .iter()
        .filter(|tag| [WORKSPACE_TAG, ALIEN_WORKSPACE_TAG].contains(tag))
        .collect();
    if roles.len() > 1 {
        return Err(AppError::new(format!(
            "VM {vmid} has conflicting workspace role tags: '{WORKSPACE_TAG}' and '{ALIEN_WORKSPACE_TAG}'."
        )));
    }
    if roles.is_empty() {
        return Err(AppError::new(format!(
            "VM {vmid} is not a HomeStack workspace: required tag '{WORKSPACE_TAG}' or '{ALIEN_WORKSPACE_TAG}' is missing."
        )));
    }
    Ok(tags.contains(&ALIEN_WORKSPACE_TAG))
}

/// Resolve `text` to a running workspace (Python `setup.resolve_target`),
/// and take the workspace user from the server's configuration.
pub async fn resolve_target(client: &Client, settings: &mut Settings, text: &str) -> Result<Target, AppError> {
    resolve(client, settings, text, &|_, _| {
        "Setup requires a running workspace; lifecycle operations must be performed separately".to_owned()
    })
    .await
}

/// [`resolve_target`] with the refusal of a workspace that is not
/// running given.
async fn resolve(
    client: &Client,
    settings: &mut Settings,
    text: &str,
    stopped: &(dyn Fn(u32, &str) -> String + Sync),
) -> Result<Target, AppError> {
    let text = text.trim();
    let config = client.get("/v1/config").await.map_err(client_error)?;
    let server = hs_model::config::Config::parse(config.as_str().unwrap_or_default())
        .map_err(|error| AppError::new(format!("The server's configuration cannot be read: {error}")))?;
    settings.user.clone_from(&server.user.name);
    settings.uid = server.user.uid;
    settings.gid = server.user.gid;
    if !is_vmid(text) && !valid_name(text) {
        return Err(AppError::new(format!(
            "Invalid workspace target {}. Use a numeric VMID or a workspace name whose first character is a letter.",
            super::text::py_repr(text)
        )));
    }
    let found = client
        .get(&format!("/v1/workspaces/{text}"))
        .await
        .map_err(client_error)?;
    let vmid = found["vmid"]
        .as_u64()
        .and_then(|vmid| u32::try_from(vmid).ok())
        .ok_or_else(|| AppError::new(format!("VMID {text} does not exist")))?;
    if Some(vmid) == server.gold_vmid {
        return Err(AppError::new(format!(
            "Refusing repository operation on Gold VM {vmid}"
        )));
    }
    let tags: Vec<&str> = found["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let alien = check_role(vmid, &tags)?;
    let name = found["name"].as_str().unwrap_or_default().trim().to_owned();
    if name.is_empty() {
        return Err(AppError::new(format!("VM {vmid} has no name")));
    }
    if !valid_name(&name) {
        return Err(AppError::new(format!(
            "Invalid workspace name {}",
            super::text::py_repr(&name)
        )));
    }
    let status = found["status"].as_str().unwrap_or_default().to_owned();
    if status != "running" {
        return Err(AppError::new(stopped(vmid, &status)));
    }
    // Imported workspaces need not encode their address in the VMID.
    // The guest agent reports the actual address with a CIDR prefix.
    let ip = found["actual_ip"]
        .as_str()
        .and_then(|ip| ip.split('/').next())
        .or_else(|| found["configured_ip"].as_str())
        .ok_or_else(|| {
            AppError::new(format!(
                "VMID {vmid} has no workspace address: the guest agent reported no IPv4 address on eth0 and the API has no configured address"
            ))
        })?
        .to_owned();
    Ok(Target {
        vmid,
        name,
        node: found["node"].as_str().unwrap_or_default().to_owned(),
        status,
        alien,
        ip,
        user: settings.user.clone(),
        home: settings.guest_home(),
        local: false,
    })
}

/// Open the workspace connection (one touch of the security key).
pub fn connect(
    system: &Arc<dyn System>,
    home: &Path,
    loaded: &Loaded,
    settings: &Settings,
    target: &Target,
) -> Result<Box<dyn Workspace>, AppError> {
    let master = Master::configured(
        home,
        &loaded.client.workspace_ssh,
        &Sounds::from_table(loaded.client.sounds.as_ref()),
        &settings.user,
        &target.ip,
        Some(&target.name),
    );
    Ok(Box::new(SshWorkspace::open(Arc::clone(system), home, master)?))
}

/// [`connect`] for the TUI: ssh runs behind `prefix` (the askpass bridge),
/// so the touch of a security key and its other questions are asked in the
/// TUI; ssh signs itself, so the touch reaches the askpass. Setting `stop`
/// stops ssh while it connects.
pub fn connect_asking(
    system: &Arc<dyn System>,
    home: &Path,
    loaded: &Loaded,
    settings: &Settings,
    target: &Target,
    prefix: Vec<String>,
    stop: Arc<AtomicBool>,
) -> Result<Box<dyn Workspace>, AppError> {
    let mut master = Master::configured(
        home,
        &loaded.client.workspace_ssh,
        &Sounds::from_table(loaded.client.sounds.as_ref()),
        &settings.user,
        &target.ip,
        Some(&target.name),
    );
    // `Master::start` lets ssh sign itself behind the bridge.
    master.prefix = prefix;
    master.stop = Some(stop);
    Ok(Box::new(SshWorkspace::open(Arc::clone(system), home, master)?))
}

/// The workspace connection, or the desktop itself for a local target.
pub fn open_workspace(
    system: &Arc<dyn System>,
    home: &Path,
    loaded: &Loaded,
    settings: &Settings,
    target: &Target,
) -> Result<Box<dyn Workspace>, AppError> {
    if target.local {
        return Ok(Box::new(super::workspace::LocalWorkspace {
            home: settings.home.clone(),
            user: settings.user.clone(),
        }));
    }
    connect(system, home, loaded, settings, target)
}

/// `setup --local` (Python `local_configuration`): the desktop's own
/// account, only the Backup items, no repositories.
pub fn local_settings(mut settings: Settings) -> Result<Settings, AppError> {
    let uid = rustix::process::getuid().as_raw();
    let (name, _) =
        super::workspace::account(uid).ok_or_else(|| AppError::new("The desktop account cannot be read"))?;
    settings.user = name;
    settings.uid = uid;
    settings.gid = rustix::process::getgid().as_raw();
    settings.setup.items.retain(|entry| entry.handler == "backup");
    let groups: Vec<String> = settings.setup.items.iter().map(|entry| entry.group.clone()).collect();
    settings.setup.groups.retain(|group| groups.contains(&group.id));
    Ok(settings)
}

/// This desktop as the target of `setup --local`.
#[must_use]
pub fn local_target_of(settings: &Settings) -> Target {
    Target {
        vmid: 0,
        name: rustix::system::uname().nodename().to_string_lossy().into_owned(),
        node: String::new(),
        status: String::new(),
        alien: false,
        ip: String::new(),
        user: settings.user.clone(),
        home: settings.home.display().to_string(),
        local: true,
    }
}

/// The catalog with the owner's repositories.
pub fn catalog_with_repositories(system: &dyn System, settings: &Settings) -> Catalog {
    let discover = |settings: &Settings| discover_repositories(system, settings);
    load_catalog(settings, Some(&discover))
}

/// Prints each item's state as the run goes (Python's `progress`); with
/// `--json` on stderr, as Python's console is then.
struct Printer<'a> {
    console: &'a mut dyn crate::system::Console,
    json: bool,
    sounds: Sounds,
}

impl Observer for Printer<'_> {
    fn progress(&mut self, id: &str, state: &str) {
        let line = format!("{id}: {state}\n");
        if self.json {
            self.console.err(&line);
        } else {
            self.console.out(&line);
        }
    }

    fn register_desktop(&mut self, argv: &[String]) -> Result<crate::system::Output, AppError> {
        super::herdr::register_desktop(argv, &self.sounds)
    }
}

/// Run `homestack setup`.
#[allow(clippy::too_many_lines)]
pub async fn run(
    args: &Args,
    setup: &Setup,
    ctx: &mut Context,
    loaded: &Loaded,
    client: &Client,
) -> Result<i32, AppError> {
    let mut settings = settings(ctx, loaded)?;
    let system = Arc::clone(&ctx.system);
    let json_mode = args.json;
    let emit = |ctx: &mut Context, value: &Value| {
        let text = serde_json::to_string_pretty(value).unwrap_or_default();
        ctx.console.out(&format!("{text}\n"));
    };
    let local = setup.local;
    let mut tokens = setup.selectors.clone();
    let mut target_arg = setup.target.clone();
    let mut local_target = None;
    if local {
        settings = local_settings(settings)?;
        local_target = Some(local_target_of(&settings));
        if target_arg.as_deref().is_some_and(|text| text.contains('=')) {
            tokens.insert(0, target_arg.take().unwrap_or_default());
        } else if !matches!(target_arg.as_deref(), None | Some("list" | "status")) {
            return Err(AppError::new(
                "--local does not accept a workspace target; use setup --local app=bk",
            ));
        }
    }
    let load = |settings: &Settings| {
        if local {
            load_catalog(settings, None)
        } else {
            catalog_with_repositories(system.as_ref(), settings)
        }
    };
    if target_arg.as_deref() == Some("status") {
        let wanted = usize::from(!local);
        if tokens.len() != wanted || args.yes || setup.dry_run || setup.non_interactive || setup.catalog.is_some() {
            return Err(AppError::new(
                "Usage: homestack setup status VMID|NAME [--json] or setup --local status [--json]",
            ));
        }
        let catalog = load(&settings);
        let target = match &local_target {
            Some(target) => target.clone(),
            None => resolve_target(client, &mut settings, &tokens[0]).await?,
        };
        if !json_mode && !local {
            ctx.console
                .out("SSH: authenticate once to inspect workspace setup state; the connection closes after status.\n");
        }
        let mut workspace = open_workspace(&system, &ctx.home, loaded, &settings, &target)?;
        let state = inspect_workspace_state(workspace.as_mut(), &settings, &target, &catalog.entries)?;
        drop(workspace);
        let mut result = json!({"ok": true, "command": "setup status", "target": target.json()});
        if let (Some(result), Value::Object(state)) = (result.as_object_mut(), state.clone()) {
            result.extend(state);
        }
        if json_mode {
            emit(ctx, &result);
        } else {
            let order: Vec<String> = expand_config_entries(&catalog.entries)
                .into_iter()
                .map(|e| e.id)
                .collect();
            let text = show::status(&target.json(), &state, &order, &ctx.style);
            ctx.console.out(&text);
        }
        return Ok(0);
    }
    if target_arg.as_deref() == Some("list") {
        if !tokens.is_empty() || args.yes || setup.dry_run || setup.non_interactive || setup.catalog.is_some() {
            return Err(AppError::new(
                "setup list is targetless discovery; execution selectors/options are not accepted",
            ));
        }
        let mut catalog = load(&settings);
        save_snapshot(&settings, &mut catalog)?;
        if json_mode {
            let groups: Vec<Value> = settings
                .setup
                .groups
                .iter()
                .map(|g| json!({"id": g.id, "label": g.label}))
                .collect();
            let result = json!({"ok": true, "command": "setup list", "catalog": catalog.snapshot_id,
                                "items": catalog.rows(&settings), "groups": groups,
                                "repository_error": catalog.repository_error, "guest_state": "unknown"});
            emit(ctx, &result);
        } else {
            let text = show::catalog(&settings, &catalog, &ctx.style);
            ctx.console.out(&text);
        }
        return Ok(0);
    }
    if target_arg.is_none() && !local {
        return Err(AppError::new(
            "An explicit VMID or exact workspace name is required; use setup list for discovery",
        ));
    }
    if tokens.is_empty() && setup.catalog.is_some() {
        return Err(AppError::new(
            "--catalog requires explicit selectors; use setup list to inspect a catalog",
        ));
    }
    let unattended = json_mode || setup.non_interactive || !ctx.stdin_tty;
    let (entries, catalog_id) = if tokens.is_empty() {
        if unattended || setup.dry_run {
            return Err(AppError::new(
                "No actions selected. Run setup list, then specify env=bash, app=codex,bk or another selector",
            ));
        }
        (Vec::new(), None)
    } else {
        parse_assignments(&tokens, &settings)?;
        let identity = || github_identity(system.as_ref(), &settings);
        let catalog = || catalog_with_repositories(system.as_ref(), &settings);
        let github = Github {
            identity: &identity,
            catalog: &catalog,
        };
        select_entries(&settings, &tokens, setup.catalog.as_deref(), &github)?
    };
    // The selection is checked before even the read-only resolution.
    let target = match (&local_target, &target_arg) {
        (Some(target), _) => target.clone(),
        (None, Some(text)) => resolve_target(client, &mut settings, text).await?,
        (None, None) => {
            return Err(AppError::new(
                "An explicit resolved workspace and at least one action are required",
            ));
        }
    };
    if entries.is_empty() {
        return Err(AppError::new(
            "Interactive setup opens in the HomeStack TUI: run homestack, select the workspace and press e",
        ));
    }
    let plan = build_plan(&settings, &target, &entries, catalog_id, unattended, true)?;
    if setup.dry_run {
        if json_mode {
            emit(ctx, &json!({"ok": true, "dry_run": true, "plan": plan.public()}));
        } else {
            let text = show::plan(&plan, &ctx.style);
            ctx.console.out(&text);
            ctx.console.out(if local {
                "Dry run: no local changes.\n"
            } else {
                "Dry run: no guest SSH or remote changes.\n"
            });
        }
        return Ok(0);
    }
    if !args.yes {
        if json_mode || !ctx.stdin_tty || setup.non_interactive {
            emit(
                ctx,
                &json!({"ok": false, "confirmation_required": true, "plan": plan.public(),
                        "message": "No changes made. Re-run with --yes to accept this plan."}),
            );
            return Ok(3);
        }
        let text = show::plan(&plan, &ctx.style);
        ctx.console.out(&text);
        if !confirm(ctx, "Apply these selected actions?") {
            ctx.console.out("Cancelled. No changes were made.\n");
            return Ok(0);
        }
    }
    if args.yes && !json_mode {
        let text = show::plan(&plan, &ctx.style);
        ctx.console.out(&text);
    }
    if !local {
        let notice = format!(
            "SSH: authenticate with the configured workspace identity; touch your {} if prompted.\n",
            key_label(system.as_ref(), loaded)
        );
        say(ctx, json_mode, &notice);
        if let Some(tip) = key_advice(system.as_ref(), loaded) {
            say(ctx, json_mode, &format!("{tip}\n"));
        }
    }
    let home = ctx.home.clone();
    let mut open = || open_workspace(&system, &home, loaded, &settings, &target);
    let result = {
        let mut printer = Printer {
            console: ctx.console.as_mut(),
            json: json_mode,
            sounds: Sounds::from_table(loaded.client.sounds.as_ref()),
        };
        execute_plan(
            &settings,
            system.as_ref(),
            &plan,
            Connection::Connect(&mut open),
            &mut printer,
        )
    };
    if json_mode {
        emit(ctx, &result);
    } else {
        if result["snapshot"]["created"] == Value::Bool(true) {
            ctx.console.out(&format!(
                "Snapshot: {}\n",
                result["snapshot"]["path"].as_str().unwrap_or("None")
            ));
        }
        for item in result["results"].as_array().into_iter().flatten() {
            ctx.console.out(&format!(
                "{}: {} — {}\n",
                item["label"].as_str().unwrap_or_default(),
                item["status"].as_str().unwrap_or_default(),
                item["detail"].as_str().unwrap_or_default()
            ));
        }
    }
    Ok(if result["ok"] == Value::Bool(true) { 0 } else { 1 })
}

/// Rich's `Confirm.ask(question, default=False)`.
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

/// A line of the console: stderr with `--json`, as Python's console is.
fn say(ctx: &mut Context, json_mode: bool, text: &str) {
    if json_mode {
        ctx.console.err(text);
    } else {
        ctx.console.out(text);
    }
}

/// Run `homestack repo TARGET [REPOSITORY]` (Python's `repo` command): the
/// repository's state, then Setup or a key rotation chosen from a menu.
pub async fn repo(
    args: &Args,
    target: &str,
    explicit: Option<&str>,
    ctx: &mut Context,
    loaded: &Loaded,
    client: &Client,
) -> Result<i32, AppError> {
    let mut settings = settings(ctx, loaded)?;
    let target = resolve(client, &mut settings, target, &|vmid, status| {
        format!("Workspace VM {vmid} is {status}; repository operations require a running VM.")
    })
    .await?;
    let repository = resolve_repository_argument(settings.repo.owner.as_deref(), explicit, &target.name)?;
    let system = Arc::clone(&ctx.system);
    let mut workspace = connect(&system, &ctx.home, loaded, &settings, &target)?;
    let state = inspect_repository(
        &settings,
        system.as_ref(),
        workspace.as_mut(),
        target.vmid,
        &target.name,
        &repository,
        true,
    )?;
    if args.json {
        let text = serde_json::to_string_pretty(&state).unwrap_or_default();
        ctx.console.out(&format!("{text}\n"));
        return Ok(0);
    }
    let text = show::repository(&state, &ctx.style);
    ctx.console.out(&text);
    if !ctx.stdin_tty {
        return Err(AppError::new(
            "Interactive repository action selection requires a TTY; use --json for status-only output",
        ));
    }
    let bold = |text: &str| ctx.style.paint(text, crate::view::Color::Bold);
    let menu = format!(
        "{} Exit   {} Setup   {} Rotate key\n",
        bold("[1]"),
        bold("[2]"),
        bold("[3]")
    );
    ctx.console.out(&menu);
    let action = choose(ctx);
    if action == "1" {
        return Ok(0);
    }
    let live = ctx.style.color;
    let console = ctx.console.as_mut();
    let mut activity = |message: &str| {
        if live {
            console.out(&format!("\r\x1b[2K{message}"));
        }
    };
    let result = if action == "2" {
        setup_repository(&settings, system.as_ref(), workspace.as_mut(), &state, &mut activity)
    } else {
        rotate_repository_key(&settings, system.as_ref(), workspace.as_mut(), &state, &mut activity)
    };
    if live {
        ctx.console.out("\r\x1b[2K");
    }
    let result = result?;
    if let Some(message) = result["message"].as_str().filter(|message| !message.is_empty()) {
        let line = ctx.style.paint(message, crate::view::Color::Bold);
        ctx.console.out(&format!("{line}\n"));
    }
    let text = show::repository(&result, &ctx.style);
    ctx.console.out(&text);
    Ok(if result["ready"] == Value::Bool(true) { 0 } else { 1 })
}

/// Rich's `Prompt.ask("Action", choices=["1", "2", "3"], default="1")`.
fn choose(ctx: &mut Context) -> String {
    loop {
        let Some(answer) = ctx.console.ask("Action [1/2/3] (1): ") else {
            return "1".to_owned();
        };
        let answer = answer.trim();
        if answer.is_empty() {
            return "1".to_owned();
        }
        if ["1", "2", "3"].contains(&answer) {
            return answer.to_owned();
        }
        ctx.console.out("Please select one of the available options\n");
    }
}
