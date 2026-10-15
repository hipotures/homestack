//! The catalog without a target and its immutable numeric snapshots
//! (Python `setup_catalog.py`): `setup list`, and the selectors
//! `group=item,item` of `setup TARGET`.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::definitions::{Entry, Params, app_file_entries, config_entries, config_entry_key};
use super::files::file_plan_item;
use super::plan::write_paths;
use super::text::{py_repr, repository};
use super::{BACKUP_PATHS, Settings, json_of, sha256_hex};
use crate::AppError;
use crate::system::System;

/// One-letter names of the groups.
pub const ALIASES: [(&str, &str); 6] = [
    ("f", "files"),
    ("e", "env"),
    ("a", "app"),
    ("c", "cred"),
    ("r", "repo"),
    ("b", "backup"),
];

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub entries: Vec<Entry>,
    pub availability: HashMap<String, String>,
    /// The GitHub account the repositories were listed with.
    pub github: Option<Value>,
    pub repository_error: String,
    pub snapshot_id: Option<String>,
    /// `created_at`, `pushed_at`, `private`, `archived` of each repository.
    pub timestamps: HashMap<String, Map<String, Value>>,
}

impl Catalog {
    /// The rows of `setup list`, by group, numbered from 1 in each.
    #[must_use]
    pub fn rows(&self, settings: &Settings) -> Vec<Value> {
        let mut rows = Vec::new();
        for group in &settings.setup.groups {
            for (index, entry) in self.entries.iter().filter(|e| e.group == group.id).enumerate() {
                let mut location = match &entry.params {
                    Params::File { path, .. } => path.clone(),
                    Params::Repository { repository } => repository.clone(),
                    _ => String::new(),
                };
                let mut config_files = Vec::new();
                match &entry.params {
                    Params::Application(application) => {
                        location = application.bin_dirs.join(", ");
                        let leaves = config_entries(entry);
                        for config in &application.config_files {
                            let selectors: Vec<Value> = leaves
                                .iter()
                                .filter(|leaf| matches!(&leaf.params, Params::Structured { config: c, .. } if c.id == config.id))
                                .map(|leaf| Value::String(leaf.id.clone()))
                                .collect();
                            config_files.push(json!({
                                "id": config.id, "path": config.path, "format": config.format,
                                "values": json_of(&toml::Value::Table(config.values.clone())),
                                "selectors": selectors,
                            }));
                        }
                    }
                    Params::Environment { .. } => {
                        location = write_paths(settings, entry)
                            .iter()
                            .map(|path| format!("~/{path}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                    }
                    Params::Backup => {
                        location = BACKUP_PATHS
                            .iter()
                            .map(|path| format!("~/{path}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                    }
                    Params::Herdr => {
                        location =
                            "~/.local/bin/herdr, ~/.config/systemd/user/herdr.service; desktop profile".to_owned();
                    }
                    _ => {}
                }
                let mut row = json!({
                    "index": index + 1, "id": entry.id, "group": group.id, "label": entry.label,
                    "path_or_repository": location,
                    "availability": self.availability.get(&entry.id).map_or("unknown", String::as_str),
                    "guest_state": "unknown",
                    "selector": format!("{}={}", group.id, entry.id),
                });
                if let (Some(object), Some(extra)) = (row.as_object_mut(), self.timestamps.get(&entry.id)) {
                    object.extend(extra.clone());
                }
                if entry.application().is_some() {
                    row["config_files"] = Value::Array(config_files);
                    row["files"] = app_file_entries(entry)
                        .iter()
                        .map(|leaf| {
                            let path = match &leaf.params {
                                Params::File { path, .. } => path.clone(),
                                _ => String::new(),
                            };
                            json!({"path": path, "selector": leaf.id,
                                   "availability": self.availability.get(&leaf.id).map_or("unknown", String::as_str)})
                        })
                        .collect();
                }
                rows.push(row);
            }
        }
        rows
    }
}

/// `gh api --hostname github.com ARGS` (Python `repo._gh`).
pub fn gh(system: &dyn System, args: &[&str]) -> Result<String, AppError> {
    if system.which("gh").is_none() {
        return Err(AppError::new("Required local command 'gh' was not found"));
    }
    let mut argv: Vec<String> = ["gh", "api", "--hostname", "github.com"].map(str::to_owned).to_vec();
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    let output = system.run(&argv).map_err(|_| {
        AppError::new(
            "Desktop GitHub request failed; check gh authentication and repository administration permissions",
        )
    })?;
    if output.code != 0 {
        return Err(AppError::new(
            "Desktop GitHub request failed; check gh authentication and repository administration permissions",
        ));
    }
    Ok(output.stdout)
}

/// `repo._github_json`: the answer, an object or an array as expected.
pub fn github_json(system: &dyn System, args: &[&str], array: bool) -> Result<Value, AppError> {
    let value: Value =
        serde_json::from_str(&gh(system, args)?).map_err(|_| AppError::new("GitHub API returned invalid JSON"))?;
    if (array && !value.is_array()) || (!array && !value.is_object()) {
        return Err(AppError::new("GitHub API returned an unexpected response"));
    }
    Ok(value)
}

/// The GitHub account of `gh` and the configured owner.
pub fn github_identity(system: &dyn System, settings: &Settings) -> Result<Value, AppError> {
    let user = github_json(system, &["user"], false)?;
    let (Some(login), Some(id)) = (
        user["login"].as_str(),
        user["id"]
            .as_i64()
            .filter(|_| user["id"].is_i64() || user["id"].is_u64()),
    ) else {
        return Err(AppError::new(
            "GitHub account identity unavailable; run gh auth login on the desktop",
        ));
    };
    Ok(json!({"host": "github.com", "account": login, "account_id": id, "owner": settings.repo.owner}))
}

/// Seconds since the epoch of an ISO 8601 time (`fromisoformat`).
fn timestamp(value: &Value) -> Result<f64, AppError> {
    let invalid = || AppError::new("GitHub returned an invalid repository timestamp");
    match value {
        Value::Null => Ok(0.0),
        Value::String(text) if text.is_empty() => Ok(0.0),
        Value::String(text) => {
            let text = text.replace('Z', "+00:00");
            let parsed = time::OffsetDateTime::parse(&text, &time::format_description::well_known::Iso8601::DEFAULT)
                .or_else(|_| {
                    time::PrimitiveDateTime::parse(&text, &time::format_description::well_known::Iso8601::DEFAULT)
                        .map(time::PrimitiveDateTime::assume_utc)
                })
                .map_err(|_| invalid())?;
            #[allow(clippy::cast_precision_loss)]
            Ok(parsed.unix_timestamp_nanos() as f64 / 1e9)
        }
        Value::Bool(false) => Ok(0.0),
        _ => Err(invalid()),
    }
}

/// The owner's repositories `gh` can see, sorted by `[repo] sort`.
pub fn discover_repositories(system: &dyn System, settings: &Settings) -> Result<(Value, Vec<Value>), AppError> {
    let Some(owner) = settings.repo.owner.as_deref().filter(|owner| !owner.is_empty()) else {
        return Err(AppError::new(
            "Repository discovery unavailable: configure [repo] owner",
        ));
    };
    let identity = github_identity(system, settings)?;
    // This authenticated endpoint includes private repositories,
    // collaborators and organizations.
    let pages = github_json(
        system,
        &[
            "--paginate",
            "--slurp",
            "user/repos?per_page=100&affiliation=owner,collaborator,organization_member&visibility=all",
        ],
        true,
    )?;
    let mut repos: Vec<(String, Value)> = Vec::new();
    for page in pages.as_array().into_iter().flatten() {
        let Some(page) = page.as_array() else {
            return Err(AppError::new("GitHub pagination returned an invalid page"));
        };
        for repo in page {
            if !repo.is_object() {
                return Err(AppError::new("GitHub returned an invalid repository"));
            }
            let name = repository(repo["full_name"].as_str().unwrap_or_default())?;
            if name.split('/').next().unwrap_or_default().to_lowercase() == owner.to_lowercase() {
                match repos.iter_mut().find(|(known, _)| *known == name) {
                    Some(slot) => slot.1 = repo.clone(),
                    None => repos.push((name, repo.clone())),
                }
            }
        }
    }
    let mut keyed = Vec::new();
    for (name, repo) in repos {
        let created = timestamp(&repo["created_at"])?;
        let recent = created.max(timestamp(&repo["pushed_at"])?);
        let primary = match settings.repo.sort.as_str() {
            "name" => 0.0,
            "created" => -created,
            _ => -recent,
        };
        keyed.push((primary, name.to_lowercase(), name, repo));
    }
    keyed.sort_by(|a, b| {
        a.0.total_cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    Ok((identity, keyed.into_iter().map(|(_, _, _, repo)| repo).collect()))
}

/// Discover repositories for the catalog; tests replace it.
pub type Discover<'a> = &'a dyn Fn(&Settings) -> Result<(Value, Vec<Value>), AppError>;

/// The catalog: the configured items, their availability, and with
/// `repositories` the owner's GitHub repositories.
pub fn load_catalog(settings: &Settings, repositories: Option<Discover<'_>>) -> Catalog {
    let mut entries = settings.setup.items.clone();
    let mut availability = HashMap::new();
    for entry in &entries {
        let state = match &entry.params {
            Params::File { path, .. } => {
                file_plan_item(settings, path).map_or_else(|_| "invalid".to_owned(), |item| item.status)
            }
            _ => "guest state unknown".to_owned(),
        };
        availability.insert(entry.id.clone(), state);
        for leaf in app_file_entries(entry) {
            if let Params::File { path, .. } = &leaf.params {
                let state = file_plan_item(settings, path).map_or_else(|_| "invalid".to_owned(), |item| item.status);
                availability.insert(leaf.id.clone(), state);
            }
        }
    }
    let mut catalog = Catalog {
        availability,
        ..Catalog::default()
    };
    if let Some(discover) = repositories {
        match discover(settings) {
            Ok((identity, repos)) => {
                catalog.github = Some(identity);
                let known: Vec<(String, String)> = entries
                    .iter()
                    .filter_map(|e| match &e.params {
                        Params::Repository { repository } => Some((repository.clone(), e.id.clone())),
                        _ => None,
                    })
                    .collect();
                for repo in &repos {
                    let name = repo["full_name"].as_str().unwrap_or_default().to_owned();
                    let item_id = known
                        .iter()
                        .find(|(repository, _)| *repository == name)
                        .map_or_else(|| name.clone(), |(_, id)| id.clone());
                    if !known.iter().any(|(repository, _)| *repository == name) {
                        entries.push(Entry {
                            id: name.clone(),
                            group: "repo".to_owned(),
                            handler: "repository".to_owned(),
                            label: name.clone(),
                            description: "Provision a Git checkout and repo-scoped deploy key; no project scripts run."
                                .to_owned(),
                            params: Params::Repository {
                                repository: name.clone(),
                            },
                            depends_on: Vec::new(),
                        });
                    }
                    let usable = repo["permissions"]["admin"].as_bool() == Some(true)
                        && !truthy(&repo["archived"])
                        && !truthy(&repo["disabled"]);
                    catalog.availability.insert(
                        item_id.clone(),
                        if usable {
                            "available".to_owned()
                        } else {
                            "unavailable: repository administration permission required or repository archived/disabled"
                                .to_owned()
                        },
                    );
                    let mut stamps = Map::new();
                    for key in ["created_at", "pushed_at", "private", "archived"] {
                        stamps.insert(key.to_owned(), repo.get(key).cloned().unwrap_or(Value::Null));
                    }
                    catalog.timestamps.insert(item_id, stamps);
                }
                let order = |entry: &Entry| match &entry.params {
                    Params::Repository { repository } => (
                        repos
                            .iter()
                            .position(|repo| repo["full_name"].as_str() == Some(repository.as_str()))
                            .unwrap_or(repos.len()),
                        repository.clone(),
                    ),
                    _ => (0, String::new()),
                };
                let is_repo = |e: &Entry| e.group == "repo" && matches!(e.params, Params::Repository { .. });
                let mut repository_entries: Vec<Entry> = entries.iter().filter(|e| is_repo(e)).cloned().collect();
                repository_entries.sort_by_key(|e| order(e));
                entries.retain(|e| !is_repo(e));
                entries.extend(repository_entries);
            }
            Err(error) => catalog.repository_error = error.0,
        }
    }
    catalog.entries = entries;
    catalog
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
        Value::Bool(true) => true,
    }
}

/// What a snapshot is bound to: the definitions of the groups and items.
#[must_use]
pub fn fingerprint(settings: &Settings) -> String {
    let items: Vec<Value> = settings
        .setup
        .items
        .iter()
        .map(|entry| json_of(&toml::Value::Table(entry.definition())))
        .collect();
    let groups: Vec<Value> = settings
        .setup
        .groups
        .iter()
        .map(|group| json!([group.id, group.label, group.description]))
        .collect();
    sha256_hex(json!({"items": items, "groups": groups}).to_string().as_bytes())
}

fn resolved(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// `$XDG_STATE_HOME/homestack/catalogs/<scope of client.toml>`.
#[must_use]
pub fn snapshot_directory(settings: &Settings) -> PathBuf {
    let scope = sha256_hex(resolved(&settings.config_path).as_bytes());
    settings.state_home.join("homestack/catalogs").join(&scope[..24])
}

/// Save the catalog's numbering; its ID.
pub fn save_snapshot(settings: &Settings, catalog: &mut Catalog) -> Result<String, AppError> {
    let failed = |error: std::io::Error| AppError::new(format!("Could not save the catalog snapshot: {error}"));
    let directory = snapshot_directory(settings);
    std::fs::create_dir_all(&directory).map_err(failed)?;
    let identifier = uuid::Uuid::new_v4().simple().to_string();
    let mut mapping = Map::new();
    for group in &settings.setup.groups {
        let ids: Vec<Value> = catalog
            .entries
            .iter()
            .filter(|e| e.group == group.id)
            .map(|e| Value::String(e.id.clone()))
            .collect();
        mapping.insert(group.id.clone(), Value::Array(ids));
    }
    let repositories: Map<String, Value> = catalog
        .entries
        .iter()
        .filter_map(|e| match &e.params {
            Params::Repository { repository } => Some((e.id.clone(), Value::String(repository.clone()))),
            _ => None,
        })
        .collect();
    // Metadata only: no commands, tokens or desktop file contents.
    let data = json!({
        "version": 1, "id": identifier, "config": resolved(&settings.config_path),
        "fingerprint": fingerprint(settings), "github": catalog.github,
        "repo": {"owner": settings.repo.owner, "checkout_root": settings.repo.checkout_root, "sort": settings.repo.sort},
        "mapping": mapping, "repositories": repositories,
    });
    let path = directory.join(format!("{identifier}.json"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(failed)?;
    file.write_all(data.to_string().as_bytes()).map_err(failed)?;
    file.sync_all().map_err(failed)?;
    let mut latest = tempfile::Builder::new()
        .prefix(".latest-")
        .tempfile_in(&directory)
        .map_err(failed)?;
    latest.write_all(identifier.as_bytes()).map_err(failed)?;
    latest.as_file().sync_all().map_err(failed)?;
    latest
        .persist(directory.join("latest"))
        .map_err(|error| failed(error.error))?;
    let _ = std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700));
    catalog.snapshot_id = Some(identifier.clone());
    Ok(identifier)
}

/// Read a snapshot (the latest without `identifier`) and check it still
/// fits this configuration (and, with `identity`, this GitHub account).
pub fn read_snapshot(
    settings: &Settings,
    identifier: Option<&str>,
    identity: Option<&dyn Fn() -> Result<Value, AppError>>,
) -> Result<Value, AppError> {
    let hint = "Run 'homestack setup list' and reuse its catalog ID or use stable IDs.";
    let directory = snapshot_directory(settings);
    let missing = || AppError::new(format!("Catalog snapshot is missing or invalid. {hint}"));
    let identifier = match identifier {
        Some(identifier) => identifier.to_owned(),
        None => std::fs::read_to_string(directory.join("latest"))
            .map_err(|_| missing())?
            .trim()
            .to_owned(),
    };
    if identifier.len() != 32
        || !identifier
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(AppError::new(format!("Invalid catalog ID. {hint}")));
    }
    let text = std::fs::read_to_string(directory.join(format!("{identifier}.json"))).map_err(|_| missing())?;
    let data: Value = serde_json::from_str(&text).map_err(|_| missing())?;
    if !data["mapping"].is_object() || !data["repositories"].is_object() {
        return Err(AppError::new(format!("Catalog snapshot is invalid. {hint}")));
    }
    let mapping_valid = data["mapping"]
        .as_object()
        .into_iter()
        .flat_map(|map| map.values())
        .all(|ids| ids.as_array().is_some_and(|ids| ids.iter().all(Value::is_string)));
    if !mapping_valid {
        return Err(AppError::new(format!("Catalog snapshot mapping is invalid. {hint}")));
    }
    if data["version"] != json!(1)
        || data["id"].as_str() != Some(identifier.as_str())
        || data["config"].as_str() != Some(resolved(&settings.config_path).as_str())
        || data["fingerprint"].as_str() != Some(fingerprint(settings).as_str())
    {
        return Err(AppError::new(format!(
            "Catalog snapshot is incompatible with this configuration. {hint}"
        )));
    }
    if let Some(identity) = identity {
        let scope = json!({"owner": settings.repo.owner, "checkout_root": settings.repo.checkout_root, "sort": settings.repo.sort});
        if data["repo"] != scope || !truthy(&data["github"]) || data["github"] != identity()? {
            return Err(AppError::new(format!(
                "Catalog repository identity is incompatible. {hint}"
            )));
        }
    }
    Ok(data)
}

/// `group=item,item` selectors by group (Python `parse_assignments`).
pub fn parse_assignments(tokens: &[String], settings: &Settings) -> Result<Vec<(String, Vec<String>)>, AppError> {
    let mut assignments: Vec<(String, Vec<String>)> = Vec::new();
    for token in tokens {
        if token.matches('=').count() != 1 {
            return Err(AppError::new(
                "Selectors must use group=item,item (files/f, env/e, app/a, cred/c, repo/r, backup/b)",
            ));
        }
        let (key, value) = token.split_once('=').unwrap_or_default();
        let key = ALIASES
            .iter()
            .find(|(alias, _)| *alias == key)
            .map_or(key, |(_, group)| *group)
            .to_owned();
        if !settings.setup.groups.iter().any(|group| group.id == key) {
            return Err(AppError::new(format!("Unknown setup selector {}", py_repr(&key))));
        }
        let mut values: Vec<String> = Vec::new();
        for item in value.split(',') {
            if !values.iter().any(|known| known == item) {
                values.push(item.to_owned());
            }
        }
        if values.iter().any(|item| item.is_empty() || item.trim() != item) {
            return Err(AppError::new("Selector contains an empty or malformed item"));
        }
        if values.iter().any(|item| item == "0" || item == "all") {
            if values.len() != 1 {
                return Err(AppError::new("0 or all must be used alone within a category"));
            }
            values = vec!["all".to_owned()];
        }
        if let Some((_, existing)) = assignments.iter().find(|(group, _)| *group == key) {
            let mut left = existing.clone();
            let mut right = values.clone();
            left.sort();
            right.sort();
            if left != right {
                return Err(AppError::new(format!("Conflicting repeated assignment for {key}")));
            }
            continue;
        }
        assignments.push((key, values));
    }
    Ok(assignments)
}

fn decimal(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
}

/// What `select_entries` needs from GitHub; tests replace it.
pub struct Github<'a> {
    pub identity: &'a dyn Fn() -> Result<Value, AppError>,
    pub catalog: &'a dyn Fn() -> Catalog,
}

/// The entries the selectors name, and the snapshot used for numbers.
pub fn select_entries(
    settings: &Settings,
    tokens: &[String],
    catalog_id: Option<&str>,
    github: &Github<'_>,
) -> Result<(Vec<Entry>, Option<String>), AppError> {
    let assignments = parse_assignments(tokens, settings)?;
    let numeric = assignments.iter().any(|(_, values)| values.iter().any(|v| decimal(v)));
    let mut entries: Vec<Entry> = settings.setup.items.clone();
    let repository_groups: Vec<String> = std::iter::once("repo".to_owned())
        .chain(
            entries
                .iter()
                .filter(|e| matches!(e.params, Params::Repository { .. }))
                .map(|e| e.group.clone()),
        )
        .collect();
    let needs_github = assignments.iter().any(|(group, _)| repository_groups.contains(group));
    let snapshot = if numeric || catalog_id.is_some() {
        Some(read_snapshot(
            settings,
            catalog_id,
            needs_github.then_some(github.identity),
        )?)
    } else {
        None
    };
    if let Some(snapshot) = &snapshot {
        for (item_id, repository_name) in snapshot["repositories"].as_object().into_iter().flatten() {
            if !entries.iter().any(|e| e.id == *item_id) {
                let name = repository(repository_name.as_str().unwrap_or_default())?;
                entries.push(Entry {
                    id: item_id.clone(),
                    group: "repo".to_owned(),
                    handler: "repository".to_owned(),
                    label: repository_name.as_str().unwrap_or_default().to_owned(),
                    description: "Provision the displayed repository identity.".to_owned(),
                    params: Params::Repository { repository: name },
                    depends_on: Vec::new(),
                });
            }
        }
    }
    let repo_all = assignments
        .iter()
        .any(|(group, values)| group == "repo" && values == &["all".to_owned()]);
    if repo_all && snapshot.is_none() {
        let catalog = (github.catalog)();
        if !catalog.repository_error.is_empty() {
            return Err(AppError::new(catalog.repository_error));
        }
        for entry in catalog.entries {
            match entries.iter_mut().find(|e| e.id == entry.id) {
                Some(slot) => *slot = entry,
                None => entries.push(entry),
            }
        }
    }
    let mut selected: Vec<Entry> = Vec::new();
    for (group, values) in &assignments {
        for value in values {
            let ids: Vec<String> = if value == "all" {
                match &snapshot {
                    Some(snapshot) => snapshot["mapping"][group]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|id| id.as_str().map(str::to_owned))
                        .collect(),
                    None => entries
                        .iter()
                        .filter(|e| e.group == *group)
                        .map(|e| e.id.clone())
                        .collect(),
                }
            } else if decimal(value) {
                let mapping: Vec<String> = snapshot
                    .as_ref()
                    .and_then(|s| s["mapping"][group].as_array().cloned())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|id| id.as_str().map(str::to_owned))
                    .collect();
                let number: usize = value.parse().unwrap_or(0);
                if number < 1 || number > mapping.len() {
                    return Err(AppError::new(format!(
                        "Index {value} is out of range for {group}; run setup list"
                    )));
                }
                vec![mapping[number - 1].clone()]
            } else {
                let mut value = value.clone();
                if group == "repo" && value.contains('/') {
                    let name = repository(&value)?;
                    let existing = entries.iter().find(|e| {
                        e.group == *group
                            && matches!(&e.params, Params::Repository { repository } if repository.to_lowercase() == name.to_lowercase())
                    });
                    if let Some(existing) = existing {
                        value = existing.id.clone();
                    } else {
                        entries.retain(|e| e.id != name);
                        entries.push(Entry {
                            id: name.clone(),
                            group: "repo".to_owned(),
                            handler: "repository".to_owned(),
                            label: name.clone(),
                            description: "Explicit repository checkout.".to_owned(),
                            params: Params::Repository {
                                repository: name.clone(),
                            },
                            depends_on: Vec::new(),
                        });
                    }
                }
                // Generated leaves are not in the catalog, so the numbers of
                // applications stay stable; their stable IDs are accepted in
                // the application's group.
                if !entries.iter().any(|e| e.id == value)
                    && value.contains(':')
                    && let Some(application) = entries.iter().find(|e| Some(e.id.as_str()) == value.split(':').next())
                    && let Some(leaf) = app_file_entries(application).into_iter().find(|leaf| leaf.id == value)
                    && application.group == *group
                {
                    entries.push(leaf);
                }
                if !entries.iter().any(|e| e.id == value) && value.contains(':') {
                    let (application_id, config_id, key) =
                        config_entry_key(&value).unwrap_or((String::new(), String::new(), Vec::new()));
                    if let Some(application) = entries.iter().find(|e| e.id == application_id)
                        && application.application().is_some()
                        && application.group == *group
                        && let Some(leaf) = config_entries(application).into_iter().find(|leaf| {
                            matches!(&leaf.params, Params::Structured { config, key: k, .. } if config.id == config_id && *k == key)
                        })
                    {
                        value = leaf.id.clone();
                        entries.push(leaf);
                    }
                }
                vec![value]
            };
            for item_id in ids {
                let entry = entries
                    .iter()
                    .find(|e| e.id == item_id && e.group == *group)
                    .ok_or_else(|| {
                        AppError::new(format!(
                            "Unknown stable ID {} in {group}; run setup list",
                            py_repr(&item_id)
                        ))
                    })?;
                match selected.iter_mut().find(|known| known.id == item_id) {
                    Some(slot) => *slot = entry.clone(),
                    None => selected.push(entry.clone()),
                }
            }
        }
    }
    if selected.is_empty() {
        return Err(AppError::new(
            "No actions selected; run setup list and specify at least one item",
        ));
    }
    Ok((selected, snapshot.and_then(|s| s["id"].as_str().map(str::to_owned))))
}
