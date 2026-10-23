//! The setup box (phase 13b, step 5; Python `setup_tui.py`): the catalog as
//! a tree, one section per group, the state the workspace reported, a
//! selection reviewed before it is applied, and the run's activity. No I/O:
//! the worker (`setup_work.rs`) connects, inspects and applies; a key
//! returns what it has to do.

use std::collections::{BTreeSet, HashMap};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hs_model::status::StatusEntry;
use serde_json::Value;
use yaml_rust2::Yaml;

use crate::services::github::{Discovery, ManifestFile};
use crate::services::manifest::{Checked, SECTIONS};
use crate::setup::catalog::Catalog;
use crate::setup::definitions::{ConfigFile, Entry, Params, app_file_entries, application_children, config_entry_id};
use crate::setup::engine::truthy;
use crate::setup::herdr::AUTH_NOTICE;
use crate::setup::plan::{Plan, build_plan, write_paths};
use crate::setup::{Settings, Target, json_of};

/// How a part of a row or a line is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Dim,
    /// Detected ready.
    Green,
    /// Selected for an update, a reapply or an overwrite (bold).
    Red,
    Yellow,
    Bold,
}

/// One node of the tree (a group, an item, a configuration file or section,
/// a managed value or a file copy).
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub id: String,
    pub parent: Option<String>,
    pub children: Vec<String>,
    /// Drawn with an arrow (Textual's `add` rather than `add_leaf`).
    pub branch: bool,
    /// The label: the checkbox first, then its parts.
    pub label: Vec<(String, Tone)>,
}

impl Node {
    /// The label as text.
    #[must_use]
    pub fn plain(&self) -> String {
        self.label.iter().map(|(text, _)| text.as_str()).collect()
    }
}

/// A line of the tree as it is drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: String,
    pub depth: usize,
    pub branch: bool,
    pub expanded: bool,
}

/// What the box shows.
#[derive(Debug, Clone)]
pub enum Stage {
    /// Connecting and reading the state (Python's start-up spinners).
    Loading {
        message: String,
    },
    /// The start failed: the box shows why and closes with Enter or Esc.
    Failed {
        lines: Vec<String>,
    },
    Tree,
    /// The plan before it is applied (Python `Review`).
    Review {
        lines: Vec<String>,
        scroll: usize,
        /// The focus is on Apply (else on Back).
        apply: bool,
        plan: Box<Plan>,
    },
    /// The run (Python `Execution`).
    Running {
        lines: Vec<String>,
        finished: bool,
        title: String,
        /// Desktop Herdr authentication is under way.
        auth_wait: bool,
    },
}

/// What the box asks the worker to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// F5: read the workspace state again.
    Refresh,
    /// Ctrl+R: read the catalog and the state again.
    Catalog,
    /// Run `pending_plan`.
    Execute,
    /// Read the manifests of `services_request`.
    Services,
    /// Open the deployment of `deploy_request`.
    Deploy,
    /// Esc while it runs.
    Cancel,
    /// The box closes: the connection ends.
    Close,
}

/// A row of the Services group: a repository, or one of its manifest
/// files (its index in the repository's discovery).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRow {
    pub repository: String,
    pub file: Option<usize>,
}

/// The label of the Services group in the group bar.
pub const SERVICES: &str = "Services";

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// The line of a manifest file in the services list.
#[must_use]
pub fn service_row(file: &ManifestFile) -> (String, Tone) {
    match &file.manifest {
        Ok(checked) => (
            format!(
                "{}  {}",
                checked
                    .id
                    .as_deref()
                    .unwrap_or(file.path.trim_start_matches(".homestack/")),
                checked.name.as_deref().unwrap_or_default()
            ),
            if checked.manifest.is_some() {
                Tone::Plain
            } else {
                Tone::Red
            },
        ),
        Err(_) => (
            format!("{}  invalid", file.path.trim_start_matches(".homestack/")),
            Tone::Red,
        ),
    }
}

/// What a manifest file declares, its YAML with only the sections in
/// `open` unfolded, and its instances among the service containers Proxmox
/// lists (`services`); also the lines of its sections.
#[must_use]
pub fn service_details(
    discovery: &Discovery,
    file: &ManifestFile,
    services: &[StatusEntry],
    open: &BTreeSet<String>,
) -> (Vec<(String, Tone)>, Vec<usize>) {
    let revision = format!("Revision: {} @ {}", discovery.branch, short(&discovery.commit));
    let checked = match &file.manifest {
        Ok(checked) => checked,
        Err(error) => {
            let lines = vec![
                ("Invalid manifest".to_owned(), Tone::Red),
                (format!("File: {}", file.path), Tone::Plain),
                (revision, Tone::Plain),
                (format!("Error: {error}"), Tone::Red),
                (String::new(), Tone::Plain),
                (
                    "No service is offered from this file; fix it in the repository.".to_owned(),
                    Tone::Plain,
                ),
            ];
            return (lines, Vec::new());
        }
    };
    let mut lines: Vec<(String, Tone)> = [
        format!(
            "{} ({})",
            checked.name.as_deref().unwrap_or("Unnamed service"),
            checked.id.as_deref().unwrap_or("no valid id")
        ),
        format!("Repository: {}", discovery.repository),
        format!("File: {}", file.path),
    ]
    .into_iter()
    .map(|line| (line, Tone::Plain))
    .collect();
    match &file.releases {
        None => {}
        Some(Ok(releases)) if releases.is_empty() => lines.push(("Releases: none published".to_owned(), Tone::Plain)),
        Some(Ok(releases)) => lines.push((
            format!(
                "Releases: {}",
                releases
                    .iter()
                    .take(3)
                    .map(|release| release.image.rsplit_once(':').map_or("", |(_, tag)| tag))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Tone::Plain,
        )),
        Some(Err(error)) => lines.push((format!("Releases: not read: {error}"), Tone::Plain)),
    }
    lines.push((String::new(), Tone::Plain));
    if !checked.issues.is_empty() {
        let count = checked.issues.len();
        lines.push((
            format!(
                "{count} error{} in the manifest; deploy is blocked",
                if count == 1 { "" } else { "s" }
            ),
            Tone::Red,
        ));
    }
    lines.push(("Manifest".to_owned(), Tone::Dim));
    let (manifest, heads) = manifest_lines(checked, open);
    let heads = heads.into_iter().map(|at| at + lines.len()).collect();
    lines.extend(manifest);
    lines.push((String::new(), Tone::Plain));
    let found = checked
        .id
        .as_deref()
        .map(|id| instances(services, &discovery.repository, id))
        .unwrap_or_default();
    if found.is_empty() {
        lines.push(("Status: not deployed".to_owned(), Tone::Plain));
    }
    for entry in found {
        let image = entry.service.as_ref().map_or("", |s| s.image.as_str());
        lines.push((
            format!(
                "CT {} {} on {}, {}: {} · {}",
                entry.vmid,
                entry.name.as_deref().unwrap_or_default(),
                entry.node,
                entry.ip.as_deref().unwrap_or("—"),
                image.rsplit_once(':').map_or(image, |(_, tag)| tag),
                entry.status
            ),
            Tone::Plain,
        ));
    }
    // Where the manifest was read, for reference.
    lines.push((String::new(), Tone::Plain));
    lines.push((revision, Tone::Plain));
    lines.push((format!("Blob: {}", short(&file.blob)), Tone::Plain));
    (lines, heads)
}

/// The top-level keys of the manifest in the order shown: the sections,
/// then unknown ones.
#[must_use]
pub fn section_names(checked: &Checked) -> Vec<String> {
    let mut names: Vec<String> = SECTIONS.iter().map(|name| (*name).to_owned()).collect();
    if let Yaml::Hash(top) = &checked.document {
        names.extend(
            top.keys()
                .filter_map(Yaml::as_str)
                .filter(|name| !SECTIONS.contains(name))
                .map(str::to_owned),
        );
    }
    names
}

/// The manifest's YAML in section order, each section collapsed to one
/// line with its error count unless it is in `open`; faulty values and
/// their sections red, each issue below its line. Also the index of each
/// section's line, in [`section_names`] order.
fn manifest_lines(checked: &Checked, open: &BTreeSet<String>) -> (Vec<(String, Tone)>, Vec<usize>) {
    let Yaml::Hash(top) = &checked.document else {
        return (Vec::new(), Vec::new());
    };
    let mut lines = Vec::new();
    let mut heads = Vec::new();
    for name in section_names(checked) {
        heads.push(lines.len());
        let path = vec![name.clone()];
        let Some(value) = top.get(&Yaml::String(name.clone())) else {
            lines.push((format!("  {name}:"), Tone::Red));
            issue_lines(checked, &path, 2, &mut lines);
            continue;
        };
        let errors = checked
            .issues
            .iter()
            .filter(|issue| issue.path.starts_with(&path))
            .count();
        let tone = if errors > 0 { Tone::Red } else { Tone::Plain };
        match value {
            Yaml::Null => {
                lines.push((format!("  {name}: (empty: off)"), tone));
                issue_lines(checked, &path, 2, &mut lines);
            }
            Yaml::Hash(_) | Yaml::Array(_) if !open.contains(&name) => {
                let note = match errors {
                    0 => String::new(),
                    1 => "  ✗ 1 error".to_owned(),
                    n => format!("  ✗ {n} errors"),
                };
                lines.push((format!("▸ {name}{note}"), tone));
                issue_lines(checked, &path, 2, &mut lines);
            }
            Yaml::Hash(_) | Yaml::Array(_) => {
                lines.push((format!("▾ {name}:"), tone));
                issue_lines(checked, &path, 2, &mut lines);
                yaml_lines(checked, value, &path, 4, &mut lines);
            }
            scalar => {
                lines.push((format!("  {name}: {}", scalar_text(scalar)), tone));
                issue_lines(checked, &path, 2, &mut lines);
            }
        }
    }
    (lines, heads)
}

/// The issues exactly at `path`, below its line.
fn issue_lines(checked: &Checked, path: &[String], indent: usize, lines: &mut Vec<(String, Tone)>) {
    for issue in checked.issues.iter().filter(|issue| issue.path == path) {
        lines.push((format!("{}✗ {}", " ".repeat(indent + 2), issue.message), Tone::Red));
    }
}

/// The nested `value` at `path`, indented by `indent`.
fn yaml_lines(checked: &Checked, value: &Yaml, path: &[String], indent: usize, lines: &mut Vec<(String, Tone)>) {
    let pad = " ".repeat(indent);
    let tone_at = |path: &[String]| {
        if checked.issues.iter().any(|issue| issue.path.starts_with(path)) {
            Tone::Red
        } else {
            Tone::Plain
        }
    };
    match value {
        Yaml::Hash(fields) => {
            for (key, value) in fields {
                let key = scalar_text(key);
                let mut path = path.to_vec();
                path.push(key.clone());
                let tone = tone_at(&path);
                match value {
                    Yaml::Hash(inner) if !inner.is_empty() => {
                        lines.push((format!("{pad}{key}:"), tone));
                        issue_lines(checked, &path, indent, lines);
                        yaml_lines(checked, value, &path, indent + 2, lines);
                    }
                    Yaml::Array(inner) if !inner.is_empty() => {
                        lines.push((format!("{pad}{key}:"), tone));
                        issue_lines(checked, &path, indent, lines);
                        yaml_lines(checked, value, &path, indent + 2, lines);
                    }
                    _ => {
                        lines.push((format!("{pad}{key}: {}", scalar_text(value)), tone));
                        issue_lines(checked, &path, indent, lines);
                    }
                }
            }
        }
        Yaml::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                let mut path = path.to_vec();
                path.push(index.to_string());
                match item {
                    Yaml::Hash(inner) if !inner.is_empty() => {
                        // The entries one level deeper, the first behind `- `.
                        let first = lines.len();
                        yaml_lines(checked, item, &path, indent + 2, lines);
                        if let Some((line, _)) = lines.get_mut(first) {
                            line.replace_range(indent..indent + 2, "- ");
                        }
                        issue_lines(checked, &path, indent, lines);
                    }
                    _ => {
                        lines.push((format!("{pad}- {}", scalar_text(item)), tone_at(&path)));
                        issue_lines(checked, &path, indent, lines);
                    }
                }
            }
        }
        _ => {}
    }
}

/// A scalar as YAML shows it; an empty mapping or list inline.
fn scalar_text(value: &Yaml) -> String {
    match value {
        Yaml::String(text) if text.is_empty() => "\"\"".to_owned(),
        Yaml::String(text) | Yaml::Real(text) => text.clone(),
        Yaml::Integer(number) => number.to_string(),
        Yaml::Boolean(value) => value.to_string(),
        Yaml::Null => String::new(),
        Yaml::Hash(_) => "{}".to_owned(),
        Yaml::Array(_) => "[]".to_owned(),
        _ => "?".to_owned(),
    }
}

/// The containers of `repository`/`id` among `services`.
fn instances<'a>(services: &'a [StatusEntry], repository: &str, id: &str) -> Vec<&'a StatusEntry> {
    services
        .iter()
        .filter(|entry| {
            entry
                .service
                .as_ref()
                .is_some_and(|s| s.repository == repository && s.id == id)
        })
        .collect()
}

/// A message (Python `notify`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub error: bool,
}

/// Where a configuration node is.
#[derive(Debug, Clone)]
struct ConfigNode {
    application: String,
    config: ConfigFile,
    key: Vec<String>,
    label: String,
}

/// The setup box.
#[derive(Debug, Clone)]
pub struct SetupBox {
    pub settings: Settings,
    pub target: Target,
    pub catalog: Catalog,
    /// What `inspect_workspace_state` reported.
    pub state: Value,
    pub selected: BTreeSet<String>,
    /// The catalog's entries and the generated children of each application.
    entries: Vec<Entry>,
    config_nodes: HashMap<String, ConfigNode>,
    /// What was typed into the filter.
    pub filter: String,
    /// Keys go to the filter.
    pub filtering: bool,
    pub expanded: HashMap<String, bool>,
    /// The highlighted node.
    pub cursor: String,
    /// The group shown.
    pub section: usize,
    /// Opened with F9: show Services once the catalog is read.
    pub services_first: bool,
    pub nodes: HashMap<String, Node>,
    pub action_states: HashMap<String, String>,
    /// The last item state of the run (`ID: state`), shown with the target.
    pub last_progress: String,
    pub busy: bool,
    pub cancel_requested: bool,
    pub result: Option<Value>,
    /// The reviewed plan the run takes.
    pub pending_plan: Option<Plan>,
    /// The repository (and its default branch) whose manifests the worker
    /// reads next.
    pub services_request: Option<(String, Option<String>)>,
    /// The service whose deployment opens next.
    pub deploy_request: Option<crate::services::deploy::Source>,
    /// What each opened repository declares, as last read; `None` while
    /// it is read.
    pub discoveries: HashMap<String, Option<Result<Discovery, String>>>,
    /// The repositories opened in the Services group.
    pub services_open: BTreeSet<String>,
    /// The highlighted row of the Services group.
    pub services_cursor: usize,
    /// When and on which row the last click was, for a double click.
    pub last_click: Option<(std::time::Instant, usize)>,
    /// The service containers the status lists.
    pub services: Vec<StatusEntry>,
    pub ssh_status: String,
    pub stage: Stage,
    pub details_scroll: usize,
    /// The sections of the highlighted manifest shown open.
    pub manifest_open: BTreeSet<String>,
    /// The highlighted section while its sections are browsed.
    pub manifest_focus: Option<usize>,
    pub notice: Option<Notice>,
    /// The spinner's frame.
    pub frame: usize,
}

const UNAVAILABLE: [&str; 3] = ["missing", "type mismatch", "removed"];

/// Python's `json.dumps(value, ensure_ascii=False, indent=2)` of a TOML
/// value, in the file's key order.
fn pretty_toml(value: &toml::Value, depth: usize) -> String {
    let indent = |depth: usize| "  ".repeat(depth);
    match value {
        toml::Value::Table(table) if !table.is_empty() => {
            let items: Vec<String> = table
                .iter()
                .map(|(key, child)| {
                    format!(
                        "{}{}: {}",
                        indent(depth + 1),
                        Value::String(key.clone()),
                        pretty_toml(child, depth + 1)
                    )
                })
                .collect();
            format!("{{\n{}\n{}}}", items.join(",\n"), indent(depth))
        }
        toml::Value::Array(items) if !items.is_empty() => {
            let items: Vec<String> = items
                .iter()
                .map(|child| format!("{}{}", indent(depth + 1), pretty_toml(child, depth + 1)))
                .collect();
            format!("[\n{}\n{}]", items.join(",\n"), indent(depth))
        }
        other => json_of(other).to_string(),
    }
}

/// [`pretty_toml`] of a JSON value.
fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// Python `json.dumps(..., separators=(",", ":"))` of a TOML value.
fn compact_toml(value: &toml::Value) -> String {
    match value {
        toml::Value::Table(table) => {
            let items: Vec<String> = table
                .iter()
                .map(|(key, child)| format!("{}:{}", Value::String(key.clone()), compact_toml(child)))
                .collect();
            format!("{{{}}}", items.join(","))
        }
        toml::Value::Array(items) => {
            format!("[{}]", items.iter().map(compact_toml).collect::<Vec<_>>().join(","))
        }
        other => json_of(other).to_string(),
    }
}

/// Python `_compact_value`: at most 96 characters.
fn compact_value(value: &toml::Value) -> String {
    let rendered = compact_toml(value);
    if rendered.chars().count() <= 96 {
        rendered
    } else {
        rendered.chars().take(96).collect()
    }
}

/// Python `_mtime_text`: nanoseconds since the epoch as UTC.
fn mtime_text(value: &Value) -> String {
    let Some(nanos) = value.as_i64() else {
        return "unknown".to_owned();
    };
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(nanos)).map_or_else(
        |_| "unknown".to_owned(),
        |at| {
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                at.year(),
                u8::from(at.month()),
                at.day(),
                at.hour(),
                at.minute(),
                at.second()
            )
        },
    )
}

/// Python `Execution._clean_line`: no escape sequences, no control
/// characters, one line of at most 500 characters.
#[must_use]
pub fn clean_line(identity: Option<&str>, message: &str) -> String {
    let mut plain = String::new();
    let mut chars = message.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            // Parameters and intermediates, then the final byte.
            while let Some(&next) = chars.peek() {
                chars.next();
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
            continue;
        }
        plain.push(c);
    }
    let joined = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    let value: String = joined.chars().filter(|c| *c >= ' ' && *c != '\x7f').collect();
    if value.is_empty() {
        return String::new();
    }
    let prefix = identity.map(|id| format!("{id}: ")).unwrap_or_default();
    format!("{prefix}{}", value.chars().take(500).collect::<String>())
}

/// The details of a backup over SSH (Python `_backup_retrieval_details`).
fn backup_retrieval_details(vmid: u32) -> Vec<String> {
    vec![
        "Restricted SSH retrieval: two desktop ED25519 key pairs (private + .pub)".to_owned(),
        format!("Archive key: ~/.ssh/homestack/backup/vm{vmid}-bk-archive"),
        format!("Status key: ~/.ssh/homestack/backup/vm{vmid}-bk-status"),
        "Generate missing pairs; repair missing public keys; reuse valid pairs without rotation.".to_owned(),
        "Install only public keys in guest ~/.ssh/authorized_keys; preserve unrelated entries.".to_owned(),
        "Forced commands: cat of ~/backup/backup.tgz or ~/backup/status.json only; no shell or forwarding.".to_owned(),
        "Create missing ~/.config/bk/backup.yaml with default destination ~/backup".to_owned(),
        "Private keys stay on the desktop. Existing backup configuration and data remain unchanged.".to_owned(),
    ]
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(str::to_owned))
        .collect()
}

impl SetupBox {
    /// A box that is still connecting to `target`.
    #[must_use]
    pub fn loading(settings: Settings, target: Target, message: &str) -> Self {
        let mut setup = Self::new(settings, target, Catalog::default(), None);
        setup.stage = Stage::Loading {
            message: message.to_owned(),
        };
        setup
    }

    /// The box over `catalog`, with the state read (Python `SetupApp`).
    #[must_use]
    pub fn new(settings: Settings, target: Target, catalog: Catalog, state: Option<Value>) -> Self {
        let connected = state.is_some();
        let mut setup = Self {
            settings,
            target,
            catalog,
            state: state.unwrap_or_else(|| {
                serde_json::json!({"items": {}, "checked_at": null, "registry_present": false,
                                   "state_path": "~/.local/state/homestack/setup.json"})
            }),
            selected: BTreeSet::new(),
            entries: Vec::new(),
            config_nodes: HashMap::new(),
            filter: String::new(),
            filtering: false,
            expanded: HashMap::new(),
            cursor: String::new(),
            section: 0,
            nodes: HashMap::new(),
            action_states: HashMap::new(),
            last_progress: String::new(),
            busy: false,
            cancel_requested: false,
            result: None,
            pending_plan: None,
            services_request: None,
            deploy_request: None,
            discoveries: HashMap::new(),
            services_open: BTreeSet::new(),
            services_first: false,
            services_cursor: 0,
            last_click: None,
            services: Vec::new(),
            ssh_status: if connected { "connected" } else { "not connected" }.to_owned(),
            stage: Stage::Tree,
            details_scroll: 0,
            manifest_open: BTreeSet::new(),
            manifest_focus: None,
            notice: None,
            frame: 0,
        };
        setup.cursor = setup.group_id(0).unwrap_or_default();
        setup.rebuild();
        setup
    }

    /// The worker connected and read the state.
    pub fn ready(&mut self, catalog: Catalog, state: Value, target: Target, settings: Settings) {
        let mut ready = Self::new(settings, target, catalog, Some(state));
        if self.services_first {
            ready.show_section(ready.services_tab());
        } else {
            ready.section = self.section;
        }
        ready.services = std::mem::take(&mut self.services);
        *self = ready;
    }

    /// The ID of the group shown at tab `index`; `None` for Services.
    fn group_id(&self, index: usize) -> Option<String> {
        self.group_index(index)
            .and_then(|group| self.settings.setup.groups.get(group))
            .map(|group| group.id.clone())
    }

    /// The tab of the Services group: right after Repositories.
    #[must_use]
    pub fn services_tab(&self) -> usize {
        let groups = &self.settings.setup.groups;
        groups
            .iter()
            .position(|group| group.id == "repo")
            .map_or(groups.len(), |at| at + 1)
    }

    /// Whether the Services group is shown.
    #[must_use]
    pub fn in_services(&self) -> bool {
        !self.settings.setup.groups.is_empty() && self.section == self.services_tab()
    }

    /// The configured group at tab `tab`.
    fn group_index(&self, tab: usize) -> Option<usize> {
        let services = self.services_tab();
        match tab.cmp(&services) {
            std::cmp::Ordering::Less => Some(tab),
            std::cmp::Ordering::Equal => None,
            std::cmp::Ordering::Greater => Some(tab - 1),
        }
    }

    /// The tab of configured group `group`.
    fn tab_of(&self, group: usize) -> usize {
        if group >= self.services_tab() { group + 1 } else { group }
    }

    /// Of the group `id`: its items with something selected, and its items.
    #[must_use]
    pub fn group_count(&self, id: &str) -> (usize, usize) {
        let entries: Vec<&Entry> = self.catalog.entries.iter().filter(|entry| entry.group == id).collect();
        let count = entries
            .iter()
            .filter(|entry| {
                self.descendant_ids(&entry.id, false)
                    .iter()
                    .any(|id| self.selected.contains(id))
            })
            .count();
        (count, entries.len())
    }

    /// The groups, for the carousel (← → move through them), and
    /// Services.
    #[must_use]
    pub fn groups(&self) -> Vec<String> {
        let mut labels: Vec<String> = self
            .settings
            .setup
            .groups
            .iter()
            .map(|group| group.label.clone())
            .collect();
        if !labels.is_empty() {
            labels.insert(self.services_tab(), SERVICES.to_owned());
        }
        labels
    }

    fn filter_text(&self) -> String {
        self.filter.to_lowercase()
    }

    fn entry(&self, identity: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.id == identity)
    }

    fn is_group(&self, identity: &str) -> bool {
        self.settings.setup.groups.iter().any(|group| group.id == identity)
    }

    /// Python `available`.
    #[must_use]
    pub fn available(&self, entry: &Entry) -> bool {
        let key = match &entry.params {
            Params::Structured { application, .. } => application.as_str(),
            _ => entry.id.as_str(),
        };
        let state = self.catalog.availability.get(key).map_or("unknown", String::as_str);
        !(UNAVAILABLE.contains(&state) || state.starts_with("unavailable"))
    }

    fn availability(&self, identity: &str) -> String {
        self.catalog
            .availability
            .get(identity)
            .cloned()
            .unwrap_or_else(|| "unknown".to_owned())
    }

    fn matches(&self, entry: &Entry) -> bool {
        format!("{} {} {}", entry.label, entry.id, entry.description)
            .to_lowercase()
            .contains(&self.filter_text())
    }

    fn refresh_entries(&mut self) {
        let mut entries: Vec<Entry> = Vec::new();
        let push = |entry: Entry, entries: &mut Vec<Entry>| {
            if let Some(at) = entries.iter().position(|e| e.id == entry.id) {
                entries[at] = entry;
            } else {
                entries.push(entry);
            }
        };
        for entry in &self.catalog.entries {
            push(entry.clone(), &mut entries);
            if matches!(entry.params, Params::Application(_)) {
                for leaf in application_children(entry) {
                    push(leaf, &mut entries);
                }
            }
        }
        self.entries = entries;
    }

    fn config_leaves(&self, application: &str, config: &str, prefix: &[String]) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|entry| match &entry.params {
                Params::Structured {
                    application: owner,
                    config: file,
                    key,
                    ..
                } => owner == application && file.id == config && key.starts_with(prefix),
                _ => false,
            })
            .collect()
    }

    fn config_node_matches(&self, identity: &str) -> bool {
        let filter = self.filter_text();
        let Some(meta) = self.config_nodes.get(identity) else {
            return false;
        };
        if filter.is_empty() {
            return false;
        }
        format!("{} {identity} {}", meta.label, meta.config.path)
            .to_lowercase()
            .contains(&filter)
    }

    fn identity_visible(&self, identity: &str) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        if self.entry(identity).is_some_and(|entry| self.matches(entry)) {
            return true;
        }
        self.config_nodes.contains_key(identity) && self.config_node_matches(identity)
    }

    fn branch_visible(&self, identity: &str) -> bool {
        if self.filter.is_empty() || self.identity_visible(identity) {
            return true;
        }
        if let Some(entry) = self.entry(identity)
            && matches!(entry.params, Params::Application(_))
        {
            return application_children(entry)
                .iter()
                .any(|leaf| self.identity_visible(&leaf.id));
        }
        if let Some(meta) = self.config_nodes.get(identity) {
            if self
                .entry(&meta.application)
                .is_some_and(|application| self.matches(application))
            {
                return true;
            }
            return self
                .config_leaves(&meta.application, &meta.config.id, &meta.key)
                .iter()
                .any(|leaf| self.identity_visible(&leaf.id));
        }
        false
    }

    /// Python `_descendant_ids`.
    #[must_use]
    pub fn descendant_ids(&self, identity: &str, visible: bool) -> Vec<String> {
        let ids: Vec<String> = if self.is_group(identity) {
            self.entries
                .iter()
                .filter(|entry| entry.group == identity)
                .map(|entry| entry.id.clone())
                .collect()
        } else if let Some(entry) = self.entry(identity) {
            if matches!(entry.params, Params::Application(_)) {
                let mut ids = vec![entry.id.clone()];
                ids.extend(application_children(entry).into_iter().map(|leaf| leaf.id));
                ids
            } else {
                vec![entry.id.clone()]
            }
        } else if let Some(meta) = self.config_nodes.get(identity) {
            self.config_leaves(&meta.application, &meta.config.id, &meta.key)
                .iter()
                .map(|leaf| leaf.id.clone())
                .collect()
        } else {
            Vec::new()
        };
        if !visible || self.filter.is_empty() {
            return ids;
        }
        // A matching application, file or section makes its managed
        // descendants visible; otherwise only directly matching leaves.
        if self
            .entry(identity)
            .is_some_and(|entry| matches!(entry.params, Params::Application(_)) && self.matches(entry))
        {
            return ids;
        }
        if self.config_nodes.contains_key(identity) && self.config_node_matches(identity) {
            return ids;
        }
        ids.into_iter().filter(|id| self.identity_visible(id)).collect()
    }

    fn descendants(&self, identity: &str, visible: bool) -> Vec<&Entry> {
        self.descendant_ids(identity, visible)
            .iter()
            .filter_map(|id| self.entry(id))
            .filter(|entry| self.available(entry))
            .collect()
    }

    /// Python `checkbox`: `[x]`, `[-]` or `[ ]`.
    #[must_use]
    pub fn checkbox(&self, identity: &str) -> &'static str {
        let tri = |count: usize, total: usize| {
            if total > 0 && count == total {
                "[x]"
            } else if count > 0 {
                "[-]"
            } else {
                "[ ]"
            }
        };
        let group_entries: Vec<&Entry> = self.catalog.entries.iter().filter(|e| e.group == identity).collect();
        if group_entries
            .iter()
            .any(|entry| matches!(entry.params, Params::Application(_)))
        {
            let states: Vec<&str> = group_entries.iter().map(|entry| self.checkbox(&entry.id)).collect();
            return if states.iter().all(|state| *state == "[x]") {
                "[x]"
            } else if states.iter().any(|state| *state != "[ ]") {
                "[-]"
            } else {
                "[ ]"
            };
        }
        let application = self
            .entry(identity)
            .is_some_and(|entry| matches!(entry.params, Params::Application(_)));
        if application || self.config_nodes.contains_key(identity) {
            let ids = self.descendant_ids(identity, false);
            let count = ids.iter().filter(|id| self.selected.contains(*id)).count();
            return tri(count, ids.len());
        }
        let entries = self.descendants(identity, false);
        let count = entries.iter().filter(|entry| self.selected.contains(&entry.id)).count();
        tri(count, entries.len())
    }

    /// Python `config_selection_count`: managed values and file copies.
    #[must_use]
    pub fn config_selection_count(&self, identity: &str) -> (usize, usize) {
        let leaves: Vec<String> = self
            .descendant_ids(identity, false)
            .into_iter()
            .filter(|id| {
                self.entry(id).is_some_and(|entry| {
                    matches!(
                        entry.params,
                        Params::Structured { .. }
                            | Params::File {
                                application: Some(_),
                                ..
                            }
                    )
                })
            })
            .collect();
        let selected = leaves.iter().filter(|id| self.selected.contains(*id)).count();
        (selected, leaves.len())
    }

    fn state_item(&self, identity: &str) -> &Value {
        &self.state["items"][identity]
    }

    /// Python `entry_style`.
    #[must_use]
    pub fn entry_style(&self, entry: &Entry) -> Tone {
        let state = self.state_item(&entry.id);
        let selected = self.selected.contains(&entry.id);
        if matches!(entry.params, Params::Environment { .. })
            && (truthy(&state["ready"]) || (truthy(&state["managed"]) && state["state"] == "needs update"))
        {
            if truthy(&state["changed_since_apply"]) || truthy(&state["will_overwrite"]) {
                return if selected { Tone::Red } else { Tone::Green };
            }
            return Tone::Green;
        }
        if selected && (truthy(&state["ready"]) || truthy(&state["will_overwrite"])) {
            return Tone::Red;
        }
        if truthy(&state["ready"]) {
            return Tone::Green;
        }
        Tone::Plain
    }

    /// Python `entry_label`: a repository of `[repo] owner` without it.
    fn entry_label(&self, entry: &Entry) -> String {
        if let Params::Repository { repository } = &entry.params
            && entry.label == *repository
            && let Some((owner, name)) = repository.split_once('/')
            && self
                .settings
                .repo
                .owner
                .as_deref()
                .is_some_and(|configured| owner.to_lowercase() == configured.to_lowercase())
        {
            return name.to_owned();
        }
        entry.label.clone()
    }

    fn entry_text(&self, entry: &Entry, index: usize, interaction: &str, suffix: &str) -> Vec<(String, Tone)> {
        let mut text = vec![
            (format!("{} {index}. ", self.checkbox(&entry.id)), Tone::Plain),
            (self.entry_label(entry), self.entry_style(entry)),
        ];
        if matches!(entry.params, Params::Application(_)) {
            let (selected, total) = self.config_selection_count(&entry.id);
            if total > 0 {
                text.push((format!("  {selected}/{total}"), Tone::Dim));
            }
        }
        if !interaction.is_empty() {
            text.push((interaction.to_owned(), Tone::Dim));
        }
        if !suffix.is_empty() {
            text.push((suffix.to_owned(), Tone::Dim));
        }
        text
    }

    fn config_leaf_text(&self, entry: &Entry) -> Vec<(String, Tone)> {
        let Params::Structured { key, value, .. } = &entry.params else {
            return Vec::new();
        };
        let mut text = vec![
            (format!("{} ", self.checkbox(&entry.id)), Tone::Plain),
            (
                format!("{} = {}", key.last().map_or("", String::as_str), compact_value(value)),
                self.entry_style(entry),
            ),
        ];
        let live = self.state_item(&entry.id);
        let state = live["state"].as_str().unwrap_or_default();
        if ["unavailable", "parse error", "structural conflict"].contains(&state) {
            text.push((format!(" — {state}"), Tone::Dim));
        } else if let Some(action) = self.action_states.get(&entry.id) {
            text.push((format!(" — {action}"), Tone::Dim));
        }
        text
    }

    fn file_leaf_text(&self, entry: &Entry) -> Vec<(String, Tone)> {
        let Params::File { path, .. } = &entry.params else {
            return Vec::new();
        };
        let mut text = vec![
            (format!("{} ", self.checkbox(&entry.id)), Tone::Plain),
            (path.clone(), self.entry_style(entry)),
        ];
        if !self.available(entry) {
            text.push((format!(" — {}", self.availability(&entry.id)), Tone::Dim));
        }
        if let Some(action) = self.action_states.get(&entry.id) {
            text.push((format!(" — {action}"), Tone::Dim));
        }
        text
    }

    fn add(&mut self, parent: Option<&str>, id: &str, branch: bool, label: Vec<(String, Tone)>) {
        if let Some(parent) = parent
            && let Some(node) = self.nodes.get_mut(parent)
        {
            node.children.push(id.to_owned());
        }
        self.nodes.insert(
            id.to_owned(),
            Node {
                id: id.to_owned(),
                parent: parent.map(str::to_owned),
                children: Vec::new(),
                branch,
                label,
            },
        );
    }

    fn config_node_id(application: &str, config: &ConfigFile, key: &[String]) -> String {
        if key.is_empty() {
            format!("{application}:{}", config.id)
        } else {
            config_entry_id(application, &config.id, key)
        }
    }

    /// Python `config_branch`: one file or section and what is under it.
    fn config_branch(&mut self, application: &Entry, config: &ConfigFile, prefix: &[String], parent: &str) {
        let identity = Self::config_node_id(&application.id, config, prefix);
        let label = prefix.last().cloned().unwrap_or_else(|| config.path.clone());
        self.config_nodes.insert(
            identity.clone(),
            ConfigNode {
                application: application.id.clone(),
                config: config.clone(),
                key: prefix.to_vec(),
                label: label.clone(),
            },
        );
        let mut table = &config.values;
        for key in prefix {
            match table.get(key) {
                Some(toml::Value::Table(child)) => table = child,
                _ => return,
            }
        }
        let children: Vec<(String, toml::Value)> = table.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        if !self.branch_visible(&identity) {
            return;
        }
        let (selected, total) = self.config_selection_count(&identity);
        let text = format!("{} {label}  {selected}/{total}", self.checkbox(&identity));
        self.add(Some(parent), &identity, true, vec![(text, Tone::Plain)]);
        for (key, value) in children {
            let mut child = prefix.to_vec();
            child.push(key);
            if matches!(value, toml::Value::Table(_)) {
                self.config_branch(application, config, &child, &identity);
                continue;
            }
            let leaf_id = config_entry_id(&application.id, &config.id, &child);
            let Some(leaf) = self.entry(&leaf_id).cloned() else {
                continue;
            };
            if !self.filter.is_empty()
                && !self.identity_visible(&leaf_id)
                && !self.config_node_matches(&identity)
                && !self.identity_visible(&application.id)
            {
                continue;
            }
            let label = self.config_leaf_text(&leaf);
            self.add(Some(&identity), &leaf_id, false, label);
        }
    }

    /// Python `rebuild`: the tree from the catalog, the selection and the
    /// state; the highlight stays, or goes to its nearest shown ancestor.
    pub fn rebuild(&mut self) {
        self.refresh_entries();
        self.config_nodes.clear();
        self.nodes.clear();
        let groups = self.settings.setup.groups.clone();
        for group in &groups {
            let extra = if group.id == "repo" && !self.catalog.repository_error.is_empty() {
                format!(" — {}", self.catalog.repository_error)
            } else {
                String::new()
            };
            let entries: Vec<Entry> = self
                .catalog
                .entries
                .iter()
                .filter(|entry| entry.group == group.id)
                .cloned()
                .collect();
            let count = entries
                .iter()
                .filter(|entry| {
                    self.descendant_ids(&entry.id, false)
                        .iter()
                        .any(|id| self.selected.contains(id))
                })
                .count();
            let text = format!(
                "{} {}  {count}/{}{extra}",
                self.checkbox(&group.id),
                group.label,
                entries.len()
            );
            self.add(None, &group.id, true, vec![(text, Tone::Plain)]);
            // The numbers are the catalog's (`repo=N`); the repositories the
            // workspace has come first, the others in the catalog's order
            // (the owner, 2026-10-15).
            let mut order: Vec<(usize, &Entry)> = entries.iter().enumerate().collect();
            if group.id == "repo" {
                order.sort_by_key(|(_, entry)| !truthy(&self.state_item(&entry.id)["exists"]));
            }
            for (index, entry) in order {
                if !self.branch_visible(&entry.id) {
                    continue;
                }
                let interaction = match &entry.params {
                    Params::Application(p) if p.interaction == "interactive" => " — interactive",
                    _ => "",
                };
                let mut suffix = if self.available(entry) {
                    String::new()
                } else {
                    format!(" — {}", self.availability(&entry.id))
                };
                if let Some(action) = self.action_states.get(&entry.id) {
                    suffix.push_str(&format!(" — {action}"));
                }
                let label = self.entry_text(entry, index + 1, interaction, &suffix);
                match &entry.params {
                    Params::Application(p) if !p.config_files.is_empty() || !p.files.is_empty() => {
                        self.add(Some(&group.id), &entry.id, true, label);
                        for config in &p.config_files {
                            self.config_branch(entry, config, &[], &entry.id);
                        }
                        for leaf in app_file_entries(entry) {
                            if !self.filter.is_empty()
                                && !self.identity_visible(&leaf.id)
                                && !self.identity_visible(&entry.id)
                            {
                                continue;
                            }
                            let label = self.file_leaf_text(&leaf);
                            self.add(Some(&entry.id), &leaf.id, false, label);
                        }
                    }
                    _ => self.add(Some(&group.id), &entry.id, false, label),
                }
            }
        }
        self.restore_cursor();
    }

    /// Whether `identity` is drawn open: groups always (each is a section),
    /// everything else when it was opened.
    fn is_expanded(&self, identity: &str) -> bool {
        self.is_group(identity) || self.expanded.get(identity).copied().unwrap_or(false)
    }

    /// The rows of the section shown.
    #[must_use]
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        let Some(group) = self.group_id(self.section) else {
            return rows;
        };
        let mut stack = vec![(group, 0_usize)];
        while let Some((id, depth)) = stack.pop() {
            let Some(node) = self.nodes.get(&id) else {
                continue;
            };
            let expanded = self.is_expanded(&id);
            rows.push(Row {
                id: id.clone(),
                depth,
                branch: node.branch && !self.is_group(&id),
                expanded,
            });
            if expanded {
                for child in node.children.iter().rev() {
                    stack.push((child.clone(), depth + 1));
                }
            }
        }
        rows
    }

    fn section_of(&self, identity: &str) -> Option<usize> {
        let mut id = identity.to_owned();
        while let Some(parent) = self.nodes.get(&id).and_then(|node| node.parent.clone()) {
            id = parent;
        }
        self.settings
            .setup
            .groups
            .iter()
            .position(|group| group.id == id)
            .map(|group| self.tab_of(group))
    }

    /// Python `restore_cursor`: a hidden highlight moves to the closest
    /// collapsed ancestor, a removed one to the section's group.
    fn restore_cursor(&mut self) {
        let group = self.group_id(self.section).unwrap_or_default();
        if !self.nodes.contains_key(&self.cursor) || self.section_of(&self.cursor) != Some(self.section) {
            self.cursor = group;
            return;
        }
        let mut node = self.cursor.clone();
        let mut ancestor = self.nodes.get(&node).and_then(|n| n.parent.clone());
        while let Some(id) = ancestor {
            if !self.is_expanded(&id) {
                node = id.clone();
            }
            ancestor = self.nodes.get(&id).and_then(|n| n.parent.clone());
        }
        self.cursor = node;
    }

    /// Selected items the filter hides (Python's `Hidden selected`).
    fn hidden_selected(&self) -> usize {
        if self.filter.is_empty() {
            return 0;
        }
        self.entries
            .iter()
            .filter(|entry| self.selected.contains(&entry.id) && !self.identity_visible(&entry.id))
            .count()
    }

    /// Python `target_description`.
    #[must_use]
    pub fn target_description(&self) -> String {
        if self.target.local {
            format!("Local: {} ({})", self.target.name, self.target.home)
        } else {
            format!("Workspace: {} (VM {})", self.target.name, self.target.vmid)
        }
    }

    /// The target and its connection, at the foot of the details (Python
    /// `target_label`, the line above the tree).
    #[must_use]
    pub fn target_lines(&self, utc_offset: i64) -> Vec<String> {
        let mut lines = vec![self.target_description()];
        if !self.target.ip.is_empty() {
            lines.push(format!("Address: {}", self.target.ip));
        }
        if !self.target.local {
            lines.push(format!("SSH: {}", self.ssh_status));
        }
        if let Some(checked) = self.state["checked_at"].as_str() {
            let local = super::reinstall::local_time(checked, utc_offset).unwrap_or_else(|| checked.to_owned());
            lines.push(format!("State: {local}"));
        }
        let hidden = self.hidden_selected();
        if hidden > 0 {
            lines.push(format!("Hidden selected: {hidden}"));
        }
        if !self.last_progress.is_empty() {
            lines.push(self.last_progress.clone());
        }
        lines
    }

    fn backup_details(&self) -> Vec<String> {
        if self.target.local {
            return [
                "Install ~/.local/bin/bk and the backup.service / backup.timer user units.",
                "Enable the nightly user timer. No SSH keys or authorized_keys changes.",
                "Create missing ~/.config/bk/backup.yaml with default destination ~/backup",
                "Run bk edit to select sources. The configuration always backs itself up.",
                "Existing backup configuration and data remain unchanged.",
            ]
            .map(str::to_owned)
            .to_vec();
        }
        backup_retrieval_details(self.target.vmid)
    }

    /// Python `_value_lines`.
    fn value_lines(desired: &toml::Value, live: &Value) -> Vec<String> {
        let state = live["state"].as_str();
        let current = match live.get("current") {
            Some(current) => pretty_json(current),
            None => match state {
                Some("missing") => "(not set)".to_owned(),
                Some("unavailable") => "(unavailable)".to_owned(),
                _ => "(unknown; reread with F5)".to_owned(),
            },
        };
        if state == Some("matching") {
            return vec!["Workspace value (matches desired):".to_owned(), current];
        }
        vec![
            "Workspace value:".to_owned(),
            current,
            "Desired value:".to_owned(),
            pretty_toml(desired, 0),
        ]
    }

    /// Python `details`: everything about the highlighted node.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn details(&self, identity: &str) -> String {
        let entry = self
            .catalog
            .entries
            .iter()
            .find(|entry| entry.id == identity)
            .or_else(|| self.entry(identity));
        if entry.is_none()
            && let Some(meta) = self.config_nodes.get(identity)
        {
            let leaves = self.config_leaves(&meta.application, &meta.config.id, &meta.key);
            let (selected, total) = self.config_selection_count(identity);
            let mut states: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
            for leaf in &leaves {
                let state = self.state_item(&leaf.id)["state"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_owned();
                *states.entry(state).or_default() += 1;
            }
            let state_text = if states.is_empty() {
                "unknown".to_owned()
            } else {
                states
                    .iter()
                    .map(|(state, count)| format!("{count} {state}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let mut lines = vec![
                if meta.key.is_empty() {
                    "Configuration file"
                } else {
                    "Configuration section"
                }
                .to_owned(),
                String::new(),
                meta.label.clone(),
                String::new(),
                format!("Application: {}", meta.application),
                format!("Path: {}", meta.config.path),
                format!("Format: {}", meta.config.format),
                format!("Configuration: selected {selected}/{total} managed options"),
                format!("Workspace state: {state_text}"),
                String::new(),
                "Managed values (workspace -> desired):".to_owned(),
            ];
            if leaves.is_empty() {
                lines.push("none declared".to_owned());
            }
            for leaf in &leaves {
                let Params::Structured { key, value, .. } = &leaf.params else {
                    continue;
                };
                let live = self.state_item(&leaf.id);
                lines.push(String::new());
                lines.push(format!(
                    "{}  [{}]",
                    key.join("."),
                    live["state"].as_str().unwrap_or("unknown")
                ));
                lines.extend(Self::value_lines(value, live));
            }
            lines.push(String::new());
            lines.push(format!("ID: {identity}"));
            return lines.join("\n");
        }
        let Some(entry) = entry else {
            let group = self.settings.setup.groups.iter().find(|group| group.id == identity);
            let mut lines = vec![group.map_or_else(|| "Workspace setup".to_owned(), |group| group.label.clone())];
            if let Some(group) = group {
                lines.push(group.description.clone());
            }
            if identity == "repo" {
                let sort = match self.settings.repo.sort.as_str() {
                    "recent" => "Recent first",
                    "created" => "Newest created first",
                    "name" => "Name",
                    other => other,
                };
                lines.push(format!(
                    "Owner: {}",
                    self.settings.repo.owner.as_deref().unwrap_or("not configured")
                ));
                lines.push(format!("Sort: {sort}"));
                if !self.catalog.repository_error.is_empty() {
                    lines.push(self.catalog.repository_error.clone());
                }
            }
            // The keys are in Menu › Help.
            lines.push(String::new());
            lines.push(format!(
                "State registry: {}",
                self.state["state_path"].as_str().unwrap_or("None")
            ));
            if let Some(id) = &self.catalog.snapshot_id {
                lines.push(format!("Catalog: {id}"));
            }
            return lines.join("\n");
        };
        let live = self.state_item(&entry.id);
        let state = live["state"].as_str();
        let status = self.status(entry);
        let application = entry.application();
        let mut lines = vec![
            format!("Status: {status}"),
            String::new(),
            entry.label.clone(),
            entry.description.clone(),
            String::new(),
            format!("Availability: {}", self.availability(&entry.id)),
            format!(
                "{}{}",
                if application.is_some() {
                    "Application state: "
                } else {
                    "Workspace state: "
                },
                state.unwrap_or("unknown")
            ),
        ];
        if application.is_some() {
            let (selected, total) = self.config_selection_count(&entry.id);
            lines.push(format!(
                "Install/update: {}",
                if self.selected.contains(&entry.id) {
                    "selected"
                } else {
                    "not selected"
                }
            ));
            lines.push(if total > 0 {
                format!("Configuration: selected {selected}/{total} managed options")
            } else {
                "Configuration: no managed options".to_owned()
            });
        }
        if matches!(entry.params, Params::Backup) {
            lines.push(String::new());
            lines.extend(self.backup_details());
        }
        let changed = strings(&live["changed_since_apply"]);
        if !changed.is_empty() {
            lines.push("Changed since last apply:".to_owned());
            lines.extend(changed.iter().map(|path| format!("~/{path}")));
            if truthy(&live["will_overwrite"]) {
                lines.push(
                    "Changed files will be backed up before replacement with HomeStack configuration.".to_owned(),
                );
            }
        }
        if let Some(checked) = self.state["checked_at"].as_str().filter(|text| !text.is_empty()) {
            lines.push(format!("State checked: {checked}"));
        }
        for (key, label) in [
            ("first_managed_at", "First managed: "),
            ("installed_at", "Installed by HomeStack: "),
            ("last_applied_at", "Last applied: "),
            ("last_snapshot", "Last snapshot: "),
        ] {
            if let Some(value) = live[key].as_str().filter(|text| !text.is_empty()) {
                lines.push(format!("{label}{value}"));
            }
        }
        if truthy(&live["detail"]) {
            let detail = live["detail"]
                .as_str()
                .map_or_else(|| live["detail"].to_string(), str::to_owned);
            lines.push(format!("State detail: {detail}"));
        }
        for file in live["files"].as_array().into_iter().flatten() {
            if !truthy(&file["exists"]) {
                continue;
            }
            let sha = file["sha256"].as_str().unwrap_or_default();
            lines.push(format!("Managed path: ~/{}", file["path"].as_str().unwrap_or("None")));
            lines.push(format!(
                "SHA-256: {}",
                if sha.is_empty() {
                    "unknown".to_owned()
                } else {
                    sha.to_owned()
                }
            ));
            if truthy(&file["birthtime_ns"]) {
                lines.push(format!("Created: {}", mtime_text(&file["birthtime_ns"])));
            }
            if !file["size"].is_null() {
                lines.push(format!("Size: {} bytes", file["size"]));
            }
            lines.push(format!("Modified: {}", mtime_text(&file["mtime_ns"])));
        }
        lines.push(String::new());
        lines.push(format!("ID: {}", entry.id));
        match &entry.params {
            Params::Application(p) => {
                lines.push(format!("Interpreter: {}", p.interpreter));
                lines.push(format!("Interaction: {}", p.interaction));
                lines.push(format!(
                    "Prerequisites: {}",
                    if p.prerequisites.is_empty() {
                        "none".to_owned()
                    } else {
                        p.prerequisites.join(", ")
                    }
                ));
                lines.push("Executable paths:".to_owned());
                lines.extend(p.bin_dirs.iter().cloned());
                lines.push("Backup paths:".to_owned());
                if p.backup_paths.is_empty() {
                    lines.push("none declared".to_owned());
                } else {
                    lines.extend(p.backup_paths.iter().cloned());
                }
                let known = crate::setup::definitions::defaults()
                    .iter()
                    .any(|item| item.application().is_some_and(|known| known.command == p.command));
                lines.push(format!(
                    "Command: {}",
                    if known {
                        p.command.clone()
                    } else {
                        "[Custom payload withheld: review command in the local configuration; it may contain secrets]"
                            .to_owned()
                    }
                ));
                if let Some(validation) = &p.validation {
                    lines.push("Application validator:".to_owned());
                    lines.push(format!("Type: {}", validation.kind));
                    lines.push(format!(
                        "Path: {}",
                        if validation.path.is_empty() {
                            "none".to_owned()
                        } else {
                            validation.path.join(".")
                        }
                    ));
                    lines.push(format!(
                        "Accepted: {}",
                        if validation.accepted.is_empty() {
                            "exit code 0".to_owned()
                        } else {
                            validation.accepted.join(", ")
                        }
                    ));
                }
                if !p.config_files.is_empty() {
                    lines.push("Structured configuration files:".to_owned());
                    for config in &p.config_files {
                        lines.push(format!("{} ({})", config.path, config.format));
                    }
                }
                if !p.files.is_empty() {
                    lines.push("Desktop file copies:".to_owned());
                    lines.extend(p.files.iter().cloned());
                }
            }
            Params::Herdr => {
                lines.extend(
                    [
                        "User service: herdr.service",
                        "Updates before every service start, including automatic restarts.",
                        "Linger starts the service at VM boot without a user login.",
                        "Existing servers are left running; an unmanaged server is replaced only after VM reboot.",
                    ]
                    .map(str::to_owned),
                );
                lines.push(format!("Desktop label: {}", self.target.name));
                lines.push(
                    "Desktop registration opens separate SSH authentication; touch your YubiKey/security key if prompted."
                        .to_owned(),
                );
            }
            Params::Repository { repository } => {
                let (owner, name) = repository.split_once('/').unwrap_or_default();
                lines.push(format!("Owner: {owner}"));
                lines.push(format!("Repository: {name}"));
                lines.push(String::new());
                lines.push("Checkout:".to_owned());
                lines.push(format!(
                    "{}/{name}",
                    self.settings.repo.checkout_root.trim_end_matches('/')
                ));
                if let Some(times) = self.catalog.timestamps.get(&entry.id).filter(|times| !times.is_empty()) {
                    let field = |key: &str, missing: &str| {
                        times
                            .get(key)
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                            .unwrap_or(missing)
                            .to_owned()
                    };
                    lines.push(String::new());
                    lines.push(format!("Created: {}", field("created_at", "unknown")));
                    lines.push(format!("Last push: {}", field("pushed_at", "none")));
                }
                lines.push(String::new());
                lines.push(format!("Repository: {repository}"));
            }
            Params::File { path, application } => {
                if let Some(application) = application {
                    lines.push(format!("Application: {application}"));
                    lines.push(String::new());
                }
                lines.push("Source (desktop):".to_owned());
                lines.push(path.clone());
                lines.push(String::new());
                lines.push("Destination (workspace):".to_owned());
                lines.extend(
                    write_paths(&self.settings, entry)
                        .iter()
                        .map(|path| format!("~/{path}")),
                );
            }
            _ => {
                let differs = strings(&live["differs"]);
                lines.push("Managed paths:".to_owned());
                lines.extend(write_paths(&self.settings, entry).iter().map(|path| {
                    if differs.contains(path) {
                        format!("~/{path}  *")
                    } else {
                        format!("~/{path}")
                    }
                }));
                if !differs.is_empty() {
                    lines.push("* differs from what HomeStack writes; applying replaces the whole file".to_owned());
                }
            }
        }
        if let Params::Structured {
            application,
            config,
            key,
            value,
        } = &entry.params
        {
            lines.push(format!("Application: {application}"));
            lines.push(format!("Configuration file: {}", config.path));
            lines.push(format!("Format: {}", config.format));
            lines.push(format!("Key path: {}", key.join(".")));
            lines.push(String::new());
            lines.extend(Self::value_lines(value, live));
        }
        if !entry.depends_on.is_empty() {
            lines.push(String::new());
            lines.push(format!("Requires explicit selection: {}", entry.depends_on.join(", ")));
        }
        lines.join("\n")
    }

    /// What applying `entry` does (Python's `Status:` of the details).
    #[must_use]
    pub fn status(&self, entry: &Entry) -> &'static str {
        let live = self.state_item(&entry.id);
        let state = live["state"].as_str();
        let environment = matches!(entry.params, Params::Environment { .. });
        if matches!(entry.params, Params::Structured { .. }) {
            match state {
                Some("matching") => "NO CHANGES",
                Some("missing") => "INSTALL",
                Some("different") => "UPDATE",
                Some("unavailable") => "UNAVAILABLE",
                _ => "UNKNOWN",
            }
        } else if environment && truthy(&live["ready"]) && truthy(&live["changed_since_apply"]) {
            "MODIFIED"
        } else if environment && truthy(&live["ready"]) {
            "NO CHANGES"
        } else if truthy(&live["ready"]) || truthy(&live["will_overwrite"]) {
            "UPDATE"
        } else if !truthy(live) || state.unwrap_or("unknown").starts_with("unknown") {
            "UNKNOWN"
        } else {
            "INSTALL"
        }
    }

    /// The STATE column of a row (mockup 23): what the workspace has, and
    /// for a selected item what applying it does. None for a group or a
    /// configuration file or section.
    #[must_use]
    pub fn row_state(&self, identity: &str) -> Option<(String, Tone)> {
        let entry = self.entry(identity)?;
        let live = self.state_item(identity);
        let state = live["state"].as_str().unwrap_or("unknown");
        let (mark, tone) = if !self.available(entry) {
            ("✗", Tone::Dim)
        } else if truthy(&live["ready"]) {
            ("✓", Tone::Green)
        } else if truthy(&live["will_overwrite"]) {
            ("!", Tone::Yellow)
        } else if state.starts_with("unknown") || state == "unavailable" {
            ("?", Tone::Dim)
        } else {
            ("○", Tone::Dim)
        };
        // "needs update" read as an offer; it means the files differ and
        // applying replaces them (the owner, 2026-10-15).
        let shown = if state == "needs update" { "differs" } else { state };
        let mut text = format!("{mark} {shown}");
        if !self.available(entry) {
            text = format!("{mark} {}", self.availability(identity));
        }
        if self.selected.contains(identity) {
            let action = match self.status(entry) {
                "INSTALL" => "install",
                "UPDATE" | "MODIFIED" => "update",
                "NO CHANGES" => "verify",
                _ => "apply",
            };
            return Some((
                format!("{text} → {action}"),
                if action == "update" { Tone::Red } else { tone },
            ));
        }
        Some((text, tone))
    }

    /// The tone of the first line of the details (Python colours the
    /// status).
    #[must_use]
    pub fn status_tone(details: &str) -> Option<Tone> {
        let status = details.lines().next()?.strip_prefix("Status: ")?;
        Some(match status {
            "UPDATE" => Tone::Red,
            "INSTALL" | "NO CHANGES" => Tone::Green,
            _ => Tone::Yellow,
        })
    }

    pub fn notify(&mut self, text: &str, error: bool) {
        self.notice = Some(Notice {
            text: text.to_owned(),
            error,
        });
    }

    /// Python `toggle_node`: Space (or `select`, the key 0, for the branch).
    pub fn toggle(&mut self, identity: &str, select: bool) {
        if self.busy {
            return;
        }
        let Some(node) = self.nodes.get(identity).cloned() else {
            return;
        };
        let mut identity = identity.to_owned();
        if !select && node.children.is_empty() && self.selected.contains(&identity) {
            self.selected.remove(&identity);
            self.rebuild();
            return;
        }
        if select
            && node.children.is_empty()
            && let Some(parent) = &node.parent
        {
            identity.clone_from(parent);
        }
        let ids: BTreeSet<String> = self
            .descendants(&identity, !self.filter.is_empty())
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        if ids.is_empty() {
            // A removed or unavailable selection stays deselectable.
            for id in self.descendant_ids(&identity, false) {
                self.selected.remove(&id);
            }
        } else if !select && ids.is_subset(&self.selected) {
            for id in &ids {
                self.selected.remove(id);
            }
        } else {
            self.selected.extend(ids);
        }
        self.rebuild();
    }

    fn move_cursor(&mut self, step: isize) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let at = rows.iter().position(|row| row.id == self.cursor).unwrap_or(0);
        let next = at.saturating_add_signed(step).min(rows.len() - 1);
        self.highlight(&rows[next].id.clone());
    }

    /// Highlight `identity` (its details from the top).
    pub fn highlight(&mut self, identity: &str) {
        if self.cursor != identity {
            self.details_scroll = 0;
        }
        identity.clone_into(&mut self.cursor);
    }

    /// Show tab `index` (a group, or Services).
    pub fn show_section(&mut self, index: usize) {
        let count = self.groups().len();
        if count == 0 {
            return;
        }
        self.section = index % count;
        if let Some(group) = self.group_id(self.section) {
            self.highlight(&group);
        }
        self.details_scroll = 0;
    }

    /// Expand (`open`) or collapse a branch (Python `action_expand`,
    /// `action_collapse`, a click on the arrow).
    pub fn set_open(&mut self, identity: &str, open: bool) {
        if self.is_group(identity) {
            return;
        }
        self.expanded.insert(identity.to_owned(), open);
    }

    /// Python `review`: the plan of the selection, or why there is none.
    pub fn review(&mut self) {
        if self.busy {
            return;
        }
        if self.selected.is_empty() {
            self.notify("No setup actions selected.", false);
            return;
        }
        let selected: Vec<Entry> = self
            .entries
            .iter()
            .filter(|entry| self.selected.contains(&entry.id))
            .cloned()
            .collect();
        if selected.iter().any(|entry| !self.available(entry)) {
            self.notify(
                "A selected entry was removed or is unavailable. Deselect it before applying.",
                true,
            );
            return;
        }
        match build_plan(
            &self.settings,
            &self.target,
            &selected,
            self.catalog.snapshot_id.clone(),
            false,
            false,
        ) {
            Ok(plan) => {
                let lines = self.review_lines(&plan);
                self.stage = Stage::Review {
                    lines,
                    scroll: 0,
                    apply: true,
                    plan: Box::new(plan),
                };
            }
            Err(error) => self.notify(&error.0, true),
        }
    }

    /// Python `show_review`: what each action will do.
    #[must_use]
    pub fn review_lines(&self, plan: &Plan) -> Vec<String> {
        let mut lines = vec![
            format!("Apply to {}?", self.target_description()),
            "All selected actions will be preflighted before writes. Existing declared configuration is snapshotted before overwrite.".to_owned(),
            String::new(),
        ];
        for entry in &plan.entries {
            let live = self.state_item(&entry.id);
            let action = match &entry.params {
                Params::Structured { .. } => match live["state"].as_str() {
                    Some("matching") => "Verify declared value; no file write is needed",
                    Some("missing") => "Add declared value",
                    Some("different") => "Patch declared value",
                    Some("unavailable") => "Blocked: configuration path is unavailable",
                    _ => "Inspect and patch declared value",
                },
                Params::Backup if self.target.local => "Reconcile local BK assets and user timer",
                Params::Backup => "Reconcile BK assets, user timer and restricted SSH retrieval keys",
                Params::Herdr => {
                    "Install if missing; enable user service and linger; preserve running sessions; register desktop profile"
                }
                Params::Environment { .. } if truthy(&live["changed_since_apply"]) => {
                    "Back up modified shell files; overwrite configuration"
                }
                Params::Environment { .. } => "Verify shell configuration; write only if changes are needed",
                _ if truthy(&live["ready"]) => "Update / reapply existing state",
                _ if truthy(&live["will_overwrite"]) => "Overwrite existing configuration",
                _ => "Install / configure",
            };
            let hidden = if !self.filter.is_empty() && !self.identity_visible(&entry.id) {
                " [hidden by filter]"
            } else {
                ""
            };
            lines.push(format!("{}: {} ({}){hidden}", entry.group, entry.label, entry.id));
            lines.push(entry.description.clone());
            lines.push(format!("Action: {action}"));
            lines.push(format!(
                "Requires selection: {}",
                if entry.depends_on.is_empty() {
                    "none".to_owned()
                } else {
                    entry.depends_on.join(", ")
                }
            ));
            if matches!(entry.params, Params::Backup) {
                lines.extend(self.backup_details());
            }
            if let Params::Structured { key, value, .. } = &entry.params {
                lines.push(format!("Desired key: {}", key.join(".")));
                lines.push("Desired value:".to_owned());
                lines.extend(pretty_toml(value, 0).lines().map(str::to_owned));
            }
            let changed = strings(&live["changed_since_apply"]);
            if !changed.is_empty() {
                let paths: Vec<String> = changed.iter().map(|path| format!("~/{path}")).collect();
                lines.push(format!("Changed since last apply: {}", paths.join(", ")));
            }
            if let Some(p) = entry.application()
                && truthy(&live["ready"])
                && p.backup_paths.is_empty()
            {
                lines.push(
                    "WARNING: no application backup_paths are declared; external installer changes outside HomeStack-managed files cannot be restored."
                        .to_owned(),
                );
            }
            lines.push(String::new());
        }
        lines
    }

    /// Python `review_answer(True)`: the run starts.
    fn start_run(&mut self, plan: Box<Plan>) -> Ask {
        self.pending_plan = Some(*plan);
        self.busy = true;
        self.cancel_requested = false;
        let mut lines = Vec::new();
        let first = clean_line(None, &format!("Start setup for {}", self.target_description()));
        lines.push(first);
        self.stage = Stage::Running {
            lines,
            finished: false,
            title: "Setup running".to_owned(),
            auth_wait: false,
        };
        Ask::Execute
    }

    /// Python `append_activity`.
    pub fn activity(&mut self, identity: Option<&str>, message: &str) {
        if let Stage::Running {
            lines,
            finished,
            auth_wait,
            ..
        } = &mut self.stage
        {
            let line = clean_line(identity, message);
            if !line.is_empty() {
                lines.push(line);
                *auth_wait = !*finished && identity == Some("herdr") && message == AUTH_NOTICE;
            }
        }
    }

    /// Python `update_progress`.
    pub fn progress(&mut self, identity: &str, state: &str) {
        self.action_states.insert(identity.to_owned(), state.to_owned());
        self.last_progress = format!("{identity}: {state}");
        "connected".clone_into(&mut self.ssh_status);
        self.rebuild();
    }

    /// Python `finished`: the result, the state read after it.
    pub fn finished(&mut self, result: Value, state: Option<Value>) {
        self.busy = false;
        self.last_progress.clear();
        if let Some(state) = state {
            self.state = state;
        }
        let rows = result["results"].as_array().cloned().unwrap_or_default();
        self.action_states = rows
            .iter()
            .filter_map(|item| Some((item["id"].as_str()?.to_owned(), item["status"].as_str()?.to_owned())))
            .collect();
        for item in &rows {
            if matches!(item["status"].as_str(), Some("succeeded" | "already-ready"))
                && let Some(id) = item["id"].as_str()
            {
                self.selected.remove(id);
            }
        }
        "connected".clone_into(&mut self.ssh_status);
        self.rebuild();
        if let Stage::Running {
            lines,
            finished,
            title,
            auth_wait,
        } = &mut self.stage
        {
            let status = if result["ok"] == true {
                "Setup finished"
            } else if rows
                .iter()
                .any(|row| matches!(row["status"].as_str(), Some("succeeded" | "already-ready")))
            {
                "Setup partially completed"
            } else {
                "Setup stopped"
            };
            *finished = true;
            *auth_wait = false;
            status.clone_into(title);
            lines.push(String::new());
            lines.push(status.to_owned());
            if result["snapshot"]["created"] == true {
                lines.push(format!(
                    "Snapshot: {}",
                    result["snapshot"]["path"].as_str().unwrap_or("None")
                ));
            }
            for item in &rows {
                lines.push(format!(
                    "{}: {} — {}",
                    item["label"].as_str().unwrap_or_default(),
                    item["status"].as_str().unwrap_or_default(),
                    item["detail"].as_str().unwrap_or_default()
                ));
            }
        }
        self.result = Some(result);
    }

    /// The state read again (F5).
    pub fn replace_state(&mut self, state: Value) {
        self.state = state;
        "connected".clone_into(&mut self.ssh_status);
        self.rebuild();
    }

    /// Python `replace_catalog`: a removed selected item stays, marked
    /// removed, until it is deselected.
    pub fn replace_catalog(&mut self, mut catalog: Catalog, state: Option<Value>) {
        if self.busy {
            return;
        }
        let current: BTreeSet<String> = catalog.entries.iter().map(|entry| entry.id.clone()).collect();
        let removed: Vec<Entry> = self
            .catalog
            .entries
            .iter()
            .filter(|entry| self.selected.contains(&entry.id) && !current.contains(&entry.id))
            .cloned()
            .collect();
        for entry in &removed {
            catalog.availability.insert(entry.id.clone(), "removed".to_owned());
        }
        catalog.entries.extend(removed.iter().cloned());
        self.catalog = catalog;
        if let Some(state) = state {
            self.state = state;
        }
        self.rebuild();
        if !removed.is_empty() {
            self.notify(
                "Selected entries were removed. They remain selected and block review until deselected.",
                true,
            );
        }
    }

    /// The spinner's character now.
    fn spinner(&self) -> char {
        super::draw::SPINNER[self.frame % super::draw::SPINNER.len()]
    }

    /// Whether the spinner turns (connecting, a run under way, or a
    /// repository's manifests being read).
    #[must_use]
    pub fn animating(&self) -> bool {
        match &self.stage {
            Stage::Loading { .. } | Stage::Running { finished: false, .. } => true,
            Stage::Tree => self.in_services() && self.discoveries.values().any(Option::is_none),
            _ => false,
        }
    }

    /// Whether the run is under way.
    #[must_use]
    pub fn running(&self) -> bool {
        matches!(self.stage, Stage::Running { finished: false, .. })
    }

    /// The rows of the Services group: the catalog's repositories, those
    /// with deployed services first, each opened one followed by its
    /// manifest files.
    #[must_use]
    pub fn service_rows(&self) -> Vec<ServiceRow> {
        let filter = self.filter_text();
        let mut repositories: Vec<&String> = self
            .catalog
            .entries
            .iter()
            .filter_map(|entry| match &entry.params {
                Params::Repository { repository } if entry.group == "repo" => Some(repository),
                _ => None,
            })
            .filter(|repository| filter.is_empty() || repository.to_lowercase().contains(&filter))
            .collect();
        repositories.sort_by_key(|repository| !self.has_repository(repository));
        let mut rows = Vec::new();
        for repository in repositories {
            rows.push(ServiceRow {
                repository: repository.clone(),
                file: None,
            });
            if self.services_open.contains(repository)
                && let Some(Ok(found)) = self.discovery(repository)
            {
                rows.extend((0..found.files.len()).map(|index| ServiceRow {
                    repository: repository.clone(),
                    file: Some(index),
                }));
            }
        }
        rows
    }

    /// The highlighted row of the Services group.
    #[must_use]
    pub fn service_cursor_row(&self) -> Option<ServiceRow> {
        let rows = self.service_rows();
        let at = self.services_cursor.min(rows.len().checked_sub(1)?);
        rows.into_iter().nth(at)
    }

    /// The default branch the catalog reported for `repository`.
    fn default_branch(&self, repository: &str) -> Option<String> {
        let entry =
            self.catalog.entries.iter().find(
                |entry| matches!(&entry.params, Params::Repository { repository: known } if known == repository),
            )?;
        self.catalog
            .timestamps
            .get(&entry.id)?
            .get("default_branch")?
            .as_str()
            .map(str::to_owned)
    }

    fn read_services(&mut self, repository: String) -> Ask {
        let branch = self.default_branch(&repository);
        self.discoveries.insert(repository.clone(), None);
        self.services_request = Some((repository, branch));
        Ask::Services
    }

    /// What the worker read of `repository`.
    pub fn services_read(&mut self, repository: String, result: Result<Discovery, String>) {
        self.discoveries.insert(repository, Some(result));
    }

    /// The discovery the services view shows, once read.
    #[must_use]
    pub fn discovery(&self, repository: &str) -> Option<&Result<Discovery, String>> {
        self.discoveries.get(repository).and_then(Option::as_ref)
    }

    /// Whether a container runs a service of `repository`.
    fn has_repository(&self, repository: &str) -> bool {
        self.services
            .iter()
            .any(|entry| entry.service.as_ref().is_some_and(|s| s.repository == repository))
    }

    /// The label of a Services row and its STATE: its containers in Proxmox.
    #[must_use]
    pub fn service_row_text(&self, row: &ServiceRow) -> (String, (String, Tone)) {
        let deployed = self.has_repository(&row.repository);
        let read = self.discoveries.get(&row.repository);
        let Some(index) = row.file else {
            let state = match read {
                _ if deployed => ("deployed".to_owned(), Tone::Green),
                None => (String::new(), Tone::Plain),
                Some(None) => (format!("{} reading GitHub", self.spinner()), Tone::Yellow),
                Some(Some(Err(_))) => ("error".to_owned(), Tone::Red),
                Some(Some(Ok(found))) => {
                    let valid = found.services().count();
                    let invalid = found.files.len() - valid;
                    match (valid, invalid) {
                        (0, 0) => ("no services".to_owned(), Tone::Dim),
                        (valid, 0) => (
                            format!("{valid} service{}", if valid == 1 { "" } else { "s" }),
                            Tone::Plain,
                        ),
                        (valid, invalid) => (format!("{valid} valid, {invalid} invalid"), Tone::Yellow),
                    }
                }
            };
            return (row.repository.clone(), state);
        };
        let Some(Some(Ok(found))) = read else {
            return (String::new(), (String::new(), Tone::Plain));
        };
        let Some(file) = found.files.get(index) else {
            return (String::new(), (String::new(), Tone::Plain));
        };
        let (label, tone) = service_row(file);
        let state = match &file.manifest {
            Err(_) => ("invalid".to_owned(), tone),
            Ok(checked) if checked.manifest.is_none() => ("errors".to_owned(), tone),
            Ok(checked) => {
                let found = instances(
                    &self.services,
                    &row.repository,
                    checked.id.as_deref().unwrap_or_default(),
                );
                let running = found.iter().filter(|entry| entry.status == "running").count();
                match (found.len(), running) {
                    (0, _) => ("not deployed".to_owned(), Tone::Dim),
                    (1, 1) => ("running".to_owned(), Tone::Green),
                    (1, _) => ("stopped".to_owned(), Tone::Yellow),
                    (all, running) if all == running => (format!("{all} running"), Tone::Green),
                    (all, running) => (format!("{running} of {all} running"), Tone::Yellow),
                }
            }
        };
        (label, state)
    }

    /// The details of the highlighted Services row.
    #[must_use]
    pub fn service_detail_lines(&self) -> (Vec<(String, Tone)>, Vec<usize>) {
        let Some(row) = self.service_cursor_row() else {
            let reason = if self.catalog.repository_error.is_empty() {
                "No repositories match.".to_owned()
            } else {
                self.catalog.repository_error.clone()
            };
            let lines = [SERVICES.to_owned(), String::new(), reason]
                .into_iter()
                .map(|line| (line, Tone::Plain))
                .collect();
            return (lines, Vec::new());
        };
        let read = self.discovery(&row.repository);
        if let (Some(index), Some(Ok(found))) = (row.file, read)
            && let Some(file) = found.files.get(index)
        {
            return service_details(found, file, &self.services, &self.manifest_open);
        }
        let mut lines = vec![row.repository.clone()];
        if let Some(branch) = self.default_branch(&row.repository) {
            lines.push(format!("Branch: {branch}"));
        }
        let deployed = self
            .services
            .iter()
            .filter(|entry| entry.service.as_ref().is_some_and(|s| s.repository == row.repository))
            .count();
        lines.push(format!("Instances: {deployed}"));
        match self.discoveries.get(&row.repository) {
            None => lines.push("Services: not read; → or ENTER reads .homestack/*.yaml".to_owned()),
            Some(None) => lines.push(format!(
                "Services: {} reading .homestack/*.yaml on GitHub",
                self.spinner()
            )),
            Some(Some(Err(error))) => lines.push(format!("Error: {error}")),
            Some(Some(Ok(found))) => {
                lines.push(format!("Revision: {} @ {}", found.branch, short(&found.commit)));
                if let Some(problem) = &found.problem {
                    lines.push(format!("Problem: {problem}"));
                }
                if found.files.is_empty() {
                    lines.push("Services: none (no .homestack/*.yaml)".to_owned());
                }
                for file in &found.files {
                    lines.push(match &file.manifest {
                        Ok(checked) if checked.manifest.is_some() => {
                            format!("Service: {} ({})", checked.id.as_deref().unwrap_or_default(), file.path)
                        }
                        Ok(_) => format!("Errors: {}", file.path),
                        Err(_) => format!("Invalid: {}", file.path),
                    });
                }
            }
        }
        let lines = lines
            .into_iter()
            .map(|line| {
                let tone =
                    if line.starts_with("Errors: ") || line.starts_with("Invalid: ") || line.starts_with("Error: ") {
                        Tone::Red
                    } else {
                        Tone::Plain
                    };
                (line, tone)
            })
            .collect();
        (lines, Vec::new())
    }

    /// Whether a manifest file of a repository is highlighted.
    #[must_use]
    pub fn on_manifest(&self) -> bool {
        self.service_cursor_row().is_some_and(|row| row.file.is_some())
    }

    /// The top-level keys of the highlighted manifest, when it was read.
    fn manifest_sections(&self) -> Vec<String> {
        let Some(ServiceRow {
            repository,
            file: Some(index),
        }) = self.service_cursor_row()
        else {
            return Vec::new();
        };
        match self.discovery(&repository) {
            Some(Ok(found)) => match found.files.get(index).map(|file| &file.manifest) {
                Some(Ok(checked)) => section_names(checked),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// Open or close the section `index` of the highlighted manifest and
    /// highlight it.
    pub fn toggle_manifest_section(&mut self, index: usize) {
        let Some(name) = self.manifest_sections().into_iter().nth(index) else {
            return;
        };
        self.manifest_focus = Some(index);
        if !self.manifest_open.remove(&name) {
            self.manifest_open.insert(name);
        }
    }

    /// A key while the highlighted manifest's sections are browsed; `None`
    /// when it is not one of theirs.
    fn manifest_key(&mut self, focus: usize, code: KeyCode) -> Option<()> {
        let last = self.manifest_sections().len().saturating_sub(1);
        match code {
            KeyCode::Up => self.manifest_focus = Some(focus.saturating_sub(1)),
            KeyCode::Down => self.manifest_focus = Some((focus + 1).min(last)),
            KeyCode::Char(' ') | KeyCode::Right => self.toggle_manifest_section(focus),
            KeyCode::Left => {
                if let Some(name) = self.manifest_sections().get(focus) {
                    self.manifest_open.remove(name);
                }
            }
            KeyCode::Esc => self.manifest_focus = None,
            _ => return None,
        }
        // The details follow the highlighted section.
        self.details_scroll = 0;
        Some(())
    }

    /// Move the Services highlight by `step` rows.
    fn move_service_cursor(&mut self, step: isize) {
        let count = self.service_rows().len();
        if count == 0 {
            return;
        }
        let at = self.services_cursor.min(count - 1);
        self.services_cursor = at.saturating_add_signed(step).min(count - 1);
        self.details_scroll = 0;
        self.manifest_open.clear();
        self.manifest_focus = None;
    }

    /// Open a repository of the Services group: its manifests are read the
    /// first time.
    fn open_repository(&mut self, repository: &str) -> Option<Ask> {
        self.services_open.insert(repository.to_owned());
        (!self.discoveries.contains_key(repository)).then(|| self.read_services(repository.to_owned()))
    }

    /// The highlighted service when it can be deployed: a valid manifest
    /// with no deployment, or one an unfinished run left.
    #[must_use]
    pub fn service_source(&self) -> Option<crate::services::deploy::Source> {
        let ServiceRow {
            repository,
            file: Some(index),
        } = self.service_cursor_row()?
        else {
            return None;
        };
        let Some(Ok(found)) = self.discovery(&repository) else {
            return None;
        };
        let file = found.files.get(index)?;
        let manifest = file.valid()?;
        Some(crate::services::deploy::Source {
            repository,
            branch: found.branch.clone(),
            commit: found.commit.clone(),
            path: file.path.clone(),
            blob: file.blob.clone(),
            manifest: manifest.clone(),
        })
    }

    /// `d` deploys new instances of any valid service.
    #[must_use]
    pub fn deployable(&self) -> Option<crate::services::deploy::Source> {
        self.service_source()
    }

    /// `d`: deploy the highlighted service.
    fn deploy(&mut self) -> Option<Ask> {
        self.deploy_request = Some(self.deployable()?);
        Some(Ask::Deploy)
    }

    /// Open or close the highlighted repository of the Services group.
    fn toggle_repository(&mut self) -> Option<Ask> {
        let Some(ServiceRow { repository, file: None }) = self.service_cursor_row() else {
            return None;
        };
        if self.services_open.remove(&repository) {
            return None;
        }
        self.open_repository(&repository)
    }

    /// A key in the Services group: read only, nothing is selected.
    fn services_key(&mut self, key: KeyEvent) -> Option<Ask> {
        if self.filtering {
            match key.code {
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => self.filter.push(c),
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filtering = false;
                }
                KeyCode::Enter | KeyCode::Down | KeyCode::Up | KeyCode::Tab => self.filtering = false,
                _ => {}
            }
            self.services_cursor = 0;
            return None;
        }
        if let Some(focus) = self.manifest_focus
            && self.manifest_key(focus, key.code).is_some()
        {
            return None;
        }
        let row = self.service_cursor_row();
        match key.code {
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) && !self.busy => {
                return Some(Ask::Catalog);
            }
            KeyCode::Up => self.move_service_cursor(-1),
            KeyCode::Down => self.move_service_cursor(1),
            KeyCode::Home => self.move_service_cursor(isize::MIN / 2),
            KeyCode::End => self.move_service_cursor(isize::MAX / 2),
            KeyCode::PageUp => self.details_scroll = self.details_scroll.saturating_sub(10),
            KeyCode::PageDown => self.details_scroll += 10,
            KeyCode::Right | KeyCode::Tab => self.show_section(self.section + 1),
            KeyCode::Left | KeyCode::BackTab => self.show_section(self.section + self.groups().len() - 1),
            KeyCode::Char('f') => self.filtering = true,
            KeyCode::Char('d') => return self.deploy(),
            KeyCode::Char(' ') if !self.manifest_sections().is_empty() => {
                self.manifest_focus = Some(0);
                self.details_scroll = 0;
            }
            KeyCode::Enter if row.as_ref().is_some_and(|row| row.file.is_some()) => return self.deploy(),
            KeyCode::Enter => return self.toggle_repository(),
            KeyCode::F(5) => {
                // Not while the last read is still under way.
                if let Some(ServiceRow { repository, .. }) = row
                    && !matches!(self.discoveries.get(&repository), Some(None))
                {
                    self.services_open.insert(repository.clone());
                    return Some(self.read_services(repository));
                }
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.services_cursor = 0;
            }
            KeyCode::Esc => return Some(Ask::Close),
            _ => {}
        }
        None
    }

    /// Handle a key; what the worker has to do.
    pub fn key(&mut self, key: KeyEvent) -> Option<Ask> {
        self.notice = None;
        if matches!(self.stage, Stage::Tree) && self.in_services() {
            return self.services_key(key);
        }
        match &mut self.stage {
            Stage::Loading { .. } => (key.code == KeyCode::Esc).then_some(Ask::Close),
            Stage::Failed { .. } => matches!(key.code, KeyCode::Esc | KeyCode::Enter).then_some(Ask::Close),
            Stage::Review {
                lines, scroll, apply, ..
            } => {
                match key.code {
                    KeyCode::Esc => self.stage = Stage::Tree,
                    KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => *apply = !*apply,
                    KeyCode::Up => *scroll = scroll.saturating_sub(1),
                    KeyCode::Down => *scroll = (*scroll + 1).min(lines.len().saturating_sub(1)),
                    KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                    KeyCode::PageDown => *scroll = (*scroll + 10).min(lines.len().saturating_sub(1)),
                    KeyCode::Char('a') | KeyCode::Enter if *apply || key.code == KeyCode::Char('a') => {
                        let Stage::Review { plan, .. } = std::mem::replace(&mut self.stage, Stage::Tree) else {
                            return None;
                        };
                        return Some(self.start_run(plan));
                    }
                    KeyCode::Enter => self.stage = Stage::Tree,
                    _ => {}
                }
                None
            }
            Stage::Running { finished, lines, .. } => match key.code {
                KeyCode::Esc | KeyCode::Enter if *finished => {
                    self.stage = Stage::Tree;
                    None
                }
                KeyCode::Esc if !self.cancel_requested => {
                    self.cancel_requested = true;
                    lines.push("Cancellation requested".to_owned());
                    self.notice = Some(Notice {
                        text: "Cancellation requested; waiting for the current operation to return. Interactive installers use Ctrl+C in their terminal.".to_owned(),
                        error: false,
                    });
                    Some(Ask::Cancel)
                }
                _ => None,
            },
            Stage::Tree => self.tree_key(key),
        }
    }

    fn tree_key(&mut self, key: KeyEvent) -> Option<Ask> {
        if self.filtering {
            match key.code {
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.filter.push(c);
                    self.rebuild();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.rebuild();
                }
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filtering = false;
                    self.rebuild();
                }
                KeyCode::Enter | KeyCode::Down | KeyCode::Up | KeyCode::Tab => self.filtering = false,
                _ => {}
            }
            return None;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let cursor = self.cursor.clone();
        match key.code {
            KeyCode::Char('r') if control => {
                if !self.busy {
                    return Some(Ask::Catalog);
                }
            }
            KeyCode::F(5) => {
                if !self.busy {
                    return Some(Ask::Refresh);
                }
            }
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Home => self.move_cursor(isize::MIN / 2),
            KeyCode::End => self.move_cursor(isize::MAX / 2),
            KeyCode::PageUp => self.details_scroll = self.details_scroll.saturating_sub(10),
            KeyCode::PageDown => self.details_scroll += 10,
            KeyCode::Right | KeyCode::Tab => self.show_section(self.section + 1),
            KeyCode::Left | KeyCode::BackTab => {
                let count = self.groups().len().max(1);
                self.show_section(self.section + count - 1);
            }
            // Enter opens or closes the highlighted item.
            KeyCode::Enter => {
                if self.nodes.get(&cursor).is_some_and(|node| node.branch) && !self.is_group(&cursor) {
                    let open = self.is_expanded(&cursor);
                    self.set_open(&cursor, !open);
                }
            }
            KeyCode::Char(' ') => self.toggle(&cursor, false),
            KeyCode::Char('0') => self.toggle(&cursor, true),
            KeyCode::Char('f') => self.filtering = true,
            KeyCode::Char('a') => self.review(),
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rebuild();
                } else {
                    return Some(Ask::Close);
                }
            }
            _ => {}
        }
        None
    }
}

impl super::state::App {
    /// Open `setup` over the workspaces panel, which gets the width of the
    /// storage and log panels while it is open.
    pub fn open_setup_box(&mut self, setup: SetupBox) {
        self.before_setup = Some(self.shown);
        self.shown.workspaces = true;
        self.shown.storage = false;
        self.shown.log = false;
        self.setup = Some(setup);
        self.share_services();
    }

    /// Enter or `e` on a running workspace: the setup box, connecting first.
    pub(super) fn open_setup(&mut self) -> Option<super::state::Request> {
        if self.selected_entry().is_some_and(super::acting::is_site) {
            return self.open_reinstall();
        }
        // Other containers are services, not workspaces: no setup, and no
        // update until a service workflow manages them.
        if self.selected_entry().is_some_and(|entry| entry.role == "CT") {
            return None;
        }
        if let Some(action) = &self.action {
            let line = format!("one action at a time: {} is still open", action.title());
            self.log("", &line);
            return None;
        }
        let entry = self.selected_entry().filter(|entry| entry.status == "running")?.clone();
        let target = Target {
            vmid: entry.vmid,
            name: entry.name.clone().unwrap_or_default(),
            ..Target::default()
        };
        self.open_setup_box(SetupBox::loading(Settings::default(), target, "Starting setup"));
        Some(super::state::Request::Setup(Some(entry.vmid)))
    }

    pub(super) fn setup_key(&mut self, key: KeyEvent) -> Option<super::state::Request> {
        let setup = self.setup.as_mut()?;
        let ask = setup.key(key);
        if let Some(notice) = setup.notice.clone() {
            let time = self.clock.clone();
            self.log(&time, &format!("setup: {}", notice.text));
        }
        let ask = ask?;
        if ask == Ask::Close {
            self.setup = None;
            if let Some(shown) = self.before_setup.take() {
                self.shown = shown;
            }
        }
        Some(super::state::Request::SetupAsk(ask))
    }

    /// A click on a row of the tree: the arrow opens or closes, the
    /// checkbox toggles, the name highlights; a double click on the name
    /// opens or closes, as Enter.
    pub(super) fn setup_click(&mut self, index: usize, part: u8) -> Option<super::state::Request> {
        let setup = self.setup.as_mut()?;
        if !matches!(setup.stage, Stage::Tree) {
            return None;
        }
        let now = std::time::Instant::now();
        let double = setup
            .last_click
            .is_some_and(|(at, row)| row == index && now.duration_since(at) < std::time::Duration::from_millis(400));
        setup.last_click = (!double).then_some((now, index));
        setup.notice = None;
        setup.filtering = false;
        setup.details_scroll = 0;
        if setup.in_services() {
            if setup.services_cursor != index {
                setup.manifest_open.clear();
                setup.manifest_focus = None;
            }
            setup.services_cursor = index;
            return (part == 0 || double)
                .then(|| setup.toggle_repository())
                .flatten()
                .map(super::state::Request::SetupAsk);
        }
        let row = setup.rows().get(index).cloned()?;
        setup.highlight(&row.id);
        match part {
            0 if row.branch => setup.set_open(&row.id, !row.expanded),
            1 => setup.toggle(&row.id, false),
            2 if double && row.branch => setup.set_open(&row.id, !row.expanded),
            _ => {}
        }
        None
    }

    /// What the worker of the setup box tells.
    pub fn setup_event(&mut self, event: super::setup_work::Event) {
        use super::setup_work::Event;
        let time = self.clock.clone();
        let Some(setup) = self.setup.as_mut() else {
            return;
        };
        match event {
            Event::Loading(message) => {
                if let Stage::Loading { message: shown } = &mut setup.stage {
                    *shown = message;
                }
            }
            Event::Ready {
                settings,
                target,
                catalog,
                state,
            } => {
                setup.ready(*catalog, state, *target, *settings);
                let line = format!("setup: {}", setup.target_description());
                self.log(&time, &line);
            }
            Event::Failed(message) => {
                setup.stage = Stage::Failed {
                    lines: message.lines().map(str::to_owned).collect(),
                };
                self.log(&time, &format!("setup: {message}"));
            }
            Event::State(state) => setup.replace_state(state),
            Event::Catalog(catalog, state) => setup.replace_catalog(*catalog, state),
            Event::Notice(text, error) => {
                setup.notify(&text, error);
                self.log(&time, &format!("setup: {text}"));
            }
            Event::Activity(identity, message) => setup.activity(identity.as_deref(), &message),
            Event::Progress(identity, state) => setup.progress(&identity, &state),
            Event::Finished(result, state) => {
                let ok = result["ok"] == true;
                setup.finished(result, state);
                let outcome = if ok { "finished" } else { "stopped" };
                self.log(&time, &format!("setup: {outcome}"));
            }
            Event::Services(repository, found) => {
                let failure = found
                    .as_ref()
                    .err()
                    .map(|error| format!("setup: services of {repository}: {error}"));
                setup.services_read(repository, found);
                if let Some(line) = failure {
                    self.log(&time, &line);
                }
            }
            // The loop lends the terminal itself.
            Event::Terminal(_) => {}
        }
    }
}
