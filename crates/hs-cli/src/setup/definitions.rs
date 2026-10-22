//! The setup catalog's definitions (Python `setup_config.py`): groups,
//! items and their defaults, `[setup]` of `client.toml` merged over them,
//! and the generated entries of an application (its managed values and its
//! file copies).

use toml::{Table, Value};

use super::text::{home_path, repository, setup_id};
use crate::AppError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub label: String,
    pub description: String,
}

/// A command that validates an application after setup changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validation {
    pub command: String,
    /// `exit-code` or `json-path`.
    pub kind: String,
    pub path: Vec<String>,
    pub accepted: Vec<String>,
}

/// One structured configuration document of an application.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigFile {
    pub id: String,
    pub path: String,
    /// `toml`, `json` or `yaml`.
    pub format: String,
    pub values: Table,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Application {
    pub command: String,
    pub interpreter: String,
    pub interaction: String,
    pub prerequisites: Vec<String>,
    pub check: String,
    pub prerequisite_checks: Vec<String>,
    pub non_interactive: String,
    pub bin_dirs: Vec<String>,
    pub requires_absent: Vec<String>,
    pub backup_paths: Vec<String>,
    pub files: Vec<String>,
    pub config_files: Vec<ConfigFile>,
    pub validation: Option<Validation>,
}

impl Application {
    /// An application with Python's defaults and `command`.
    #[must_use]
    pub fn new(command: &str) -> Self {
        Self {
            command: command.to_owned(),
            interpreter: "bash".to_owned(),
            interaction: "interactive".to_owned(),
            prerequisites: Vec::new(),
            check: String::new(),
            prerequisite_checks: Vec::new(),
            non_interactive: String::new(),
            bin_dirs: vec!["~/.local/bin".to_owned()],
            requires_absent: Vec::new(),
            backup_paths: Vec::new(),
            files: Vec::new(),
            config_files: Vec::new(),
            validation: None,
        }
    }

    fn non_interactive(mut self, prerequisites: &[&str], check: &str) -> Self {
        self.interaction = "non-interactive".to_owned();
        self.prerequisites = strings(prerequisites);
        self.check = check.to_owned();
        self
    }

    fn bins(mut self, bins: &[&str]) -> Self {
        self.bin_dirs = strings(bins);
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Params {
    /// A desktop file or directory copied into the home; `application` is
    /// set for a file an application declares (`AppFileParams`).
    File {
        path: String,
        application: Option<String>,
    },
    Environment {
        profile: String,
    },
    Application(Box<Application>),
    Repository {
        repository: String,
    },
    Backup,
    Herdr,
    /// One managed leaf of an application's configuration document.
    Structured {
        application: String,
        config: Box<ConfigFile>,
        key: Vec<String>,
        value: Value,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: String,
    pub group: String,
    pub handler: String,
    pub label: String,
    pub description: String,
    pub params: Params,
    pub depends_on: Vec<String>,
}

impl Entry {
    fn new(id: &str, group: &str, handler: &str, label: &str, description: &str, params: Params) -> Self {
        Self {
            id: id.to_owned(),
            group: group.to_owned(),
            handler: handler.to_owned(),
            label: label.to_owned(),
            description: description.to_owned(),
            params,
            depends_on: Vec::new(),
        }
    }

    /// The application's parameters, for an application entry.
    #[must_use]
    pub fn application(&self) -> Option<&Application> {
        match &self.params {
            Params::Application(application) => Some(application),
            _ => None,
        }
    }

    /// The flat definition (Python `Entry.definition`): the common fields,
    /// then the parameters; no validator is `false`, as TOML has no null.
    #[must_use]
    pub fn definition(&self) -> Table {
        let mut data = Table::new();
        data.insert("id".into(), Value::String(self.id.clone()));
        data.insert("group".into(), Value::String(self.group.clone()));
        data.insert("handler".into(), Value::String(self.handler.clone()));
        data.insert("label".into(), Value::String(self.label.clone()));
        data.insert("description".into(), Value::String(self.description.clone()));
        data.insert("depends_on".into(), string_array(&self.depends_on));
        match &self.params {
            Params::File { path, application } => {
                data.insert("path".into(), Value::String(path.clone()));
                if let Some(application) = application {
                    data.insert("application".into(), Value::String(application.clone()));
                }
            }
            Params::Environment { profile } => {
                data.insert("profile".into(), Value::String(profile.clone()));
            }
            Params::Application(p) => {
                let text = |value: &str| Value::String(value.to_owned());
                data.insert("command".into(), text(&p.command));
                data.insert("interpreter".into(), text(&p.interpreter));
                data.insert("interaction".into(), text(&p.interaction));
                data.insert("prerequisites".into(), string_array(&p.prerequisites));
                data.insert("check".into(), text(&p.check));
                data.insert("prerequisite_checks".into(), string_array(&p.prerequisite_checks));
                data.insert("non_interactive".into(), text(&p.non_interactive));
                data.insert("bin_dirs".into(), string_array(&p.bin_dirs));
                data.insert("requires_absent".into(), string_array(&p.requires_absent));
                data.insert("backup_paths".into(), string_array(&p.backup_paths));
                data.insert("files".into(), string_array(&p.files));
                data.insert(
                    "config_files".into(),
                    Value::Array(p.config_files.iter().map(|c| Value::Table(config_table(c))).collect()),
                );
                data.insert(
                    "validation".into(),
                    p.validation.as_ref().map_or(Value::Boolean(false), |v| {
                        let mut table = Table::new();
                        table.insert("command".into(), text(&v.command));
                        table.insert("type".into(), text(&v.kind));
                        table.insert("path".into(), string_array(&v.path));
                        table.insert("accepted".into(), string_array(&v.accepted));
                        Value::Table(table)
                    }),
                );
            }
            Params::Repository { repository } => {
                data.insert("repository".into(), Value::String(repository.clone()));
            }
            Params::Backup | Params::Herdr => {}
            Params::Structured {
                application,
                config,
                key,
                value,
            } => {
                data.insert("application".into(), Value::String(application.clone()));
                data.insert("config".into(), Value::Table(config_table(config)));
                data.insert("key".into(), string_array(key));
                data.insert("value".into(), value.clone());
            }
        }
        data
    }
}

fn config_table(config: &ConfigFile) -> Table {
    let mut table = Table::new();
    table.insert("id".into(), Value::String(config.id.clone()));
    table.insert("path".into(), Value::String(config.path.clone()));
    table.insert("format".into(), Value::String(config.format.clone()));
    table.insert("values".into(), Value::Table(config.values.clone()));
    table
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn string_array(values: &[String]) -> Value {
    Value::Array(values.iter().cloned().map(Value::String).collect())
}

pub const CODEX_RECIPE: &str = "curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_NON_INTERACTIVE=1 sh";
pub const CLAUDE_RECIPE: &str = "curl -fsSL https://claude.ai/install.sh | bash";
pub const RUST_RECIPE: &str = "installer=$(mktemp)
trap 'rm -f -- \"$installer\"' EXIT
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o \"$installer\"
sh \"$installer\" -y --profile minimal
. \"$HOME/.cargo/env\"";
/// Downloaded separately so the installer's stdin stays the real terminal
/// when it needs one.
pub const HERMES_RECIPE: &str = "installer=$(mktemp)
trap 'rm -f -- \"$installer\"' EXIT
curl -fsSL https://hermes-agent.nousresearch.com/install.sh -o \"$installer\"
for stage in repository venv python-deps node-deps path config complete; do
    bash \"$installer\" --stage \"$stage\" --non-interactive --skip-browser --skip-computer-use
done";
const HERMES_NODE_CHECK: &str = "node -e \"const v=process.versions.node; const [a,b]=v.split('.').map(Number); process.exit(!v.includes('-') && ((a===22 && b>=22)||(a===24 && b>=11)||a>=26) ? 0 : 1)\"";
const HERMES_NPM_CHECK: &str = "npm --version | node -e \"let v='';process.stdin.on('data',x=>v+=x).on('end',()=>{const [a,b]=v.trim().split('.').map(Number);process.exit(a===11 && b>=10 && b<=16 ? 1 : 0)})\"";

/// The default groups, in their order.
#[must_use]
pub fn default_groups() -> Vec<Group> {
    [
        (
            "app",
            "Applications",
            "Explicit user-space installers; onboarding is separate.",
        ),
        (
            "env",
            "Environment",
            "User shell profiles; login shell stays unchanged.",
        ),
        (
            "repo",
            "Repositories",
            "GitHub checkouts with workspace-local deploy keys.",
        ),
        (
            "cred",
            "Credentials",
            "Authentication files and keys copied from the trusted desktop.",
        ),
    ]
    .into_iter()
    .map(|(id, label, description)| Group {
        id: id.to_owned(),
        label: label.to_owned(),
        description: description.to_owned(),
    })
    .collect()
}

/// The built-in items (Python `defaults()`).
#[must_use]
pub fn defaults() -> Vec<Entry> {
    let mut items: Vec<Entry> = [("bash", "Bash"), ("zsh", "Zsh"), ("fish", "Fish"), ("nu", "Nushell")]
        .into_iter()
        .map(|(id, label)| {
            Entry::new(
                id,
                "env",
                "environment",
                label,
                &format!("Prepare {label} startup files and user executable paths."),
                Params::Environment { profile: id.to_owned() },
            )
        })
        .collect();
    let app = |application: Application| Params::Application(Box::new(application));
    let mut hermes = Application::new(HERMES_RECIPE)
        .non_interactive(
            &["git", "curl", "tar", "xz", "node", "npm", "g++", "make"],
            "hermes --version",
        )
        .bins(&["~/.local/bin", "~/.hermes/bin", "~/.hermes/node/bin"]);
    hermes.prerequisite_checks = strings(&[HERMES_NODE_CHECK, HERMES_NPM_CHECK]);
    hermes.requires_absent = strings(&["~/.hermes/hermes-agent"]);
    // Nothing to install: the "installer" only confirms the system's Git.
    let mut git = Application::new("git --version").non_interactive(&["git"], "git --version");
    git.files = strings(&["~/.gitconfig"]);
    items.extend([
        Entry::new(
            "codex",
            "app",
            "application",
            "Codex",
            "Install Codex CLI. Sign-in remains separate.",
            app(Application::new(CODEX_RECIPE).non_interactive(&["curl", "tar"], "codex --version")),
        ),
        Entry::new(
            "claude",
            "app",
            "application",
            "Claude Code",
            "Install Claude Code CLI. Sign-in remains separate.",
            app(Application::new(CLAUDE_RECIPE).non_interactive(&["curl"], "claude --version")),
        ),
        Entry::new(
            "herdr",
            "app",
            "herdr",
            "Herdr",
            "Install Herdr, enable its user service with updates before start, and connect the desktop using the VM name. Existing servers are left running. Desktop SSH may require another security-key touch.",
            Params::Herdr,
        ),
        Entry::new(
            "opencode",
            "app",
            "application",
            "OpenCode",
            "Install OpenCode. Provider configuration remains separate.",
            app(Application::new("curl -fsSL https://opencode.ai/install | bash")
                .non_interactive(&["curl", "tar"], "opencode --version")
                .bins(&["~/.opencode/bin", "~/.local/bin"])),
        ),
        Entry::new(
            "opencode-v2",
            "app",
            "application",
            "OpenCode v2",
            "",
            app(Application::new("curl -fsSL https://opencode.ai/v2/install | bash")
                .non_interactive(&["curl", "tar"], "opencode --version")
                .bins(&["~/.opencode/bin", "~/.local/bin"])),
        ),
        Entry::new(
            "pi",
            "app",
            "application",
            "Pi",
            "",
            app(Application::new("curl -fsSL https://pi.dev/install.sh | sh").non_interactive(&["curl", "tar"], "pi --version")),
        ),
        Entry::new(
            "hermes",
            "app",
            "application",
            "Hermes Agent",
            "Install the Hermes CLI through user-space bootstrap stages. No browser, desktop, gateway or provider setup. Run hermes setup separately.",
            app(hermes),
        ),
        Entry::new(
            "rust",
            "app",
            "application",
            "Rust (rustup)",
            "Install the Rust toolchain with the minimal rustup profile, including rustc and Cargo.",
            app(Application::new(RUST_RECIPE)
                .non_interactive(&["curl", "sh"], "rustup --version && rustc --version && cargo --version")
                .bins(&["~/.cargo/bin"])),
        ),
        Entry::new(
            "git",
            "app",
            "application",
            "Git",
            "Git itself is installed system-wide by root; this checks it and copies the desktop Git configuration into persistent home.",
            app(git),
        ),
        Entry::new(
            "bk",
            "app",
            "backup",
            "BK",
            "Install the persistent-home BK backup tool and nightly user timer.",
            Params::Backup,
        ),
    ]);
    items
}

/// The groups and items of the setup.
#[derive(Debug, Clone, PartialEq)]
pub struct SetupConfig {
    pub groups: Vec<Group>,
    pub items: Vec<Entry>,
}

impl Default for SetupConfig {
    fn default() -> Self {
        Self {
            groups: default_groups(),
            items: defaults(),
        }
    }
}

impl SetupConfig {
    #[must_use]
    pub fn item(&self, id: &str) -> Option<&Entry> {
        self.items.iter().find(|entry| entry.id == id)
    }
}

/// A JSON-pointer segment (RFC 6901).
fn pointer_escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn pointer_unescape(value: &str) -> String {
    // In this order, so a literal `~01` stays `~1`.
    value.replace("~1", "/").replace("~0", "~")
}

/// `urllib.parse.quote(value, safe="~./-_")`.
fn selector_escape(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"~./-_".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `urllib.parse.unquote`: `%XX` decoded, invalid UTF-8 replaced.
fn selector_unescape(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = value.get(index + 1..index + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The stable ID of a generated structured leaf entry.
#[must_use]
pub fn config_entry_id(application: &str, config: &str, key: &[String]) -> String {
    let pointer: Vec<String> = key
        .iter()
        .map(|segment| selector_escape(&pointer_escape(segment)))
        .collect();
    format!("{application}:{config}:/{}", pointer.join("/"))
}

/// Decode a generated structured leaf ID.
pub fn config_entry_key(identifier: &str) -> Result<(String, String, Vec<String>), AppError> {
    let invalid = || AppError::new("Invalid structured configuration selector");
    let parts: Vec<&str> = identifier.splitn(3, ':').collect();
    let [application, config, pointer] = parts.as_slice() else {
        return Err(invalid());
    };
    if !setup_id(application) || !setup_id(config) || !pointer.starts_with('/') {
        return Err(invalid());
    }
    let key = pointer[1..]
        .split('/')
        .map(|segment| pointer_unescape(&selector_unescape(segment)))
        .collect();
    Ok(((*application).to_owned(), (*config).to_owned(), key))
}

/// The stable ID of a generated application file copy.
#[must_use]
pub fn app_file_entry_id(application: &str, path: &str) -> String {
    format!("{application}:file:{}", selector_escape(path))
}

/// One file-copy entry per file an application declares.
#[must_use]
pub fn app_file_entries(entry: &Entry) -> Vec<Entry> {
    let Some(application) = entry.application() else {
        return Vec::new();
    };
    application
        .files
        .iter()
        .map(|path| {
            Entry::new(
                &app_file_entry_id(&entry.id, path),
                &entry.group,
                "file",
                path,
                &format!("Copy {path} from the trusted desktop."),
                Params::File {
                    path: path.clone(),
                    application: Some(entry.id.clone()),
                },
            )
        })
        .collect()
}

fn leaf_paths(values: &Table, prefix: &[String], out: &mut Vec<(Vec<String>, Value)>) {
    for (key, value) in values {
        let mut child = prefix.to_vec();
        child.push(key.clone());
        match value {
            Value::Table(table) => leaf_paths(table, &child, out),
            other => out.push((child, other.clone())),
        }
    }
}

/// One entry per managed leaf of an application's configuration files.
#[must_use]
pub fn config_entries(entry: &Entry) -> Vec<Entry> {
    let Some(application) = entry.application() else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for config in &application.config_files {
        let mut leaves = Vec::new();
        leaf_paths(&config.values, &[], &mut leaves);
        for (key, value) in leaves {
            let label = key.last().cloned().unwrap_or_default();
            result.push(Entry::new(
                &config_entry_id(&entry.id, &config.id, &key),
                &entry.group,
                "structured",
                &label,
                &format!("Set {} {}", config.path, key.join(".")),
                Params::Structured {
                    application: entry.id.clone(),
                    config: Box::new(config.clone()),
                    key,
                    value,
                },
            ));
        }
    }
    result
}

/// Every generated child of an application: managed values, then files.
#[must_use]
pub fn application_children(entry: &Entry) -> Vec<Entry> {
    let mut children = config_entries(entry);
    children.extend(app_file_entries(entry));
    children
}

/// `_strings`: an array of non-empty strings without NUL.
fn string_list(value: &Value, key: &str) -> Result<Vec<String>, AppError> {
    let error = || AppError::new(format!("[setup] {key} must be an array of non-empty strings"));
    let Value::Array(items) = value else {
        return Err(error());
    };
    items
        .iter()
        .map(|item| match item {
            Value::String(text) if !text.trim().is_empty() && !text.contains('\0') => Ok(text.clone()),
            _ => Err(error()),
        })
        .collect()
}

/// `_config_values`: tables, arrays and finite scalars; no NUL.
fn config_values(value: &Value) -> Result<Table, AppError> {
    fn check(item: &Value) -> Result<(), AppError> {
        match item {
            Value::Table(table) => {
                for (key, child) in table {
                    if key.contains('\0') {
                        return Err(AppError::new("Structured configuration keys must be text"));
                    }
                    check(child)?;
                }
                Ok(())
            }
            Value::Array(items) => items.iter().try_for_each(check),
            Value::Boolean(_) | Value::Integer(_) => Ok(()),
            Value::Float(number) if number.is_finite() => Ok(()),
            Value::Float(_) => Err(AppError::new("Structured configuration numbers must be finite")),
            Value::String(text) if !text.contains('\0') => Ok(()),
            _ => Err(AppError::new(
                "Structured configuration values must use scalar, array or table values",
            )),
        }
    }
    let Value::Table(table) = value else {
        return Err(AppError::new("Structured configuration values must be a table"));
    };
    check(value)?;
    Ok(table.clone())
}

fn config_validation(value: Option<&Value>) -> Result<Option<Validation>, AppError> {
    let table = match value {
        None | Some(Value::Boolean(false)) => return Ok(None),
        Some(Value::Table(table))
            if table
                .keys()
                .all(|key| ["command", "type", "path", "accepted"].contains(&key.as_str())) =>
        {
            table
        }
        Some(_) => {
            return Err(AppError::new(
                "Application validation must be a table with command, type, path and accepted",
            ));
        }
    };
    let command = match table.get("command") {
        None => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(_) => return Err(AppError::new("Application validation command must be valid text")),
    };
    if command.trim().is_empty() || command.contains('\0') {
        return Err(AppError::new("Application validation command must be valid text"));
    }
    let kind = match table.get("type") {
        None => "exit-code".to_owned(),
        Some(Value::String(text)) if text == "exit-code" || text == "json-path" => text.clone(),
        Some(_) => {
            return Err(AppError::new(
                "Application validation type must be exit-code or json-path",
            ));
        }
    };
    let empty = Value::Array(Vec::new());
    let path = string_list(table.get("path").unwrap_or(&empty), "validation.path")?;
    let accepted = string_list(table.get("accepted").unwrap_or(&empty), "validation.accepted")?;
    if kind == "json-path" && (path.is_empty() || accepted.is_empty()) {
        return Err(AppError::new(
            "JSON-path application validation requires path and accepted",
        ));
    }
    if kind == "exit-code" && !path.is_empty() {
        return Err(AppError::new(
            "Exit-code application validation cannot specify a JSON path",
        ));
    }
    Ok(Some(Validation {
        command,
        kind,
        path,
        accepted,
    }))
}

fn config_files(value: &Value, application: &str, seen_paths: &mut Vec<String>) -> Result<Vec<ConfigFile>, AppError> {
    let Value::Array(items) = value else {
        return Err(AppError::new(format!(
            "Application {application} config_files must be an array of tables"
        )));
    };
    let mut parsed = Vec::new();
    let mut seen_ids: Vec<String> = Vec::new();
    for data in items {
        let table = match data {
            Value::Table(table)
                if table
                    .keys()
                    .all(|key| ["id", "path", "format", "values"].contains(&key.as_str())) =>
            {
                table
            }
            _ => {
                return Err(AppError::new(format!(
                    "Invalid structured configuration file for application {application}"
                )));
            }
        };
        let id = match table.get("id") {
            None => String::new(),
            Some(Value::String(id)) => id.clone(),
            Some(_) => "\0".to_owned(),
        };
        if !setup_id(&id) || seen_ids.contains(&id) {
            return Err(AppError::new(format!(
                "Invalid or duplicate structured configuration file ID for {application}"
            )));
        }
        seen_ids.push(id.clone());
        let path = match table.get("path") {
            Some(Value::String(path)) if !path.contains('\0') => path.clone(),
            _ => {
                return Err(AppError::new(format!(
                    "Structured configuration file {id} needs a valid path"
                )));
            }
        };
        let (relative, is_directory) = home_path(&path)
            .map_err(|_| AppError::new(format!("Structured configuration file {id} has an unsafe path")))?;
        if is_directory {
            return Err(AppError::new(format!(
                "Structured configuration file {id} must name a file"
            )));
        }
        if seen_paths.contains(&relative) {
            return Err(AppError::new(format!(
                "Duplicate structured configuration file path {path}"
            )));
        }
        seen_paths.push(relative);
        let format = match table.get("format") {
            Some(Value::String(format)) if ["toml", "json", "yaml"].contains(&format.to_lowercase().as_str()) => {
                format.to_lowercase()
            }
            _ => {
                return Err(AppError::new(format!(
                    "Structured configuration file {id} format must be TOML, JSON or YAML"
                )));
            }
        };
        let values = config_values(table.get("values").unwrap_or(&Value::Table(Table::new())))?;
        parsed.push(ConfigFile {
            id,
            path,
            format,
            values,
        });
    }
    Ok(parsed)
}

fn text_field<'a>(data: &'a Table, key: &str) -> Option<&'a str> {
    data.get(key).and_then(Value::as_str)
}

/// Parse `[setup]` (Python `parse_setup`): the defaults with the groups and
/// items of the file merged over them by ID.
pub fn parse_setup(raw: &Table) -> Result<SetupConfig, AppError> {
    if raw.keys().any(|key| key != "groups" && key != "items") {
        return Err(AppError::new("[setup] accepts only groups and items"));
    }
    let mut groups = default_groups();
    let default_items = defaults();
    let mut seen: Vec<String> = Vec::new();
    let empty = Value::Array(Vec::new());
    let Value::Array(raw_groups) = raw.get("groups").unwrap_or(&empty) else {
        return Err(AppError::new("[setup] groups must be an array of tables"));
    };
    for data in raw_groups {
        let table = match data {
            Value::Table(table)
                if table
                    .keys()
                    .all(|key| ["id", "label", "description"].contains(&key.as_str())) =>
            {
                table
            }
            _ => return Err(AppError::new("Invalid setup group definition")),
        };
        let id = text_field(table, "id").unwrap_or_default().to_owned();
        let reserved =
            ["root", "e", "a", "c", "r"].contains(&id.as_str()) || default_items.iter().any(|entry| entry.id == id);
        if !matches!(table.get("id"), None | Some(Value::String(_))) || !setup_id(&id) || seen.contains(&id) || reserved
        {
            return Err(AppError::new("Invalid or duplicate setup group ID"));
        }
        seen.push(id.clone());
        let existing = groups.iter().position(|group| group.id == id);
        let base = existing.map(|index| groups[index].clone());
        let label = match table.get("label") {
            Some(Value::String(label)) => Some(label.clone()),
            Some(_) => None,
            None => base.as_ref().map(|group| group.label.clone()),
        };
        let label = label
            .filter(|label| !label.trim().is_empty())
            .ok_or_else(|| AppError::new("Setup group requires a label"))?;
        let description = match table.get("description") {
            Some(Value::String(text)) => text.clone(),
            Some(_) => return Err(AppError::new("Setup group description must be text")),
            None => base.map(|group| group.description).unwrap_or_default(),
        };
        let group = Group { id, label, description };
        match existing {
            Some(index) => groups[index] = group,
            None => groups.push(group),
        }
    }
    let mut entries: Vec<(String, Table)> = default_items
        .iter()
        .map(|entry| (entry.id.clone(), entry.definition()))
        .collect();
    let mut config_paths: Vec<String> = Vec::new();
    seen.clear();
    let Value::Array(raw_items) = raw.get("items").unwrap_or(&empty) else {
        return Err(AppError::new("[setup] items must be an array of tables"));
    };
    for data in raw_items {
        let Value::Table(table) = data else {
            return Err(AppError::new("Setup item must be a table"));
        };
        let id = match table.get("id") {
            None => String::new(),
            Some(Value::String(id)) => id.clone(),
            Some(_) => "\0".to_owned(),
        };
        if !setup_id(&id)
            || seen.contains(&id)
            || groups.iter().any(|group| group.id == id)
            || id == "root"
            || id == "all"
        {
            return Err(AppError::new("Invalid or duplicate setup item ID"));
        }
        seen.push(id.clone());
        let position = entries.iter().position(|(known, _)| *known == id);
        let mut base = position.map(|index| entries[index].1.clone()).unwrap_or_default();
        if let Some(command) = table.get("command")
            && base.get("command") != Some(command)
        {
            // A customized payload cannot inherit a safety claim about another recipe.
            base.insert("interaction".into(), Value::String("interactive".into()));
            base.insert("non_interactive".into(), Value::String(String::new()));
            base.insert("check".into(), Value::String(String::new()));
            base.insert("requires_absent".into(), Value::Array(Vec::new()));
            base.insert("backup_paths".into(), Value::Array(Vec::new()));
            base.insert("validation".into(), Value::Boolean(false));
        }
        for (key, value) in table {
            base.insert(key.clone(), value.clone());
        }
        match position {
            Some(index) => entries[index].1 = base,
            None => entries.push((id, base)),
        }
    }
    let mut parsed = Vec::new();
    for (_, data) in entries {
        parsed.push(parse_item(&data, &groups, &mut config_paths)?);
    }
    Ok(SetupConfig { groups, items: parsed })
}

const COMMON: [&str; 6] = ["id", "group", "handler", "label", "description", "depends_on"];
const APPLICATION_FIELDS: [&str; 13] = [
    "command",
    "interpreter",
    "interaction",
    "prerequisites",
    "check",
    "prerequisite_checks",
    "non_interactive",
    "bin_dirs",
    "requires_absent",
    "backup_paths",
    "files",
    "config_files",
    "validation",
];

fn parse_item(data: &Table, groups: &[Group], config_paths: &mut Vec<String>) -> Result<Entry, AppError> {
    let id = text_field(data, "id").unwrap_or_default().to_owned();
    let handler = text_field(data, "handler");
    let group = text_field(data, "group");
    let fields: &[&str] = match handler {
        Some("file") => &["path"],
        Some("environment") => &["profile"],
        Some("application") => &APPLICATION_FIELDS,
        Some("repository") => &["repository"],
        Some("backup" | "herdr") => &[],
        _ => &["\0"],
    };
    let (Some(handler), Some(group)) = (handler, group) else {
        return Err(AppError::new("Setup item has an unknown handler or group"));
    };
    if fields == ["\0"] || !groups.iter().any(|known| known.id == group) {
        return Err(AppError::new("Setup item has an unknown handler or group"));
    }
    for key in ["label", "description"] {
        let valid = match data.get(key) {
            Some(Value::String(text)) => key != "label" || !text.trim().is_empty(),
            _ => false,
        };
        if !valid {
            return Err(AppError::new(format!("Setup item {id} needs {key}")));
        }
    }
    let params: Table = data
        .iter()
        .filter(|(key, _)| !COMMON.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if params.keys().any(|key| !fields.contains(&key.as_str())) {
        return Err(AppError::new(format!("Unknown parameters for setup item {id}")));
    }
    let invalid = || AppError::new(format!("Invalid parameters for setup item {id}"));
    let lists = [
        "prerequisites",
        "bin_dirs",
        "requires_absent",
        "prerequisite_checks",
        "backup_paths",
        "files",
    ];
    let mut list_values: Vec<(String, Vec<String>)> = Vec::new();
    for key in lists {
        if let Some(value) = params.get(key) {
            list_values.push((key.to_owned(), string_list(value, key)?));
        }
    }
    let params_value = match handler {
        "file" => {
            let path = params.get("path").ok_or_else(invalid)?;
            let Value::String(path) = path else {
                return Err(AppError::new("File path must be a string"));
            };
            home_path(path)?;
            Params::File {
                path: path.clone(),
                application: None,
            }
        }
        "environment" => {
            let profile = params.get("profile").ok_or_else(invalid)?;
            match profile {
                Value::String(profile) if ["bash", "zsh", "fish", "nu"].contains(&profile.as_str()) => {
                    Params::Environment {
                        profile: profile.clone(),
                    }
                }
                _ => return Err(AppError::new("Environment profile must be bash, zsh, fish or nu")),
            }
        }
        "repository" => {
            let value = params.get("repository").ok_or_else(invalid)?;
            let Value::String(text) = value else {
                return Err(AppError::new("Repository must be a string"));
            };
            repository(text)?;
            Params::Repository {
                repository: text.clone(),
            }
        }
        "backup" => Params::Backup,
        "herdr" => Params::Herdr,
        _ => {
            let empty = Value::Array(Vec::new());
            let config = config_files(params.get("config_files").unwrap_or(&empty), &id, config_paths)?;
            let validation = config_validation(params.get("validation"))?;
            if !params.contains_key("command") {
                return Err(invalid());
            }
            Params::Application(Box::new(parse_application(
                &id,
                &params,
                list_values,
                config,
                validation,
            )?))
        }
    };
    let empty = Value::Array(Vec::new());
    let depends_on = string_list(data.get("depends_on").unwrap_or(&empty), "depends_on")?;
    Ok(Entry {
        id: id.clone(),
        group: group.to_owned(),
        handler: handler.to_owned(),
        label: text_field(data, "label").unwrap_or_default().to_owned(),
        description: text_field(data, "description").unwrap_or_default().to_owned(),
        params: params_value,
        depends_on,
    })
}

fn parse_application(
    id: &str,
    params: &Table,
    lists: Vec<(String, Vec<String>)>,
    config_files: Vec<ConfigFile>,
    validation: Option<Validation>,
) -> Result<Application, AppError> {
    let mut p = Application::new("");
    for (key, values) in lists {
        match key.as_str() {
            "prerequisites" => p.prerequisites = values,
            "bin_dirs" => p.bin_dirs = values,
            "requires_absent" => p.requires_absent = values,
            "prerequisite_checks" => p.prerequisite_checks = values,
            "backup_paths" => p.backup_paths = values,
            _ => p.files = values,
        }
    }
    p.config_files = config_files;
    p.validation = validation;
    let text = |key: &str, default: &str| -> Result<String, ()> {
        match params.get(key) {
            None => Ok(default.to_owned()),
            Some(Value::String(text)) => Ok(text.clone()),
            Some(_) => Err(()),
        }
    };
    let interpreter = text("interpreter", "bash");
    if !matches!(interpreter.as_deref(), Ok("bash" | "zsh")) {
        return Err(AppError::new(
            "Application interpreter must be bash or zsh (pipeline failure handling required)",
        ));
    }
    let interaction = text("interaction", "interactive");
    if !matches!(interaction.as_deref(), Ok("interactive" | "non-interactive")) {
        return Err(AppError::new(
            "Application interaction must be interactive or non-interactive",
        ));
    }
    p.interpreter = interpreter.unwrap_or_default();
    p.interaction = interaction.unwrap_or_default();
    for key in ["command", "check", "non_interactive"] {
        let value = text(key, "")
            .ok()
            .filter(|value| !value.contains('\0') && (key != "command" || !value.trim().is_empty()))
            .ok_or_else(|| AppError::new(format!("Application {key} must be valid text")))?;
        match key {
            "command" => p.command = value,
            "check" => p.check = value,
            _ => p.non_interactive = value,
        }
    }
    if p.prerequisites
        .iter()
        .any(|tool| !tool.chars().all(|c| c.is_ascii_alphanumeric() || "_.+-".contains(c)))
    {
        return Err(AppError::new("Application prerequisites must be executable names"));
    }
    if p.bin_dirs.iter().any(|path| path.contains(':')) {
        return Err(AppError::new("Application bin_dirs must not contain PATH separators"));
    }
    for path in p
        .bin_dirs
        .iter()
        .chain(&p.requires_absent)
        .chain(&p.backup_paths)
        .chain(&p.files)
    {
        home_path(path)?;
    }
    let mut distinct: Vec<&str> = p.files.iter().map(|path| path.trim_end_matches('/')).collect();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() != p.files.len() {
        return Err(AppError::new(format!("Application {id} declares a file twice")));
    }
    Ok(p)
}
