//! Herdr: its installer and a persistent user service (Python
//! `setup_herdr.py`). A running Herdr server is never updated, restarted or
//! stopped. No desktop machine profile is added anywhere.

use base64::Engine;
use hs_install::shell;
use serde_json::{Value, json};

use super::engine::{Observer, truthy};
use super::guest::{command_environment, guest, require_tool};
use super::workspace::Workspace;
use super::{HERDR_UNIT_PATH, Settings, sha256_hex};
use crate::AppError;
use crate::guest_args;
use crate::system::Output;

/// The user unit, byte for byte Python's.
pub const UNIT: &[u8] = include_bytes!("../../guest/herdr/herdr.service");
pub const INSTALL_COMMAND: &str = "curl -fsSL https://herdr.dev/install.sh | sh";
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

/// Herdr's state on the workspace.
pub fn inspect(workspace: &mut dyn Workspace, settings: &Settings) -> Result<Value, AppError> {
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
    let deferred = !sessions.is_empty() && !active;
    let ready = installed
        && !changed
        && enabled
        && lingering
        && sessions.iter().any(|s| s == "default")
        && (active || deferred);
    let missing: Vec<&str> = [
        (changed, "service unit differs"),
        (!enabled, "service not enabled"),
        (!lingering, "linger off"),
        (!sessions.iter().any(|s| s == "default"), "no default session"),
        (!active && !deferred, "service not running"),
    ]
    .into_iter()
    .filter_map(|(missing, what)| missing.then_some(what))
    .collect();
    let state = if ready && deferred {
        "configured; service starts after VM reboot".to_owned()
    } else if ready {
        "configured".to_owned()
    } else if installed {
        format!("needs setup: {}", missing.join(", "))
    } else {
        "not installed".to_owned()
    };
    let unit_exists = truthy(&unit["exists"]);
    Ok(json!({
        "installed": installed, "ready": ready, "exists": installed || unit_exists, "state": state,
        "files": files, "will_overwrite": changed && unit_exists, "changed": changed,
        "snapshot_paths": if changed && unit_exists { vec![HERDR_UNIT_PATH] } else { Vec::new() },
        "service_enabled": enabled, "service_active": active, "linger": lingering,
        "running_sessions": sessions,
        "detail": if deferred { "Existing Herdr server left running; the user service is enabled for the next VM boot" } else { "" },
    }))
}

/// Check everything Herdr needs before any item writes.
pub fn preflight(workspace: &mut dyn Workspace, settings: &Settings) -> Result<Value, AppError> {
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
    inspect(workspace, settings)
}

/// Install Herdr if needed, its user service and linger.
pub fn apply(
    workspace: &mut dyn Workspace,
    settings: &Settings,
    state: &Value,
    observer: &mut dyn Observer,
    id: &str,
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
        observer.activity(Some(id), "Start Herdr service using the installed binary");
        if systemctl(workspace, settings, "start herdr.service")? != 0 {
            return Err(AppError::new("Could not start Herdr user service"));
        }
        if systemctl(workspace, settings, "is-active herdr.service")? != 0 {
            return Err(AppError::new("Herdr user service did not become active"));
        }
    }
    if !truthy(&inspect(workspace, settings)?["ready"]) {
        return Err(AppError::new("Herdr setup verification failed"));
    }
    let mut detail = "Herdr configured".to_owned();
    detail.push_str(if deferred {
        "; existing sessions preserved; service starts after VM reboot"
    } else {
        "; user service enabled and active"
    });
    Ok(("succeeded".to_owned(), detail))
}
