//! Herdr: its installer, a persistent user service, and the desktop's
//! machine profile named after the VM (Python `setup_herdr.py`). A running
//! Herdr server is never updated, restarted or stopped.

use base64::Engine;
use hs_install::shell;
use serde_json::{Value, json};

use super::engine::{Observer, truthy};
use super::guest::{command_environment, guest, require_tool};
use super::workspace::Workspace;
use super::{HERDR_UNIT_PATH, Settings, Target, sha256_hex};
use crate::AppError;
use crate::guest_args;
use crate::system::{Output, System};
use crate::touch::{self, Sounds};

/// The user unit, byte for byte Python's.
pub const UNIT: &[u8] = include_bytes!("../../guest/herdr/herdr.service");
pub const INSTALL_COMMAND: &str = "curl -fsSL https://herdr.dev/install.sh | sh";
pub const AUTH_NOTICE: &str =
    "Connecting desktop Herdr through separate SSH authentication. Touch your YubiKey/security key if prompted.";

/// Register the desktop's Herdr profile on the terminal (Python
/// `register_desktop`): the notice and the command on stderr, the touch
/// cues while it runs; no output is captured.
pub fn register_desktop(argv: &[String], sounds: &Sounds) -> Result<Output, AppError> {
    register_desktop_with(argv, sounds, touch::default_player())
}

/// [`register_desktop`] with the cues played by `player`.
pub fn register_desktop_with(argv: &[String], sounds: &Sounds, player: touch::Player) -> Result<Output, AppError> {
    eprintln!("{AUTH_NOTICE}");
    eprintln!("{}", shell::join(argv));
    touch::touch_wait("herdr", sounds, player, |watch| {
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .map_err(|error| AppError::new(format!("Could not run herdr: {error}")))?;
        let code = status.code().unwrap_or(-1);
        if code != 0 {
            watch.finish(false);
        }
        Ok(Output {
            code,
            ..Output::default()
        })
    })
}

fn command(workspace: &mut dyn Workspace, settings: &Settings, line: &str) -> Result<Output, AppError> {
    workspace.run(&command_environment(settings, line, "bash", &[], true), None, false)
}

fn systemctl(workspace: &mut dyn Workspace, settings: &Settings, arguments: &str) -> Result<i32, AppError> {
    Ok(command(
        workspace,
        settings,
        &format!(
            "env XDG_RUNTIME_DIR=/run/user/{} systemctl --user {arguments}",
            settings.uid
        ),
    )?
    .code)
}

/// The desktop profile of this workspace (`herdr machine list`), if any.
pub fn desktop_profile(system: &dyn System, settings: &Settings, target: &Target) -> Result<Option<Value>, AppError> {
    let output = system
        .run(&["herdr", "machine", "list", "--json"].map(str::to_owned))
        .map_err(|_| AppError::new("Herdr setup requires a working desktop herdr executable"))?;
    let invalid = || AppError::new("Could not inspect desktop Herdr machine profiles; output withheld");
    let profiles: Value = serde_json::from_str(&output.stdout).map_err(|_| invalid())?;
    let Some(profiles) = profiles.as_array().filter(|_| output.code == 0) else {
        return Err(invalid());
    };
    for profile in profiles {
        let fields = ["id", "target", "label", "session"]
            .iter()
            .all(|key| profile[key].is_string());
        if !profile.is_object() || !fields || !profile["enabled"].is_boolean() {
            return Err(invalid());
        }
    }
    let targets = [
        format!("{}@{}", settings.user, target.name),
        format!("{}@{}", settings.user, target.ip),
    ];
    let matches: Vec<&Value> = profiles
        .iter()
        .filter(|p| targets.iter().any(|t| p["target"] == json!(t)) && p["session"] == json!("default"))
        .collect();
    if profiles
        .iter()
        .any(|p| p["label"] == json!(target.name) && !matches.contains(&p))
    {
        return Err(AppError::new(
            "The VM name is already used by another desktop Herdr profile; resolve the label conflict first",
        ));
    }
    if matches.len() > 1 {
        return Err(AppError::new(
            "Multiple desktop Herdr profiles reference this workspace; resolve duplicates first",
        ));
    }
    Ok(matches.first().map(|p| (*p).clone()))
}

/// The desktop's SSH alias of the workspace resolves to its address,
/// account and keys (Herdr connects through it).
pub fn verify_desktop_alias(system: &dyn System, settings: &Settings, target: &Target) -> Result<(), AppError> {
    let output = system
        .run(&[
            "ssh".to_owned(),
            "-G".to_owned(),
            format!("{}@{}", settings.user, target.name),
        ])
        .unwrap_or_else(|_| Output::failed(255, ""));
    let mut values: Vec<(String, String)> = Vec::new();
    for line in output.stdout.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        values.push((key.to_owned(), value.trim().to_owned()));
    }
    let all = |key: &str| -> Vec<String> {
        values
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .collect()
    };
    let expand = |path: &str| {
        path.strip_prefix("~/")
            .map_or_else(|| path.to_owned(), |rest| format!("{}/{rest}", settings.home.display()))
    };
    let identities: Vec<String> = all("identityfile").iter().map(|path| expand(path)).collect();
    let expected: Vec<String> = settings.keys.identity_files.iter().map(|path| expand(path)).collect();
    let kinds: std::collections::BTreeSet<_> = expected.iter().filter_map(|path| crate::fido::kind_of(path)).collect();
    // The generated SSH alias selects one security-key kind with Match
    // exec. Require its keys and every ordinary key, not inactive kinds.
    let selected_kinds: std::collections::BTreeSet<_> = expected
        .iter()
        .filter(|path| identities.contains(path))
        .filter_map(|path| crate::fido::kind_of(path))
        .collect();
    let keys_match = (kinds.len() < 2 || !selected_kinds.is_empty())
        && expected.iter().all(|path| {
            let inactive =
                kinds.len() > 1 && crate::fido::kind_of(path).is_some_and(|kind| !selected_kinds.contains(kind));
            inactive || identities.contains(path)
        });
    if output.code != 0 {
        return Err(AppError::new(format!(
            "Could not inspect the desktop SSH alias. Run ssh -G {}@{} on the desktop to see the SSH configuration error, then retry app=herdr",
            settings.user, target.name
        )));
    }
    if all("hostname") != [target.ip.clone()]
        || all("user") != [settings.user.clone()]
        || !keys_match
        || all("identitiesonly") != [if settings.keys.identities_only { "yes" } else { "no" }.to_owned()]
    {
        return Err(AppError::new(
            "Desktop SSH alias does not match the workspace address/account/keys. Include ~/.ssh/config.d/homestack/*.conf in ~/.ssh/config, then retry app=herdr",
        ));
    }
    Ok(())
}

/// The names of the running Herdr sessions.
pub fn running_sessions(workspace: &mut dyn Workspace, settings: &Settings) -> Result<Vec<String>, AppError> {
    let output = command(workspace, settings, "\"$HOME/.local/bin/herdr\" session list --json")?;
    let refused = || AppError::new("Could not inspect Herdr sessions; refusing to start or replace a server");
    let data: Value = serde_json::from_str(&output.stdout).map_err(|_| refused())?;
    let Some(sessions) = data
        .get("sessions")
        .and_then(Value::as_array)
        .filter(|_| output.code == 0)
    else {
        return Err(refused());
    };
    if sessions
        .iter()
        .any(|s| !s["name"].is_string() || !s["running"].is_boolean())
    {
        return Err(refused());
    }
    Ok(sessions
        .iter()
        .filter(|s| s["running"] == json!(true))
        .filter_map(|s| s["name"].as_str().map(str::to_owned))
        .collect())
}

/// Herdr's state on the workspace and the desktop.
pub fn inspect(
    workspace: &mut dyn Workspace,
    settings: &Settings,
    system: &dyn System,
    target: &Target,
) -> Result<Value, AppError> {
    let installed = command(workspace, settings, "\"$HOME/.local/bin/herdr\" --version")?.code == 0;
    let sessions = if installed {
        running_sessions(workspace, settings)?
    } else {
        Vec::new()
    };
    let files = guest(
        workspace,
        settings,
        "managed-files-inspect",
        guest_args! {"paths": [HERDR_UNIT_PATH]},
    )?["items"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let unit = files
        .iter()
        .find(|f| f["path"] == json!(HERDR_UNIT_PATH))
        .cloned()
        .unwrap_or(json!({}));
    let changed = unit["sha256"] != json!(sha256_hex(UNIT)) || unit["mode"] != json!(0o600);
    let enabled = systemctl(workspace, settings, "is-enabled herdr.service")? == 0;
    let active = systemctl(workspace, settings, "is-active herdr.service")? == 0;
    let linger = command(
        workspace,
        settings,
        &format!(
            "loginctl show-user {} --property=Linger --value",
            shell::quote(&settings.user)
        ),
    )?;
    let lingering = linger.code == 0 && linger.stdout.trim() == "yes";
    let profile = desktop_profile(system, settings, target)?;
    let connected = profile
        .as_ref()
        .is_some_and(|p| p["label"] == json!(target.name) && p["enabled"] == json!(true));
    let deferred = !sessions.is_empty() && !active;
    let ready = installed
        && !changed
        && enabled
        && lingering
        && connected
        && sessions.iter().any(|s| s == "default")
        && (active || deferred);
    let state = if ready && deferred {
        "configured; service starts after VM reboot"
    } else if ready {
        "configured"
    } else if installed {
        "needs setup"
    } else {
        "not installed"
    };
    let unit_exists = truthy(&unit["exists"]);
    Ok(json!({
        "installed": installed, "ready": ready, "exists": installed || unit_exists, "state": state,
        "files": files, "will_overwrite": changed && unit_exists, "changed": changed,
        "snapshot_paths": if changed && unit_exists { vec![HERDR_UNIT_PATH] } else { Vec::new() },
        "service_enabled": enabled, "service_active": active, "linger": lingering,
        "running_sessions": sessions, "desktop_profile": connected,
        "desktop_profile_exists": profile.is_some(),
        "detail": if deferred { "Existing Herdr server left running; the user service is enabled for the next VM boot" } else { "" },
    }))
}

/// Check everything Herdr needs before any item writes.
pub fn preflight(
    workspace: &mut dyn Workspace,
    settings: &Settings,
    system: &dyn System,

    target: &Target,
    unattended: bool,
) -> Result<Value, AppError> {
    for tool in ["herdr", "ssh"] {
        if system.which(tool).is_none() {
            return Err(AppError::new(format!("Herdr setup requires desktop {tool}")));
        }
    }
    for tool in ["bash", "curl", "awk", "sha256sum", "systemctl", "loginctl"] {
        require_tool(workspace, settings, tool, &[])?;
    }
    if systemctl(workspace, settings, "show-environment")? != 0 {
        return Err(AppError::new(
            "Herdr requires a usable user systemd manager; prepare the user session separately",
        ));
    }
    guest(
        workspace,
        settings,
        "paths",
        guest_args! {"paths": json!([{"relative": ".local/bin", "directory": true}, {"relative": ".local/bin/herdr"}, {"relative": HERDR_UNIT_PATH}])},
    )?;
    let state = inspect(workspace, settings, system, target)?;
    let exists = truthy(&state["desktop_profile_exists"]);
    if unattended && !exists {
        return Err(AppError::new(
            "Desktop Herdr registration requires an interactive terminal for SSH/security-key authentication; run app=herdr interactively",
        ));
    }
    if !exists {
        verify_desktop_alias(system, settings, target)?;
    }
    Ok(state)
}

/// Name, enable or add the desktop's profile of this workspace.
pub fn connect_desktop(
    system: &dyn System,
    settings: &Settings,

    target: &Target,
    observer: &mut dyn Observer,
    id: &str,
    unattended: bool,
) -> Result<(), AppError> {
    // Read again after the guest's setup: a concurrent edit is kept.
    if let Some(profile) = desktop_profile(system, settings, target)? {
        let profile_id = profile["id"].as_str().unwrap_or_default().to_owned();
        let mut commands: Vec<Vec<String>> = Vec::new();
        if profile["label"] != json!(target.name) {
            commands.push(
                ["herdr", "machine", "rename", &profile_id, "--label", &target.name]
                    .map(str::to_owned)
                    .to_vec(),
            );
        }
        if profile["enabled"] != json!(true) {
            commands.push(["herdr", "machine", "enable", &profile_id].map(str::to_owned).to_vec());
        }
        for argv in commands {
            if system.run(&argv).map_or(true, |output| output.code != 0) {
                return Err(AppError::new(
                    "Could not configure the existing desktop Herdr profile; guest service remains configured",
                ));
            }
        }
        observer.activity(
            Some(id),
            "Desktop Herdr profile configured; no new SSH authentication needed",
        );
        return Ok(());
    }
    if unattended {
        return Err(AppError::new(
            "Desktop Herdr profile is missing; guest service is configured. Retry app=herdr interactively for SSH authentication",
        ));
    }
    verify_desktop_alias(system, settings, target)?;
    let argv: Vec<String> = [
        "herdr",
        "machine",
        "add",
        &format!("{}@{}", settings.user, target.name),
        "--label",
        &target.name,
        "--remote-session",
        "default",
    ]
    .map(str::to_owned)
    .to_vec();
    observer.activity(Some(id), AUTH_NOTICE);
    let result = observer.register_desktop(&argv)?;
    if result.code != 0 {
        // Known diagnostics are classified; external output is not shown.
        let output = format!("{}{}", result.stdout, result.stderr).to_lowercase();
        if output.contains("interactive") || output.contains("approval") {
            return Err(AppError::new(format!(
                "Herdr requires an interactive decision; the guest service is configured. Run {} in a terminal, then retry app=herdr",
                shell::join(&argv)
            )));
        }
        if output.contains("permission denied") {
            return Err(AppError::new(
                "Desktop SSH authentication failed; the guest service is configured. Unlock/load your hardware key in ssh-agent and retry app=herdr",
            ));
        }
        return Err(AppError::new(
            "Desktop Herdr registration failed; the guest service is configured. Retry app=herdr to finish connecting",
        ));
    }
    Ok(())
}

/// Install Herdr if needed, its user service and linger, then the desktop.
#[allow(clippy::too_many_arguments)]
pub fn apply(
    workspace: &mut dyn Workspace,
    settings: &Settings,
    system: &dyn System,

    target: &Target,
    state: &Value,
    observer: &mut dyn Observer,
    id: &str,
    unattended: bool,
) -> Result<(String, String), AppError> {
    if !truthy(&state["installed"]) {
        observer.activity(Some(id), "Install Herdr using the official installer");
        if command(workspace, settings, INSTALL_COMMAND)?.code != 0 {
            return Err(AppError::new("Herdr installer failed; output withheld"));
        }
    }
    if command(workspace, settings, "\"$HOME/.local/bin/herdr\" --version")?.code != 0 {
        return Err(AppError::new("Herdr installation verification failed"));
    }
    observer.activity(Some(id), "Enable linger for the workspace user");
    let linger = format!(
        "loginctl --no-ask-password enable-linger {}",
        shell::quote(&settings.user)
    );
    if command(workspace, settings, &linger)?.code != 0 {
        return Err(AppError::new(format!(
            "Could not enable user linger. An administrator must run loginctl enable-linger {}; then retry app=herdr",
            settings.user
        )));
    }
    if truthy(&state["changed"]) {
        observer.activity(Some(id), "Write Herdr user service");
        let previous = state["files"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|f| f["path"] == json!(HERDR_UNIT_PATH))
            .and_then(|f| f.get("sha256").cloned())
            .unwrap_or(Value::Null);
        guest(
            workspace,
            settings,
            "managed-files-install",
            guest_args! {
                "directories": [".config/systemd/user"],
                "files": json!([{"path": HERDR_UNIT_PATH, "content": base64::engine::general_purpose::STANDARD.encode(UNIT),
                           "sha256": sha256_hex(UNIT), "expected_sha256": previous, "mode": 0o600}]),
            },
        )?;
    }
    for arguments in ["daemon-reload", "enable herdr.service"] {
        if systemctl(workspace, settings, arguments)? != 0 {
            return Err(AppError::new("Could not configure Herdr user service"));
        }
    }
    // Again just before the start: a desktop client may have started a
    // server since preflight; an unmanaged running server is left alone.
    let sessions = running_sessions(workspace, settings)?;
    let active = systemctl(workspace, settings, "is-active herdr.service")? == 0;
    let deferred = !sessions.is_empty() && !active;
    if deferred {
        observer.activity(
            Some(id),
            "Leave existing Herdr sessions running; service will start after the next VM reboot",
        );
    } else if !active {
        observer.activity(Some(id), "Start Herdr service; it updates before launching the server");
        if systemctl(workspace, settings, "start herdr.service")? != 0 {
            return Err(AppError::new("Could not start Herdr user service"));
        }
        if systemctl(workspace, settings, "is-active herdr.service")? != 0 {
            return Err(AppError::new("Herdr user service did not become active"));
        }
    }
    connect_desktop(system, settings, target, observer, id, unattended)?;
    if !truthy(&inspect(workspace, settings, system, target)?["ready"]) {
        return Err(AppError::new("Herdr setup verification failed"));
    }
    let mut detail = format!("Herdr configured; desktop label: {}", target.name);
    detail.push_str(if deferred {
        "; existing sessions preserved; service starts after VM reboot"
    } else {
        "; user service enabled and active"
    });
    Ok(("succeeded".to_owned(), detail))
}
