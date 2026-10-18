//! `client.toml`: the desktop's part of the Python `config.toml`
//! (`docs/migration/api-mapping.md`, 8.1).
//!
//! ```toml
//! version = 1
//! [workspace_ssh]   # user, identity_files, identities_only, log_level, fido_devices
//! [repo]            # owner, checkout_root, sort
//! [display]         # storage_unit, storage_decimals
//! [server]          # urls (https only), token_file, ca_file
//! [sounds]          # optional, kept as written
//! [[setup.groups]]  # optional, kept as written; validated by the client
//! [[setup.items]]
//! ```
//!
//! The validation of `config.load_config` for these sections, and of
//! `[server]`, which only the Rust client (phase 13) has: where the
//! HomeStack server instances are and how the client authenticates to them.

use hs_model::config::fields::{self, toml_array, toml_string, type_error};
use hs_model::config::{ConfigError, FORMAT_VERSION};
use toml::{Table, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct ClientConfig {
    pub workspace_ssh: WorkspaceSsh,
    pub repo: Repo,
    pub display: Display,
    /// `[server]`; never in a Python `config.toml`.
    pub server: Option<Server>,
    /// `[sounds]`, validated, as written.
    pub sounds: Option<Table>,
    /// `[setup]` (groups and items), as written.
    pub setup: Option<Table>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSsh {
    pub user: String,
    /// Explicitly selected workspace keys; empty until keys are selected.
    pub identity_files: Vec<String>,
    pub identities_only: bool,
    /// Upper case, one of `LOG_LEVELS`.
    pub log_level: String,
    /// Lower case, without duplicates, in the order given.
    pub fido_devices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub owner: Option<String>,
    /// `~/...`, normalized.
    pub checkout_root: String,
    pub sort: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    pub storage_unit: String,
    pub storage_decimals: u8,
}

/// `[server]`: the instances, the client token's file and an extra CA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    /// `https://` only (decision 8: the token goes only over TLS).
    pub urls: Vec<String>,
    /// `~/...` allowed.
    pub token_file: String,
    pub ca_file: Option<String>,
}

/// The client token's file when `[server] token_file` is not set.
pub const DEFAULT_TOKEN_FILE: &str = "~/.config/homestack/api-token";

const LOG_LEVELS: [&str; 9] = [
    "DEBUG", "DEBUG1", "DEBUG2", "DEBUG3", "ERROR", "FATAL", "INFO", "QUIET", "VERBOSE",
];
const STORAGE_UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
const REPO_SORTS: [&str; 3] = ["recent", "created", "name"];
/// Names accepted in `fido_devices` besides USB IDs (Python `fido.NAMES`).
const FIDO_NAMES: [&str; 2] = ["yubikey", "trezor"];
const SOUNDS_KEYS: [&str; 6] = ["enabled", "player", "wait", "ok", "fail", "repeat_seconds"];

const SCHEMA: &[(&str, &[&str])] = &[
    (
        "",
        &[
            "version",
            "workspace_ssh",
            "repo",
            "display",
            "server",
            "sounds",
            "setup",
        ],
    ),
    (
        "workspace_ssh",
        &["user", "identity_files", "identities_only", "log_level", "fido_devices"],
    ),
    ("repo", &["owner", "checkout_root", "sort"]),
    ("display", &["storage_unit", "storage_decimals"]),
    ("server", &["urls", "token_file", "ca_file"]),
];

impl ClientConfig {
    /// Parse and validate `client.toml`.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let doc = fields::parse_document(text)?;
        for (section, keys) in SCHEMA {
            if section.is_empty() {
                fields::reject_unknown(&doc, "", keys)?;
            } else if let Some(table) = fields::section(&doc, section)? {
                fields::reject_unknown(table, section, keys)?;
            }
        }
        fields::version(&doc)?;
        let mut config = Self::from_sections(&doc)?;
        config.server = server(&doc)?;
        Ok(config)
    }

    /// Read the client sections of a document that may hold others, in the
    /// order of `config.load_config`.
    pub fn from_sections(doc: &Table) -> Result<Self, ConfigError> {
        let display = display(doc)?;
        let repo = repo(doc)?;
        let setup = match doc.get("setup") {
            None => None,
            Some(Value::Table(setup)) => Some(setup.clone()),
            Some(_) => return Err(ConfigError::new("setup", "[setup] must be a TOML table")),
        };
        let sounds = sounds(doc)?;
        let workspace_ssh = workspace_ssh(doc)?;
        Ok(Self {
            workspace_ssh,
            repo,
            display,
            server: None,
            sounds,
            setup,
        })
    }

    /// `client.toml` text: normalized values, `[sounds]` and `[setup]` as
    /// they were written.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        let ssh = &self.workspace_ssh;
        let mut lines = vec![
            format!("version = {FORMAT_VERSION}"),
            String::new(),
            "[workspace_ssh]".to_owned(),
            format!("user = {}", toml_string(&ssh.user)),
            format!("identity_files = {}", toml_array(&ssh.identity_files)),
            format!("identities_only = {}", ssh.identities_only),
            format!("log_level = {}", toml_string(&ssh.log_level)),
        ];
        if !ssh.fido_devices.is_empty() {
            lines.push(format!("fido_devices = {}", toml_array(&ssh.fido_devices)));
        }
        lines.extend([String::new(), "[repo]".to_owned()]);
        if let Some(owner) = &self.repo.owner {
            lines.push(format!("owner = {}", toml_string(owner)));
        }
        lines.extend([
            format!("checkout_root = {}", toml_string(&self.repo.checkout_root)),
            format!("sort = {}", toml_string(&self.repo.sort)),
            String::new(),
            "[display]".to_owned(),
            format!("storage_unit = {}", toml_string(&self.display.storage_unit)),
            format!("storage_decimals = {}", self.display.storage_decimals),
            String::new(),
        ]);
        if let Some(server) = &self.server {
            lines.extend([
                "[server]".to_owned(),
                format!("urls = {}", toml_array(&server.urls)),
                format!("token_file = {}", toml_string(&server.token_file)),
            ]);
            if let Some(ca_file) = &server.ca_file {
                lines.push(format!("ca_file = {}", toml_string(ca_file)));
            }
            lines.push(String::new());
        }
        let mut text = lines.join("\n");
        let mut rest = Table::new();
        if let Some(sounds) = &self.sounds {
            rest.insert("sounds".to_owned(), Value::Table(sounds.clone()));
        }
        if let Some(setup) = &self.setup {
            rest.insert("setup".to_owned(), Value::Table(setup.clone()));
        }
        if !rest.is_empty() {
            text.push('\n');
            text.push_str(&toml::to_string(&rest)?);
        }
        Ok(text)
    }
}

fn display(doc: &Table) -> Result<Display, ConfigError> {
    let section = fields::section(doc, "display")?;
    let get = |key: &str| section.and_then(|table| table.get(key));
    let storage_unit = match get("storage_unit") {
        None => "GiB".to_owned(),
        Some(Value::String(unit)) if STORAGE_UNITS.contains(&unit.as_str()) => unit.clone(),
        Some(_) => {
            return Err(ConfigError::new(
                "display.storage_unit",
                "[display] storage_unit must be one of KiB, MiB, GiB, TiB, PiB",
            ));
        }
    };
    let storage_decimals = match get("storage_decimals") {
        None => 0,
        Some(Value::Integer(decimals)) => u8::try_from(*decimals)
            .ok()
            .filter(|decimals| *decimals <= 4)
            .ok_or_else(|| {
                ConfigError::new(
                    "display.storage_decimals",
                    "[display] storage_decimals must be between 0 and 4",
                )
            })?,
        Some(_) => return Err(type_error("display", "storage_decimals", "an integer")),
    };
    Ok(Display {
        storage_unit,
        storage_decimals,
    })
}

fn repo(doc: &Table) -> Result<Repo, ConfigError> {
    let section = fields::section(doc, "repo")?;
    let get = |key: &str| section.and_then(|table| table.get(key));
    let owner = match get("owner") {
        None => None,
        Some(Value::String(owner)) if owner.trim().is_empty() => None,
        Some(Value::String(owner)) => Some(fields::repo_owner(owner)?),
        Some(_) => return Err(type_error("repo", "owner", "a string")),
    };
    let checkout_root = match get("checkout_root") {
        None => "~/DEV".to_owned(),
        Some(Value::String(root)) => checkout_root(root)?,
        Some(_) => return Err(type_error("repo", "checkout_root", "a string")),
    };
    let sort = match get("sort") {
        None => "recent".to_owned(),
        Some(Value::String(sort)) if REPO_SORTS.contains(&sort.as_str()) => sort.clone(),
        Some(_) => {
            return Err(ConfigError::new(
                "repo.sort",
                "[repo] sort must be recent, created or name",
            ));
        }
    };
    Ok(Repo {
        owner,
        checkout_root,
        sort,
    })
}

/// `[server]`, optional here; the Rust client requires it.
fn server(doc: &Table) -> Result<Option<Server>, ConfigError> {
    let Some(table) = fields::section(doc, "server")? else {
        return Ok(None);
    };
    let urls_error = |message: &str| ConfigError::new("server.urls", message);
    let raw = match table.get("urls") {
        Some(Value::Array(items)) if !items.is_empty() => items,
        _ => return Err(urls_error("[server] urls must be a non-empty array")),
    };
    let mut urls: Vec<String> = Vec::with_capacity(raw.len());
    for item in raw {
        let url = item
            .as_str()
            .ok_or_else(|| urls_error("[server] urls entries must be strings"))?
            .trim();
        let host = url.strip_prefix("https://").ok_or_else(|| {
            urls_error("[server] urls must start with https://; the client token is sent only over TLS")
        })?;
        if host.is_empty() || host.starts_with('/') || url.chars().any(char::is_whitespace) {
            return Err(urls_error("[server] urls entries must be https://HOST[:PORT] URLs"));
        }
        if urls.iter().any(|known| known == url) {
            return Err(urls_error("[server] urls contains duplicate entries"));
        }
        urls.push(url.to_owned());
    }
    let path = |key: &str| -> Result<Option<String>, ConfigError> {
        match table.get(key) {
            None => Ok(None),
            Some(Value::String(path)) if !path.trim().is_empty() && !path.contains('\0') => {
                Ok(Some(path.trim().to_owned()))
            }
            Some(_) => Err(ConfigError::new(
                format!("server.{key}"),
                format!("[server] {key} must be a non-empty path"),
            )),
        }
    };
    Ok(Some(Server {
        urls,
        token_file: path("token_file")?.unwrap_or_else(|| DEFAULT_TOKEN_FILE.to_owned()),
        ca_file: path("ca_file")?,
    }))
}

/// Python's `validate_repo_checkout_root`.
fn checkout_root(value: &str) -> Result<String, ConfigError> {
    let error = |message: &str| ConfigError::new("repo.checkout_root", message);
    let text = value.trim();
    let relative = text
        .strip_prefix("~/")
        .ok_or_else(|| error("[repo] checkout_root must start with '~/'"))?
        .trim_end_matches('/');
    if relative.is_empty() {
        return Err(error("[repo] checkout_root must be below the workspace home directory"));
    }
    if relative.split('/').any(|part| matches!(part, "" | "." | "..")) {
        return Err(error("[repo] checkout_root must be a normalized path below '~/'"));
    }
    Ok(format!("~/{relative}"))
}

/// Python's `_parse_sounds`.
fn sounds(doc: &Table) -> Result<Option<Table>, ConfigError> {
    let Some(table) = fields::section(doc, "sounds")? else {
        return Ok(None);
    };
    if table.keys().any(|key| !SOUNDS_KEYS.contains(&key.as_str())) {
        return Err(ConfigError::new("sounds", "[sounds] has unknown keys"));
    }
    if table.get("enabled").is_some_and(|enabled| !enabled.is_bool()) {
        return Err(ConfigError::new(
            "sounds.enabled",
            "[sounds] enabled must be true or false",
        ));
    }
    let usable = |item: &Value| {
        item.as_str()
            .is_some_and(|text| !text.trim().is_empty() && !text.contains('\0'))
    };
    let player_ok = match table.get("player") {
        None | Some(Value::String(_)) => true,
        Some(Value::Array(items)) => items.iter().all(usable),
        Some(_) => false,
    };
    if !player_ok
        || table
            .get("player")
            .is_some_and(|player| player.is_str() && !usable(player))
    {
        return Err(ConfigError::new(
            "sounds.player",
            "[sounds] player must be a command string or an array of strings",
        ));
    }
    for event in ["wait", "ok", "fail"] {
        if table
            .get(event)
            .is_some_and(|value| value.as_str().is_none_or(|text| text.contains('\0')))
        {
            return Err(ConfigError::new(
                format!("sounds.{event}"),
                format!("[sounds] {event} must be a sound file name or path"),
            ));
        }
    }
    let repeat = match table.get("repeat_seconds") {
        None => None,
        Some(Value::Integer(seconds)) => Some(*seconds as f64),
        Some(Value::Float(seconds)) => Some(*seconds),
        Some(_) => Some(f64::NAN),
    };
    if repeat.is_some_and(|seconds| !(1.0..=30.0).contains(&seconds)) {
        return Err(ConfigError::new(
            "sounds.repeat_seconds",
            "[sounds] repeat_seconds must be a number between 1 and 30",
        ));
    }
    Ok(Some(table.clone()))
}

fn workspace_ssh(doc: &Table) -> Result<WorkspaceSsh, ConfigError> {
    let table = match doc.get("workspace_ssh") {
        Some(Value::Table(table)) => table,
        _ => {
            return Err(ConfigError::new(
                "workspace_ssh",
                "Missing or invalid [workspace_ssh] configuration table",
            ));
        }
    };
    let user = table
        .get("user")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|user| !user.is_empty())
        .ok_or_else(|| ConfigError::new("workspace_ssh.user", "[workspace_ssh] user must be a non-empty string"))?
        .to_owned();

    let identity_error = |message: &str| ConfigError::new("workspace_ssh.identity_files", message);
    let raw_files: &[Value] = match table.get("identity_files") {
        None => &[],
        Some(Value::Array(items)) => items,
        _ => {
            return Err(identity_error("[workspace_ssh] identity_files must be an array"));
        }
    };
    let mut identity_files: Vec<String> = Vec::with_capacity(raw_files.len());
    for item in raw_files {
        let file = item
            .as_str()
            .ok_or_else(|| identity_error("[workspace_ssh] identity_files entries must be strings"))?
            .trim();
        if file.is_empty() || file.chars().any(char::is_whitespace) || file.contains('\0') {
            return Err(identity_error(
                "[workspace_ssh] identity_files entries must be non-empty paths without whitespace or control characters",
            ));
        }
        if identity_files.iter().any(|known| known == file) {
            return Err(identity_error(
                "[workspace_ssh] identity_files contains duplicate entries",
            ));
        }
        identity_files.push(file.to_owned());
    }

    let identities_only = table.get("identities_only").and_then(Value::as_bool).ok_or_else(|| {
        ConfigError::new(
            "workspace_ssh.identities_only",
            "[workspace_ssh] identities_only must be true or false",
        )
    })?;

    let log_level = table
        .get("log_level")
        .and_then(Value::as_str)
        .map(|level| level.trim().to_ascii_uppercase())
        .filter(|level| LOG_LEVELS.contains(&level.as_str()))
        .ok_or_else(|| {
            ConfigError::new(
                "workspace_ssh.log_level",
                format!("[workspace_ssh] log_level must be one of {}", LOG_LEVELS.join(", ")),
            )
        })?;

    let fido_error = |message: &str| ConfigError::new("workspace_ssh.fido_devices", message);
    let raw_devices = match table.get("fido_devices") {
        None => &Vec::new(),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(fido_error("[workspace_ssh] fido_devices must be an array of strings")),
    };
    let mut fido_devices: Vec<String> = Vec::new();
    for item in raw_devices {
        let entry = item
            .as_str()
            .ok_or_else(|| fido_error("[workspace_ssh] fido_devices entries must be strings"))?
            .trim()
            .to_lowercase();
        if !(FIDO_NAMES.contains(&entry.as_str()) || is_usb_id(&entry)) {
            return Err(fido_error(
                "[workspace_ssh] fido_devices entries must be yubikey, trezor or a USB id such as 1050 or 1050:0407",
            ));
        }
        if !fido_devices.contains(&entry) {
            fido_devices.push(entry);
        }
    }

    Ok(WorkspaceSsh {
        user,
        identity_files,
        identities_only,
        log_level,
        fido_devices,
    })
}

/// `vvvv` or `vvvv:pppp`, lower-case hex (Python `fido._ID_RE`).
fn is_usb_id(entry: &str) -> bool {
    let hex4 = |part: &str| part.len() == 4 && part.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    match entry.split_once(':') {
        None => hex4(entry),
        Some((vendor, product)) => hex4(vendor) && hex4(product),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_roots_are_normalized_like_python() {
        assert_eq!(checkout_root(" ~/DEV/ ").ok().as_deref(), Some("~/DEV"));
        assert_eq!(checkout_root("~/a/b//").ok().as_deref(), Some("~/a/b"));
        for root in ["~/", "~", "/tmp/x", "~/a//b", "~/./a", "~/a/.."] {
            assert!(checkout_root(root).is_err(), "{root}");
        }
    }

    #[test]
    fn usb_ids_are_four_hex_digits() {
        for id in ["1050", "1050:0407", "abcd:ef01"] {
            assert!(is_usb_id(id), "{id}");
        }
        for id in ["105", "10500", "1050:", "1050:04070", "ghij"] {
            assert!(!is_usb_id(id), "{id}");
        }
    }
}
