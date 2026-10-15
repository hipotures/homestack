//! Inspection, complete preflight and ordered execution of a setup plan
//! (Python `setup.inspect_workspace_state`, `preflight_entry`,
//! `apply_entry`, `execute_plan`), shared by the CLI and the TUI.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::backup;
use super::definitions::{Entry, Params};
use super::documents;
use super::guest::{command_environment, guest, require_tool};
use super::plan::{Plan, all_bins, application_units, build_plan, expand_config_entries, source_item, write_paths};
use super::workspace::Workspace;
use super::{Settings, Target, json_of, sha256_hex, utc_now};
use crate::AppError;
use crate::guest_args;
use crate::system::System;
use hs_install::shell;

/// Python's truthiness of a JSON value.
#[must_use]
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
        Value::Bool(true) => true,
    }
}

/// What the run tells its watcher: the CLI prints it, the TUI shows it.
pub trait Observer {
    /// An item's state: `checking`, `preflight-ready`, `blocked`,
    /// `running`, then its result.
    fn progress(&mut self, _id: &str, _state: &str) {}
    /// What is being done, for an item or the whole run.
    fn activity(&mut self, _id: Option<&str>, _message: &str) {}
    /// Run `operation` with the terminal given to it (an interactive
    /// installer, the touch of the connection); the TUI steps aside.
    fn terminal(&mut self, operation: &mut dyn FnMut()) {
        operation();
    }
}

/// An observer that ignores everything.
pub struct Quiet;

impl Observer for Quiet {}

/// A handler that is not ported yet refuses, without changing anything.
fn pending(entry: &Entry) -> AppError {
    AppError::new(format!(
        "{}: the {} setup handler is not available in this version yet",
        entry.id, entry.handler
    ))
}

struct Walk {
    digest: sha2::Sha256,
    total: u64,
}

fn read_into(path: &Path, walk: &mut Walk, count: bool) -> std::io::Result<()> {
    use sha2::Digest;
    let mut file = std::fs::File::open(path)?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        walk.digest.update(&buffer[..read]);
        if count {
            walk.total += read as u64;
        }
    }
}

/// The digest and size of a desktop file, or of a directory tree as the
/// guest program computes it (`_digest_path`): per directory, its
/// subdirectories then its files, sorted, top down.
fn digest_local_path(path: &Path) -> Result<(String, u64), AppError> {
    use sha2::Digest;
    let mut walk = Walk {
        digest: sha2::Sha256::new(),
        total: 0,
    };
    let failed = |_| AppError::new("Local transport or file operation failed; output withheld");
    if path.is_file() {
        read_into(path, &mut walk, false).map_err(failed)?;
        let size = path.metadata().map_err(failed)?.len();
        return Ok((hex(walk.digest), size));
    }
    if !path.is_dir() {
        return Err(AppError::new(format!(
            "Managed source path is neither a file nor directory: {}",
            path.display()
        )));
    }
    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut directories = Vec::new();
        let mut files = Vec::new();
        // An unreadable directory is skipped, as os.walk does.
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let child = entry.path();
            if child.is_dir() {
                directories.push(child);
            } else {
                files.push(child);
            }
        }
        directories.sort();
        files.sort();
        let relative = |child: &Path| child.strip_prefix(path).unwrap_or(child).to_string_lossy().into_owned();
        for child in &directories {
            if child.is_symlink() {
                return Err(AppError::new(format!(
                    "Managed source directory contains a symlink: {}",
                    child.display()
                )));
            }
            walk.digest.update(format!("D\0{}\0", relative(child)).as_bytes());
        }
        for child in &files {
            let meta = child.symlink_metadata().map_err(failed)?;
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(AppError::new(format!(
                    "Managed source directory contains a symlink or special file: {}",
                    child.display()
                )));
            }
            walk.digest.update(format!("F\0{}\0", relative(child)).as_bytes());
            read_into(child, &mut walk, true).map_err(failed)?;
        }
        // Visited in order: the last pushed is popped first.
        pending.extend(directories.into_iter().rev());
    }
    Ok((hex(walk.digest), walk.total))
}

fn hex(digest: sha2::Sha256) -> String {
    use sha2::Digest;
    use std::fmt::Write;
    let mut out = String::with_capacity(64);
    for byte in digest.finalize().iter() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// What the desktop has at `path` (Python `local_metadata`).
pub fn local_metadata(path: &Path) -> Result<Value, AppError> {
    let (digest, size) = digest_local_path(path)?;
    let info = path
        .metadata()
        .map_err(|_| AppError::new("Local transport or file operation failed; output withheld"))?;
    let mtime_ns = i128::from(info.mtime()) * 1_000_000_000 + i128::from(info.mtime_nsec());
    Ok(json!({
        "exists": true,
        "type": if path.is_dir() { "directory" } else { "file" },
        "sha256": digest, "size": size, "mtime_ns": i64::try_from(mtime_ns).unwrap_or(i64::MAX),
    }))
}

fn remote_metadata(
    workspace: &mut dyn Workspace,
    settings: &Settings,
    paths: &[String],
) -> Result<HashMap<String, Value>, AppError> {
    if paths.is_empty() {
        return Ok(HashMap::new());
    }
    let mut unique: Vec<String> = Vec::new();
    for path in paths {
        if !unique.contains(path) {
            unique.push(path.clone());
        }
    }
    let result = guest(workspace, settings, "metadata", guest_args! {"paths": unique})?;
    Ok(result["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["path"].as_str().map(|path| (path.to_owned(), item.clone())))
        .collect())
}

/// The registered files that are not as the last apply left them.
#[must_use]
pub fn changed_paths_since_apply(record: &Value, current: &HashMap<String, Value>) -> Vec<String> {
    let mut changed = Vec::new();
    for registered in record["files"].as_array().into_iter().flatten() {
        let Some(path) = registered["path"].as_str() else {
            continue;
        };
        let missing = json!({"path": path, "exists": false});
        let now = current.get(path).unwrap_or(&missing);
        let exists = truthy(&now["exists"]);
        if exists != truthy(&registered["exists"]) || (exists && now["sha256"] != registered["sha256"]) {
            changed.push(path.to_owned());
        }
    }
    changed
}

/// The identity check every inspection and run starts with.
fn verify_identity(workspace: &mut dyn Workspace, settings: &Settings, target: &Target) -> Result<(), AppError> {
    guest(
        workspace,
        settings,
        "identity",
        guest_args! {"user": settings.user, "uid": settings.uid, "gid": settings.gid,
        "name": target.name, "vmid": target.vmid, "alien": target.alien},
    )?;
    Ok(())
}

/// One configuration document read for its declared leaves (Python
/// `inspect_config_file`): its candidate (merge only; never shown or
/// stored), its digest, and each leaf's state and, on inspection, value.
pub fn inspect_config_file(
    workspace: &mut dyn Workspace,
    settings: &Settings,
    entries: &[&Entry],
    inspection: bool,
) -> Result<Value, AppError> {
    use base64::Engine;
    let Some(Params::Structured { config, .. }) = entries.first().map(|entry| &entry.params) else {
        return Err(AppError::new("Unknown setup action handler"));
    };
    let relative = config.path.get(2..).unwrap_or_default();
    let remote = guest(
        workspace,
        settings,
        "structured-read",
        guest_args! {"relative": relative},
    )?;
    let original = match &remote["content"] {
        Value::Null => None,
        content => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(content.as_str().unwrap_or_default())
                .map_err(|_| AppError::new(format!("Configuration cannot be decoded: {}", config.path)))?;
            Some(
                String::from_utf8(bytes)
                    .map_err(|_| AppError::new(format!("Configuration cannot be decoded: {}", config.path)))?,
            )
        }
    };
    let leaves: Vec<(Vec<String>, toml::Value)> = entries
        .iter()
        .filter_map(|entry| match &entry.params {
            Params::Structured { key, value, .. } => Some((key.clone(), value.clone())),
            _ => None,
        })
        .collect();
    let mut current = Map::new();
    let (candidate, statuses) = if inspection {
        let inspected = documents::inspect_document_values(&config.format, original.as_deref(), &leaves)?;
        for (entry, (state, value)) in entries.iter().zip(&inspected) {
            if state == "matching" || state == "different" {
                current.insert(
                    entry.id.clone(),
                    value.as_ref().map_or(Value::Null, documents::Plain::json),
                );
            }
        }
        (
            Value::Null,
            inspected.into_iter().map(|(state, _)| state).collect::<Vec<_>>(),
        )
    } else {
        let (candidate, states) = documents::merge_document(&config.format, original.as_deref(), &leaves)?;
        (Value::String(candidate), states)
    };
    let leaf_states: Map<String, Value> = entries
        .iter()
        .zip(&statuses)
        .map(|(entry, state)| (entry.id.clone(), json!(state)))
        .collect();
    Ok(json!({
        "candidate": candidate,
        "sha256": remote.get("sha256").cloned().unwrap_or(Value::Null),
        "changed": statuses.iter().any(|state| state != "matching"),
        "leaves": leaf_states,
        "current": current,
    }))
}

/// Run an application's validator after its changes (Python
/// `validate_application_config`); no output of it is ever shown.
pub fn validate_application_config(
    workspace: &mut dyn Workspace,
    settings: &Settings,
    application: &Entry,
) -> Result<(), AppError> {
    let Some(p) = application.application() else {
        return Ok(());
    };
    let Some(validator) = &p.validation else { return Ok(()) };
    let label = if validator.path.len() > 1 {
        validator.path[..validator.path.len() - 1].join(".")
    } else {
        validator.path.join(".")
    };
    let prefix = format!("{} configuration validation failed: ", application.label);
    let command = command_environment(settings, &validator.command, &p.interpreter, &p.bin_dirs, true);
    let result = match workspace.run(&command, None, false) {
        Ok(result) => result,
        Err(_) => return Err(AppError::new(format!("{prefix}validator unavailable"))),
    };
    if validator.kind == "exit-code" {
        if result.code != 0 {
            return Err(AppError::new(format!(
                "{prefix}validator exited with status {}",
                result.code
            )));
        }
        return Ok(());
    }
    if [126, 127, 255].contains(&result.code) {
        return Err(AppError::new(format!("{prefix}validator unavailable")));
    }
    let mut value: Value =
        serde_json::from_str(&result.stdout).map_err(|_| AppError::new(format!("{prefix}malformed JSON")))?;
    for key in &validator.path {
        let Some(child) = value.as_object().and_then(|map| map.get(key)) else {
            return Err(AppError::new(format!("{prefix}missing status path")));
        };
        value = child.clone();
    }
    match value.as_str() {
        Some(status) if validator.accepted.iter().any(|accepted| accepted == status) => Ok(()),
        // Never echo arbitrary remote strings: a report can hold credentials.
        Some(status @ ("ok" | "warning" | "fail")) => Err(AppError::new(format!("{prefix}{label}={status}"))),
        _ => Err(AppError::new(format!("{prefix}{label}=unaccepted status"))),
    }
}

/// Read the setup state of the workspace without changing it (`setup
/// status`, and what the TUI shows).
pub fn inspect_workspace_state(
    workspace: &mut dyn Workspace,
    settings: &Settings,
    target: &Target,
    entries: &[Entry],
) -> Result<Value, AppError> {
    let entries = expand_config_entries(entries);
    require_tool(workspace, settings, "python3", &[])?;
    verify_identity(workspace, settings, target)?;
    let state_doc = guest(
        workspace,
        settings,
        "state-read",
        guest_args! {"vmid": target.vmid, "name": target.name},
    )?;
    let registry = state_doc
        .get("registry")
        .filter(|r| truthy(r))
        .cloned()
        .unwrap_or(json!({}));
    let registered = if registry.is_object() {
        registry["items"].clone()
    } else {
        json!({})
    };
    let mut metadata_paths = Vec::new();
    let mut repositories: Vec<String> = Vec::new();
    for entry in &entries {
        match &entry.params {
            Params::File { .. } | Params::Environment { .. } => metadata_paths.extend(write_paths(settings, entry)),
            Params::Application(p) => metadata_paths.extend(
                p.backup_paths
                    .iter()
                    .map(|path| path.get(2..).unwrap_or_default().trim_end_matches('/').to_owned()),
            ),
            Params::Repository { repository } => repositories.push(repository.clone()),
            _ => {}
        }
    }
    let remote = remote_metadata(workspace, settings, &metadata_paths)?;
    let mut documents: HashMap<String, Result<Value, AppError>> = HashMap::new();
    let mut items = Map::new();
    for entry in &entries {
        let record = registered
            .get(&entry.id)
            .filter(|record| record.is_object())
            .cloned()
            .unwrap_or(json!({}));
        let mut item = json!({
            "id": entry.id, "group": entry.group, "label": entry.label, "handler": entry.handler,
            "state": "unknown", "ready": false, "exists": false, "will_overwrite": false,
            "managed": truthy(&record),
            "first_managed_at": record.get("first_managed_at").cloned().unwrap_or(Value::Null),
            "last_applied_at": record.get("last_applied_at").cloned().unwrap_or(Value::Null),
            "installed_at": record.get("installed_at").cloned().unwrap_or(Value::Null),
            "last_snapshot": record.get("last_snapshot").cloned().unwrap_or(Value::Null),
            "files": [], "detail": "",
        });
        if let Params::Structured {
            application,
            config,
            key,
            value,
        } = &entry.params
        {
            item["key"] = json!(key);
            item["desired"] = json_of(value);
            item["config_file"] = json!(config.path);
            item["application"] = json!(application);
            // One read per document for all its leaves.
            if !documents.contains_key(&config.path) {
                let siblings: Vec<&Entry> = entries
                    .iter()
                    .filter(|e| matches!(&e.params, Params::Structured { config: c, .. } if c.path == config.path))
                    .collect();
                let document = inspect_config_file(workspace, settings, &siblings, true);
                documents.insert(config.path.clone(), document);
            }
        }
        let outcome = match &entry.params {
            Params::Structured { config, .. } => match &documents[&config.path] {
                Ok(document) => {
                    let state = document["leaves"][&entry.id]
                        .as_str()
                        .unwrap_or("unavailable")
                        .to_owned();
                    if let Some(current) = document["current"].get(&entry.id) {
                        item["current"] = current.clone();
                    }
                    let exists = !document["sha256"].is_null();
                    item["state"] = json!(state);
                    item["ready"] = json!(state == "matching");
                    item["exists"] = json!(exists);
                    item["will_overwrite"] = json!(exists && state != "matching");
                    Ok(())
                }
                Err(error) => Err(error.clone()),
            },
            _ => inspect_item(workspace, settings, target, entry, &record, &remote, &mut item),
        };
        if let Err(error) = outcome {
            item["state"] = json!("unavailable");
            item["detail"] = json!(error.0);
        }
        items.insert(entry.id.clone(), item);
    }
    Ok(json!({
        "ok": true,
        "checked_at": utc_now(),
        "state_path": state_doc.get("state_path").cloned().unwrap_or(json!("~/.local/state/homestack/setup.json")),
        "registry_present": truthy(&registry),
        "items": items,
    }))
}

fn inspect_item(
    workspace: &mut dyn Workspace,
    settings: &Settings,
    target: &Target,
    entry: &Entry,
    record: &Value,
    remote: &HashMap<String, Value>,
    item: &mut Value,
) -> Result<(), AppError> {
    match &entry.params {
        Params::File { .. } => {
            let source = source_item(settings, entry)?;
            let local = local_metadata(Path::new(&source.local_path))?;
            let found = remote
                .get(&source.relative)
                .cloned()
                .unwrap_or(json!({"path": source.relative, "exists": false}));
            let exists = truthy(&found["exists"]);
            let ready = exists && found["type"] == local["type"] && found["sha256"] == local["sha256"];
            item["files"] = json!([found]);
            item["exists"] = json!(exists);
            item["ready"] = json!(ready);
            item["will_overwrite"] = json!(exists && !ready);
            item["state"] = json!(if ready {
                "in sync"
            } else if exists {
                "modified"
            } else {
                "absent"
            });
            item["source"] = local;
        }
        Params::Environment { profile } => {
            let test = format!("command -v {} >/dev/null", shell::quote(profile));
            let shell_check = workspace.run(&command_environment(settings, &test, "sh", &[], false), None, false)?;
            let paths = write_paths(settings, entry);
            let existing: Vec<Value> = paths
                .iter()
                .filter_map(|path| remote.get(path).filter(|m| truthy(&m["exists"])).cloned())
                .collect();
            item["exists"] = json!(!existing.is_empty());
            item["will_overwrite"] = json!(!existing.is_empty());
            item["files"] = Value::Array(existing);
            let current: HashMap<String, Value> = paths
                .iter()
                .filter_map(|path| remote.get(path).map(|m| (path.clone(), m.clone())))
                .collect();
            let changed_since_apply = changed_paths_since_apply(record, &current);
            item["changed_since_apply"] = json!(changed_since_apply);
            if shell_check.code != 0 {
                item["state"] = json!("shell missing");
                item["detail"] = json!(format!("Missing workspace executable {profile}"));
            } else {
                let desired = guest(
                    workspace,
                    settings,
                    "environment",
                    guest_args! {"profile": profile, "bins": all_bins(settings)},
                )?;
                let changed: Vec<String> = desired["changed"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|path| path.as_str().map(str::to_owned))
                    .collect();
                let ready = changed.is_empty();
                let overwrite = changed
                    .iter()
                    .any(|path| remote.get(path).is_some_and(|m| truthy(&m["exists"])));
                item["ready"] = json!(ready);
                item["will_overwrite"] = json!(overwrite);
                let mut state = if ready {
                    "configured"
                } else if overwrite {
                    "needs update"
                } else {
                    "not configured"
                };
                if ready && !changed_since_apply.is_empty() {
                    state = "modified";
                }
                item["state"] = json!(state);
                item["managed_paths"] = desired.get("paths").cloned().unwrap_or(json!([]));
            }
        }
        Params::Application(p) => {
            if !p.check.is_empty() {
                let checked = workspace.run(
                    &command_environment(settings, &p.check, &p.interpreter, &p.bin_dirs, true),
                    None,
                    false,
                )?;
                if checked.code == 255 {
                    return Err(AppError::new(
                        "Workspace SSH transport failed during application inspection",
                    ));
                }
                let ready = checked.code == 0;
                item["ready"] = json!(ready);
                item["exists"] = json!(ready);
                item["state"] = json!(if ready { "installed" } else { "not installed" });
            } else if truthy(record) {
                item["state"] = json!("managed; verification unavailable");
            } else {
                item["state"] = json!("unknown; no installation check");
            }
            let backup_files: Vec<Value> = p
                .backup_paths
                .iter()
                .filter_map(|path| {
                    remote
                        .get(path.get(2..).unwrap_or_default().trim_end_matches('/'))
                        .filter(|m| truthy(&m["exists"]))
                        .cloned()
                })
                .collect();
            item["will_overwrite"] = json!(truthy(&item["ready"]) || !backup_files.is_empty());
            item["files"] = Value::Array(backup_files);
            if !p.backup_paths.is_empty() {
                item["backup_paths"] = json!(p.backup_paths);
            }
        }
        Params::Backup => {
            if let (Some(item), Value::Object(found)) = (
                item.as_object_mut(),
                backup::inspect(workspace, settings, target.vmid, true)?,
            ) {
                item.extend(found);
            }
        }
        _ => return Err(pending(entry)),
    }
    Ok(())
}

/// The paths the operation snapshot keeps before `entry` writes.
#[must_use]
pub fn backup_paths_for_entry(settings: &Settings, entry: &Entry, state: &Value) -> Vec<String> {
    let strings = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|path| path.as_str().map(str::to_owned))
            .collect()
    };
    match &entry.params {
        Params::Structured { .. } => {
            if truthy(&state["changed"]) {
                write_paths(settings, entry)
            } else {
                Vec::new()
            }
        }
        Params::File { .. } => {
            if state.get("changed").is_none_or(truthy) {
                write_paths(settings, entry)
            } else {
                Vec::new()
            }
        }
        Params::Environment { .. } => strings(&state["changed"]),
        Params::Application(p) => p
            .backup_paths
            .iter()
            .map(|path| path.get(2..).unwrap_or_default().trim_end_matches('/').to_owned())
            .collect(),
        Params::Backup | Params::Herdr => strings(&state["snapshot_paths"]),
        Params::Repository { .. } => {
            let repository = &state["repository"];
            let actions = strings(&state["actions"]);
            if repository["checkout_state"] == json!("ready")
                && actions
                    .iter()
                    .any(|action| action == "set-origin" || action == "set-ssh-command")
            {
                let prefix = format!("/home/{}/", settings.user);
                let checkout = repository["checkout"].as_str().unwrap_or_default();
                let checkout = checkout.strip_prefix(&prefix).unwrap_or(checkout);
                vec![format!("{}/.git/config", checkout.trim_end_matches('/'))]
            } else {
                Vec::new()
            }
        }
    }
}

fn record_entry_state(
    workspace: &mut dyn Workspace,
    settings: &Settings,
    plan: &Plan,
    entry: &Entry,
    state: &Value,
    snapshot_id: &Value,
) -> Result<Value, AppError> {
    let paths: Vec<String> = match &entry.params {
        Params::File { .. } => write_paths(settings, entry),
        Params::Environment { .. } => state["paths"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|path| path.as_str().map(str::to_owned))
            .collect(),
        Params::Backup => {
            let mut paths: Vec<String> = super::BACKUP_PATHS.iter().map(|p| (*p).to_owned()).collect();
            if !plan.target.local {
                paths.push(super::AUTHORIZED_KEYS_PATH.to_owned());
            }
            paths
        }
        Params::Herdr => vec![super::HERDR_UNIT_PATH.to_owned()],
        _ => Vec::new(),
    };
    let mut values = guest_args! {
        "vmid": plan.target.vmid, "name": plan.target.name, "id": entry.id,
        "handler": entry.handler, "paths": paths, "snapshot": snapshot_id, "alien": plan.target.alien,
    };
    if matches!(entry.params, Params::Application(_) | Params::Herdr) {
        values.insert("installed".into(), json!(!truthy(&state["installed"])));
    }
    if let Params::Repository { repository } = &entry.params {
        values.insert("repository".into(), json!(repository));
    }
    guest(workspace, settings, "state-record", values)
}

fn preflight_entry(
    workspace: &mut dyn Workspace,
    settings: &Settings,
    target: &Target,
    system: &dyn System,
    entry: &Entry,
) -> Result<Value, AppError> {
    match &entry.params {
        Params::File { .. } => {
            if system.which("rsync").is_none() {
                return Err(AppError::new("Required desktop rsync was not found"));
            }
            let item = source_item(settings, entry)?;
            require_tool(workspace, settings, "rsync", &[])?;
            guest(
                workspace,
                settings,
                "file",
                guest_args! {"relative": item.relative, "directory": item.is_directory},
            )?;
            let remote = remote_metadata(workspace, settings, std::slice::from_ref(&item.relative))?
                .remove(&item.relative)
                .unwrap_or(json!({"exists": false}));
            let local = local_metadata(Path::new(&item.local_path))?;
            let changed =
                !(truthy(&remote["exists"]) && remote["type"] == local["type"] && remote["sha256"] == local["sha256"]);
            Ok(json!({
                "file": {"relative": item.relative, "is_directory": item.is_directory,
                         "local_path": item.local_path, "destination": item.destination},
                "remote": remote, "local": local, "changed": changed,
            }))
        }
        Params::Environment { profile } => {
            require_tool(workspace, settings, profile, &[])?;
            guest(
                workspace,
                settings,
                "environment",
                guest_args! {"profile": profile, "bins": all_bins(settings)},
            )
        }
        Params::Application(p) => {
            require_tool(workspace, settings, &p.interpreter, &p.bin_dirs)?;
            let mut installed = false;
            if !p.check.is_empty() {
                let result = workspace.run(
                    &command_environment(settings, &p.check, &p.interpreter, &p.bin_dirs, true),
                    None,
                    false,
                )?;
                if result.code == 255 {
                    return Err(AppError::new(
                        "Workspace SSH transport failed during installation check",
                    ));
                }
                installed = result.code == 0;
            }
            let relative = |path: &str| path.get(2..).unwrap_or_default().to_owned();
            let mut paths: Vec<Value> = p
                .bin_dirs
                .iter()
                .map(|path| json!({"relative": relative(path), "directory": true}))
                .collect();
            for path in &p.requires_absent {
                let inner = relative(path);
                if let Some((parent, _)) = inner.rsplit_once('/') {
                    paths.push(json!({"relative": parent, "directory": true}));
                }
            }
            for path in &p.backup_paths {
                paths.push(json!({"relative": relative(path).trim_end_matches('/'), "directory": path.ends_with('/')}));
            }
            for config in &p.config_files {
                paths.push(json!({"relative": relative(&config.path)}));
            }
            guest(workspace, settings, "paths", guest_args! {"paths": paths})?;
            if !installed {
                for path in &p.requires_absent {
                    let full = shell::quote(&format!("{}/{}", settings.guest_home(), relative(path)));
                    let test = format!("test ! -e {full} && test ! -L {full}");
                    let result = workspace.run(&command_environment(settings, &test, "sh", &[], false), None, false)?;
                    if result.code != 0 {
                        return Err(AppError::new(format!(
                            "{}: existing installation state needs manual inspection; refusing to replace it",
                            entry.id
                        )));
                    }
                }
            }
            for tool in &p.prerequisites {
                require_tool(workspace, settings, tool, &p.bin_dirs)?;
            }
            for (index, command) in p.prerequisite_checks.iter().enumerate() {
                let result = workspace.run(
                    &command_environment(settings, command, &p.interpreter, &p.bin_dirs, true),
                    None,
                    false,
                )?;
                if result.code != 0 {
                    return Err(AppError::new(format!(
                        "{}: prerequisite check {} failed; inspect the recipe requirements and prepare Gold separately",
                        entry.id,
                        index + 1
                    )));
                }
            }
            Ok(json!({"installed": installed, "ready": installed}))
        }
        Params::Backup => backup::inspect(workspace, settings, target.vmid, true),
        _ => Err(pending(entry)),
    }
}

fn apply_entry(
    workspace: &mut dyn Workspace,
    settings: &Settings,
    plan: &Plan,
    entry: &Entry,
    state: &Value,
    observer: &mut dyn Observer,
) -> Result<(String, String), AppError> {
    let id = entry.id.as_str();
    let done = |status: &str, detail: &str| Ok((status.to_owned(), detail.to_owned()));
    match &entry.params {
        Params::Environment { profile } => {
            observer.activity(Some(id), &format!("Write {} shell configuration", entry.label));
            let result = guest(
                workspace,
                settings,
                "environment",
                guest_args! {"profile": profile, "bins": all_bins(settings), "apply": true},
            )?;
            if !truthy(&result["changed"]) {
                observer.activity(Some(id), "Shell configuration already matches");
                return done("already-ready", "Shell configuration already matches; no files changed");
            }
            observer.activity(Some(id), "Shell configuration written");
            done("succeeded", "Shell configuration verified")
        }
        Params::File { .. } => {
            if state.get("changed").is_some_and(|changed| !truthy(changed)) {
                observer.activity(Some(id), "Files already match the desktop source");
                return done("already-ready", "Destination already matches the desktop source");
            }
            let item = source_item(settings, entry)?;
            observer.activity(Some(id), &format!("Prepare destination ~/{}", item.relative));
            guest(
                workspace,
                settings,
                "file",
                guest_args! {"relative": item.relative, "directory": item.is_directory, "prepare": true},
            )?;
            let suffix = if item.is_directory { "/" } else { "" };
            observer.activity(Some(id), &format!("Copy to ~/{}", item.relative));
            workspace.transfer(&format!("{}{suffix}", item.local_path), &item.destination)?;
            observer.activity(Some(id), &format!("Verify destination ~/{}", item.relative));
            guest(
                workspace,
                settings,
                "file",
                guest_args! {"relative": item.relative, "directory": item.is_directory, "verify": true},
            )?;
            done("succeeded", "Transfer and destination ownership verified")
        }
        Params::Application(p) => {
            let interactive = p.interaction == "interactive" && !plan.unattended;
            let command = if plan.unattended && !p.non_interactive.is_empty() {
                &p.non_interactive
            } else {
                &p.command
            };
            let line = command_environment(settings, command, &p.interpreter, &p.bin_dirs, true);
            observer.activity(Some(id), "Run application installer");
            let result = if interactive {
                observer.activity(Some(id), "Start interactive installer");
                let mut result = Err(AppError::new("Interactive installer did not run"));
                observer.terminal(&mut || result = workspace.run(&line, None, true));
                observer.activity(Some(id), "Interactive installer finished");
                result?
            } else {
                workspace.run(&line, None, false)?
            };
            if result.code != 0 {
                return Err(AppError::new(format!(
                    "Installer failed (exit {}); output withheld",
                    result.code
                )));
            }
            if !p.check.is_empty() {
                observer.activity(Some(id), "Verify application installation");
                let check = command_environment(settings, &p.check, &p.interpreter, &p.bin_dirs, true);
                if workspace.run(&check, None, false)?.code != 0 {
                    return Err(AppError::new(
                        "Installer exited successfully but installation verification failed",
                    ));
                }
            }
            let action = if truthy(&state["installed"]) {
                "updated"
            } else {
                "installed"
            };
            if p.check.is_empty() {
                done(
                    "succeeded",
                    &format!("Application {action}; command exited successfully; no installation check is configured"),
                )
            } else {
                done(
                    "succeeded",
                    &format!("Application {action}; installation check passed; onboarding remains separate"),
                )
            }
        }
        Params::Backup => backup::apply(workspace, settings, plan.target.vmid, state, &mut |message| {
            observer.activity(Some(id), message);
        }),
        _ => Err(pending(entry)),
    }
}

/// One item's result in a run.
fn result_row(entry: &Entry) -> Value {
    json!({"id": entry.id, "label": entry.label, "group": entry.group, "status": "not-run", "detail": ""})
}

/// How a run reaches the workspace: a connection it already has, or one
/// it opens (inside `Observer::terminal`, as a touch may be needed).
pub enum Connection<'a> {
    Open(&'a mut dyn Workspace),
    Connect(&'a mut dyn FnMut() -> Result<Box<dyn Workspace>, AppError>),
}

/// Run `plan`: check every item first, keep a snapshot of what will be
/// overwritten, then apply each item in order and record it.
pub fn execute_plan(
    settings: &Settings,
    system: &dyn System,
    plan: &Plan,
    connection: Connection<'_>,
    observer: &mut dyn Observer,
) -> Value {
    let mut results: Vec<Value> = plan.entries.iter().map(result_row).collect();
    let mut current: Option<usize> = None;
    let mut preflight_complete = false;
    let mut snapshot = json!({"created": false, "id": null, "path": null});
    let mut public = plan.public();
    let outcome = run_plan(
        settings,
        system,
        plan,
        connection,
        observer,
        &mut results,
        &mut current,
        &mut preflight_complete,
        &mut snapshot,
        &mut public,
    );
    if let Err(error) = outcome {
        let status = if preflight_complete { "failed" } else { "blocked" };
        match current {
            Some(index) => {
                results[index]["status"] = json!(status);
                results[index]["detail"] = json!(error.0);
            }
            None => {
                for item in &mut results {
                    item["status"] = json!("blocked");
                    item["detail"] = json!(error.0);
                }
            }
        }
        observer.activity(None, "Setup execution stopped");
    }
    let ok = results
        .iter()
        .all(|r| r["status"] == json!("succeeded") || r["status"] == json!("already-ready"));
    json!({"ok": ok, "plan": public, "results": results, "snapshot": snapshot})
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_plan(
    settings: &Settings,
    system: &dyn System,
    plan: &Plan,
    connection: Connection<'_>,
    observer: &mut dyn Observer,
    results: &mut Vec<Value>,
    current: &mut Option<usize>,
    preflight_complete: &mut bool,
    snapshot: &mut Value,
    public: &mut Value,
) -> Result<(), AppError> {
    let plan = build_plan(
        settings,
        &plan.target,
        &plan.entries,
        plan.catalog_id.clone(),
        plan.unattended,
        false,
    )?;
    *public = plan.public();
    let by_id: Vec<Value> = plan
        .entries
        .iter()
        .map(|entry| {
            results
                .iter()
                .find(|r| r["id"] == json!(entry.id))
                .cloned()
                .unwrap_or_else(|| result_row(entry))
        })
        .collect();
    *results = by_id;
    observer.activity(
        None,
        if plan.target.local {
            "Verify local identity"
        } else {
            "Verify workspace connection"
        },
    );
    let mut owned: Option<Box<dyn Workspace>> = None;
    let workspace: &mut dyn Workspace = match connection {
        Connection::Open(workspace) => workspace,
        Connection::Connect(connect) => {
            let mut opened = Err(AppError::new("The workspace connection was not opened"));
            observer.terminal(&mut || opened = connect());
            owned.insert(opened?).as_mut()
        }
    };
    require_tool(workspace, settings, "python3", &[])?;
    verify_identity(workspace, settings, &plan.target)?;
    observer.activity(
        None,
        if plan.target.local {
            "Local identity verified"
        } else {
            "Workspace identity verified"
        },
    );
    let mut states: HashMap<String, Value> = HashMap::new();
    // The leaves of each document, read and merged once for all of them.
    let config_groups = |path: &str| -> Vec<&Entry> {
        plan.entries
            .iter()
            .filter(|e| matches!(&e.params, Params::Structured { config, .. } if config.path == path))
            .collect()
    };
    let mut documents: HashMap<String, Result<Value, AppError>> = HashMap::new();
    for (index, entry) in plan.entries.iter().enumerate() {
        observer.activity(Some(&entry.id), &format!("Preflight {}", entry.label));
        observer.progress(&entry.id, "checking");
        let state = match &entry.params {
            Params::Structured { config, .. } => {
                if !documents.contains_key(&config.path) {
                    let document = inspect_config_file(workspace, settings, &config_groups(&config.path), false);
                    documents.insert(config.path.clone(), document);
                }
                documents[&config.path].clone()
            }
            _ => preflight_entry(workspace, settings, &plan.target, system, entry),
        };
        match state {
            Ok(state) => {
                states.insert(entry.id.clone(), state);
                observer.progress(&entry.id, "preflight-ready");
            }
            Err(error) => {
                results[index]["status"] = json!("blocked");
                results[index]["detail"] = json!(error.0);
                observer.progress(&entry.id, "blocked");
            }
        }
    }
    if results.iter().any(|r| r["status"] == json!("blocked")) {
        observer.activity(None, "Setup stopped during preflight");
        return Ok(());
    }
    let mut backup_paths: Vec<String> = Vec::new();
    let selected_applications: Vec<&str> = plan
        .entries
        .iter()
        .filter(|e| e.application().is_some())
        .map(|e| e.id.as_str())
        .collect();
    for entry in &plan.entries {
        backup_paths.extend(backup_paths_for_entry(settings, entry, &states[&entry.id]));
        if let Params::Structured { application, .. } = &entry.params
            && selected_applications.contains(&application.as_str())
        {
            backup_paths.extend(write_paths(settings, entry));
        }
    }
    if !backup_paths.is_empty() {
        let mut unique: Vec<String> = Vec::new();
        for path in backup_paths {
            if !unique.contains(&path) {
                unique.push(path);
            }
        }
        observer.activity(None, "Create operation snapshot");
        let ids: Vec<&str> = plan.entries.iter().map(|e| e.id.as_str()).collect();
        *snapshot = guest(
            workspace,
            settings,
            "snapshot",
            guest_args! {"paths": unique, "items": ids, "vmid": plan.target.vmid, "name": plan.target.name},
        )?;
        let path = snapshot["path"]
            .as_str()
            .filter(|p| !p.is_empty())
            .unwrap_or("unknown")
            .to_owned();
        observer.activity(None, &format!("Operation snapshot created: {path}"));
    }
    *preflight_complete = true;
    let position = |id: &str, results: &[Value]| results.iter().position(|r| r["id"] == json!(id)).unwrap_or(0);
    // Every configured application, the plan's own versions first.
    let mut applications: Vec<&Entry> = plan.entries.iter().filter(|e| e.application().is_some()).collect();
    applications.extend(
        settings
            .setup
            .items
            .iter()
            .filter(|e| e.application().is_some() && !plan.entries.iter().any(|p| p.id == e.id)),
    );
    for (owner, entries) in application_units(&plan.entries) {
        // Documents written in this unit and the digest written.
        let mut applied_configs: Vec<(String, String)> = Vec::new();
        let mut completed_files: HashMap<String, Value> = HashMap::new();
        let unit: Result<(), AppError> = (|| {
            for entry in &entries {
                let index = position(&entry.id, results);
                *current = Some(index);
                observer.activity(Some(&entry.id), &format!("Apply {}", entry.label));
                observer.progress(&entry.id, "running");
                let (status, detail) = match &entry.params {
                    Params::Structured { config, .. } => {
                        let path = config.path.clone();
                        if !completed_files.contains_key(&path) {
                            // Installers may edit the document: merge against
                            // its final content.
                            let document = inspect_config_file(workspace, settings, &config_groups(&path), false)?;
                            let preflight_digest = documents[&path].as_ref().map(|d| d["sha256"].clone()).ok();
                            if !selected_applications.contains(&owner.as_str())
                                && Some(&document["sha256"]) != preflight_digest.as_ref()
                            {
                                return Err(AppError::new(format!(
                                    "Configuration changed since preflight: {path}; rerun setup"
                                )));
                            }
                            if truthy(&document["changed"]) {
                                use base64::Engine;
                                let candidate = document["candidate"].as_str().unwrap_or_default();
                                guest(
                                    workspace,
                                    settings,
                                    "structured-write",
                                    guest_args! {
                                        "relative": path.get(2..).unwrap_or_default(),
                                        "content": base64::engine::general_purpose::STANDARD.encode(candidate.as_bytes()),
                                        "expected_sha256": document["sha256"],
                                    },
                                )?;
                                applied_configs.push((path.clone(), sha256_hex(candidate.as_bytes())));
                            }
                            completed_files.insert(path.clone(), document);
                        }
                        let leaf = completed_files[&path]["leaves"][&entry.id].as_str().unwrap_or_default();
                        if leaf == "matching" {
                            ("already-ready".to_owned(), "Managed value already matches".to_owned())
                        } else {
                            ("succeeded".to_owned(), "Managed value patched".to_owned())
                        }
                    }
                    _ => apply_entry(workspace, settings, &plan, entry, &states[&entry.id], observer)?,
                };
                results[index]["status"] = json!(status);
                results[index]["detail"] = json!(detail);
            }
            let application = applications.iter().find(|e| e.id == owner);
            if let Some(application) = application
                && application.application().is_some_and(|a| a.validation.is_some())
                && (selected_applications.contains(&owner.as_str()) || !applied_configs.is_empty())
            {
                *current = Some(position(&entries[0].id, results));
                observer.activity(Some(&owner), "Validate final application configuration");
                validate_application_config(workspace, settings, application)?;
            }
            Ok(())
        })();
        if let Err(error) = unit {
            let mut restore_errors = Vec::new();
            for (path, digest) in applied_configs.iter().rev() {
                let existed = documents[path].as_ref().is_ok_and(|d| !d["sha256"].is_null());
                let restored = guest(
                    workspace,
                    settings,
                    "structured-restore",
                    guest_args! {
                        "relative": path.get(2..).unwrap_or_default(), "snapshot": snapshot["id"],
                        "expected_sha256": digest, "existed": existed,
                    },
                );
                match restored {
                    Ok(_) => observer.activity(None, &format!("Restored configuration {path}")),
                    Err(_) => restore_errors.push(path.clone()),
                }
            }
            for entry in &entries {
                let index = position(&entry.id, results);
                if results[index]["status"] == json!("succeeded") || results[index]["status"] == json!("already-ready")
                {
                    results[index]["status"] = json!("failed");
                    results[index]["detail"] =
                        json!("Final validation or execution failed; setup state was not recorded");
                }
            }
            if !restore_errors.is_empty() {
                return Err(AppError::new(format!(
                    "Configuration restoration failed; inspect the operation snapshot: {}",
                    restore_errors.join(", ")
                )));
            }
            return Err(error);
        }
        for entry in &entries {
            let index = position(&entry.id, results);
            *current = Some(index);
            observer.activity(Some(&entry.id), "Record setup state");
            record_entry_state(workspace, settings, &plan, entry, &states[&entry.id], &snapshot["id"])?;
            let status = results[index]["status"].as_str().unwrap_or_default().to_owned();
            observer.progress(&entry.id, &status);
        }
    }
    observer.activity(None, "Setup execution complete");
    Ok(())
}
