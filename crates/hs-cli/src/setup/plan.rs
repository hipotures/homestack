//! The plan of a setup (Python `setup.build_plan` and its helpers): the
//! selected entries with their generated children, ordered by handler and
//! dependency, every desktop source checked and no two writes overlapping.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::definitions::{Entry, Params, application_children};
use super::files::{FileItem, file_plan_item};
use super::text::repository;
use super::{BACKUP_PATHS, HERDR_UNIT_PATH, Settings, Target, json_of};
use crate::AppError;

/// Credentials are copied last, after everything that may need them.
pub const CREDENTIALS_GROUP: &str = "cred";

fn handler_order(handler: &str) -> u8 {
    match handler {
        "environment" => 0,
        "file" => 1,
        "application" | "herdr" => 2,
        "structured" => 3,
        "repository" => 4,
        _ => 5,
    }
}

/// The phase an entry runs in.
#[must_use]
pub fn execution_order(entry: &Entry) -> u8 {
    if entry.group == CREDENTIALS_GROUP {
        6
    } else {
        handler_order(&entry.handler)
    }
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub target: Target,
    pub entries: Vec<Entry>,
    pub catalog_id: Option<String>,
    pub unattended: bool,
}

impl Plan {
    /// The plan as `--json` and the confirmation show it.
    #[must_use]
    pub fn public(&self) -> Value {
        let actions: Vec<Value> = self.entries.iter().map(|entry| self.describe(entry)).collect();
        json!({"command": "setup", "target": self.target.json(), "catalog": self.catalog_id,
               "remote_state": "unknown", "non_interactive": self.unattended, "actions": actions})
    }

    fn describe(&self, entry: &Entry) -> Value {
        let mut action = json!({"id": entry.id, "group": entry.group, "label": entry.label,
                                "description": entry.description, "handler": entry.handler,
                                "depends_on": entry.depends_on, "remote_state": "unknown"});
        let extra = match &entry.params {
            Params::File { path, .. } => json!({
                "source": path,
                "destination": format!("{}/{}", self.target.home, path.get(2..).unwrap_or_default()),
            }),
            Params::Environment { profile } => json!({"profile": profile}),
            Params::Exports { exports } => json!({"exports": exports}),
            Params::Repository { repository } => json!({"repository": repository}),
            Params::Backup => {
                let mut extra = json!({
                    "destinations": BACKUP_PATHS.iter().map(|path| format!("~/{path}")).collect::<Vec<_>>(),
                    "timer": "backup.timer",
                    "create_configuration_if_missing": "~/.config/bk/backup.yaml",
                });
                if !self.target.local {
                    extra["restricted_authorized_keys"] = json!("~/.ssh/authorized_keys");
                }
                extra
            }
            Params::Herdr => json!({
                "destinations": [format!("~/{HERDR_UNIT_PATH}")],
                "service": "herdr.service", "update": "before every service start",
                "desktop_label": self.target.name,
                "desktop_ssh_target": format!("{}@{}", self.target.user, self.target.name),
                "security_key_touch": "Desktop registration may require separate SSH authentication",
            }),
            Params::Application(p) => json!({
                "interpreter": p.interpreter, "interaction": p.interaction,
                "prerequisites": p.prerequisites, "bin_dirs": p.bin_dirs,
                "backup_paths": p.backup_paths,
                "installation_check": if p.check.is_empty() { "unknown" } else { "configured" },
                "validation": p.validation.as_ref().map(|v| json!({"type": v.kind, "path": v.path, "accepted": v.accepted})),
            }),
            Params::Structured {
                application,
                config,
                key,
                value,
            } => json!({
                "application": application, "config_file": config.id,
                "destination": config.path, "format": config.format,
                "key": key, "desired": json_of(value),
            }),
        };
        if let (Some(action), Value::Object(extra)) = (action.as_object_mut(), extra) {
            action.extend(extra);
        }
        action
    }
}

/// `checkout`, the private and the public deploy key of `name` on the
/// workspace (Python `repo.repository_paths`).
pub fn repository_paths(settings: &Settings, name: &str) -> Result<(String, String, String), AppError> {
    let name = repository(name)?;
    let (owner, project) = name.split_once('/').unwrap_or_default();
    let home = settings.guest_home();
    let root = settings
        .repo
        .checkout_root
        .get(2..)
        .unwrap_or_default()
        .trim_end_matches('/');
    let checkout = normalized(&format!("{home}/{root}/{project}"));
    let key = format!("{home}/.ssh/homestack/github/{owner}-{project}");
    Ok((checkout, key.clone(), format!("{key}.pub")))
}

/// `PurePosixPath`'s spelling: no empty or `.` parts.
fn normalized(path: &str) -> String {
    let parts: Vec<&str> = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    format!("/{}", parts.join("/"))
}

/// The home paths `entry` writes, relative to the home.
#[must_use]
pub fn write_paths(settings: &Settings, entry: &Entry) -> Vec<String> {
    match &entry.params {
        Params::Structured { config, .. } => vec![config.path.get(2..).unwrap_or_default().to_owned()],
        Params::File { path, .. } => vec![path.get(2..).unwrap_or_default().trim_end_matches('/').to_owned()],
        Params::Exports { .. } => vec![".bashrc".to_owned()],
        Params::Environment { profile } => match profile.as_str() {
            "bash" => vec![".bashrc", ".profile", ".bash_profile", ".bash_login"],
            "zsh" => vec![".zshenv", ".zshrc"],
            "fish" => vec![".config/fish/conf.d/homestack.fish"],
            _ => vec![".config/nushell/env.nu", ".config/nushell/config.nu"],
        }
        .into_iter()
        .map(str::to_owned)
        .collect(),
        Params::Backup => BACKUP_PATHS
            .iter()
            .chain(&[super::AUTHORIZED_KEYS_PATH, ".config/bk/backup.yaml"])
            .map(|path| (*path).to_owned())
            .collect(),
        Params::Herdr => vec![HERDR_UNIT_PATH.to_owned()],
        Params::Repository { repository } => {
            let prefix = format!("{}/", settings.guest_home());
            repository_paths(settings, repository).map_or_else(
                |_| Vec::new(),
                |(checkout, private, public)| {
                    [checkout, private, public]
                        .iter()
                        .map(|path| path.strip_prefix(&prefix).unwrap_or(path).to_owned())
                        .collect()
                },
            )
        }
        Params::Application(_) => Vec::new(),
    }
}

/// The selected entries with every application's generated children.
#[must_use]
pub fn expand_config_entries(entries: &[Entry]) -> Vec<Entry> {
    let mut expanded: Vec<Entry> = Vec::new();
    let put = |entry: Entry, expanded: &mut Vec<Entry>| match expanded.iter_mut().find(|e| e.id == entry.id) {
        Some(slot) => *slot = entry,
        None => expanded.push(entry),
    };
    for entry in entries {
        put(entry.clone(), &mut expanded);
        if entry.application().is_some() {
            for leaf in application_children(entry) {
                put(leaf, &mut expanded);
            }
        }
    }
    expanded
}

/// The application a generated child belongs to, else the entry itself.
#[must_use]
pub fn owner_of(entry: &Entry) -> &str {
    match &entry.params {
        Params::Structured { application, .. } => application,
        Params::File {
            application: Some(application),
            ..
        } if !application.is_empty() => application,
        _ => &entry.id,
    }
}

/// Each application's installer, values and files together, in plan
/// order; every other entry alone.
#[must_use]
pub fn application_units(entries: &[Entry]) -> Vec<(String, Vec<Entry>)> {
    let mut units: Vec<(String, Vec<Entry>)> = Vec::new();
    for entry in entries {
        let owner = owner_of(entry).to_owned();
        match units.iter_mut().find(|(known, _)| *known == owner) {
            Some((_, unit)) => unit.push(entry.clone()),
            None => units.push((owner, vec![entry.clone()])),
        }
    }
    units
}

/// The paths `bin_dirs` of every configured application, `~/.local/bin`
/// first.
#[must_use]
pub fn all_bins(settings: &Settings) -> Vec<String> {
    let mut bins = vec!["~/.local/bin".to_owned()];
    for entry in &settings.setup.items {
        if let Some(application) = entry.application() {
            for path in &application.bin_dirs {
                if !bins.contains(path) {
                    bins.push(path.clone());
                }
            }
        }
    }
    bins
}

fn readable(path: &Path, directory: bool) -> bool {
    if directory {
        std::fs::read_dir(path).is_ok()
    } else {
        std::fs::File::open(path).is_ok()
    }
}

/// A selected file's source, checked: present, of its type, no symlink on
/// the way or inside, readable.
pub fn source_item(settings: &Settings, entry: &Entry) -> Result<FileItem, AppError> {
    let Params::File { path, .. } = &entry.params else {
        return Err(AppError::new("Unknown setup action handler"));
    };
    let id = &entry.id;
    let item = file_plan_item(settings, path)?;
    if item.status != "ready" {
        return Err(AppError::new(format!(
            "Selected file {id}: {} ({})",
            item.status, item.detail
        )));
    }
    let mut current = settings.home.clone();
    for part in item.relative.split('/') {
        current.push(part);
        if current
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return Err(AppError::new(format!(
                "Selected file {id}: source ancestor is a symlink"
            )));
        }
    }
    let source = PathBuf::from(&item.local_path);
    if !readable(&source, item.is_directory) {
        return Err(AppError::new(format!("Selected file {id}: source is not readable")));
    }
    if item.is_directory {
        let mut pending = vec![source];
        while let Some(directory) = pending.pop() {
            let entries = std::fs::read_dir(&directory)
                .map_err(|_| AppError::new(format!("Selected file {id}: source tree cannot be inspected")))?;
            for child in entries {
                let child = child
                    .map_err(|_| AppError::new(format!("Selected file {id}: source tree cannot be inspected")))?
                    .path();
                let meta = child
                    .symlink_metadata()
                    .map_err(|_| AppError::new(format!("Selected file {id}: source tree cannot be inspected")))?;
                if meta.file_type().is_symlink() || !(meta.is_file() || meta.is_dir()) {
                    return Err(AppError::new(format!(
                        "Selected file {id}: source tree contains a symlink or special file"
                    )));
                }
                if !readable(&child, meta.is_dir()) {
                    return Err(AppError::new(format!(
                        "Selected file {id}: source tree is not readable"
                    )));
                }
                if meta.is_dir() {
                    pending.push(child);
                }
            }
        }
    }
    Ok(item)
}

/// Build the plan (Python `build_plan`).
pub fn build_plan(
    settings: &Settings,
    target: &Target,
    entries: &[Entry],
    catalog_id: Option<String>,
    unattended: bool,
    include_configs: bool,
) -> Result<Plan, AppError> {
    if target.local && entries.iter().any(|entry| !matches!(entry.params, Params::Backup)) {
        return Err(AppError::new("Local Setup currently supports only the Backup category"));
    }
    let entries = if include_configs {
        expand_config_entries(entries)
    } else {
        entries.to_vec()
    };
    if entries.is_empty() || (!target.local && target.vmid == 0) || target.name.is_empty() {
        return Err(AppError::new(
            "An explicit resolved workspace and at least one action are required",
        ));
    }
    if settings.uid == 0 || settings.gid == 0 || settings.user == "root" {
        return Err(AppError::new(
            "Setup requires an unprivileged configured workspace user",
        ));
    }
    let mut selected: Vec<Entry> = Vec::new();
    for entry in &entries {
        match selected.iter_mut().find(|known| known.id == entry.id) {
            Some(slot) => *slot = entry.clone(),
            None => selected.push(entry.clone()),
        }
    }
    let mut ordered: Vec<Entry> = Vec::new();
    let mut visiting: Vec<String> = Vec::new();
    let mut sorted = selected.clone();
    sorted.sort_by_key(execution_order);
    for entry in &sorted {
        visit(entry, &selected, &mut ordered, &mut visiting)?;
    }
    let ordered: Vec<Entry> = application_units(&ordered)
        .into_iter()
        .flat_map(|(_, unit)| unit)
        .collect();
    let mut writes: Vec<(String, Entry)> = Vec::new();
    for entry in &ordered {
        if let Params::Application(p) = &entry.params
            && unattended
            && p.interaction == "interactive"
            && p.non_interactive.is_empty()
        {
            return Err(AppError::new(format!(
                "{} requires an interactive terminal; configure a verified non_interactive recipe or omit it",
                entry.id
            )));
        }
        if matches!(entry.params, Params::File { .. }) {
            source_item(settings, entry)?;
        }
        for path in write_paths(settings, entry) {
            for (previous, other) in &writes {
                let overlap = path == *previous
                    || path.starts_with(&format!("{previous}/"))
                    || previous.starts_with(&format!("{path}/"));
                if !overlap {
                    continue;
                }
                let shared_document = path == *previous
                    && matches!((&entry.params, &other.params), (
                        Params::Structured { application: a, config: c, .. },
                        Params::Structured { application: b, config: d, .. },
                    ) if a == b && c == d);
                if shared_document {
                    continue;
                }
                return Err(AppError::new(format!(
                    "Overlapping selected writes: {} and {} at ~/{path}",
                    other.id, entry.id
                )));
            }
            writes.push((path, entry.clone()));
        }
    }
    Ok(Plan {
        target: target.clone(),
        entries: ordered,
        catalog_id,
        unattended,
    })
}

fn visit(
    entry: &Entry,
    selected: &[Entry],
    ordered: &mut Vec<Entry>,
    visiting: &mut Vec<String>,
) -> Result<(), AppError> {
    if ordered.iter().any(|known| known == entry) {
        return Ok(());
    }
    if visiting.contains(&entry.id) {
        return Err(AppError::new("Selected setup dependencies form a cycle"));
    }
    visiting.push(entry.id.clone());
    let owner = owner_of(entry);
    if owner != entry.id
        && let Some(application) = selected.iter().find(|known| known.id == owner)
    {
        visit(application, selected, ordered, visiting)?;
    }
    for dependency in &entry.depends_on {
        let Some(required) = selected.iter().find(|known| known.id == *dependency) else {
            return Err(AppError::new(format!(
                "{} requires explicit selection of {dependency}; no actions were added",
                entry.id
            )));
        };
        visit(required, selected, ordered, visiting)?;
    }
    visiting.retain(|id| *id != entry.id);
    ordered.push(entry.clone());
    Ok(())
}
