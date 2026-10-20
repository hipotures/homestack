//! The server configuration, `server.toml`.
//!
//! It is the server's part of the Python `config.toml`
//! (`docs/migration/api-mapping.md`, 8.1), with the same section and key
//! names, validated by the rules of Python's `config.load_config`:
//!
//! ```toml
//! version = 1
//! [node]          # name, gold_vmid
//! [storage_layouts]
//! [root]          # disk
//! [home]          # disk, default_size
//! [network]       # prefix, cidr, gateway, dns
//! [cloud_init]    # snippet_storage, snippet_dir
//! [user]          # name, uid, gid
//! [repo]          # owner (optional)
//! ```
//!
//! Unlike Python, unknown keys are errors and values must have the TOML type
//! Python's own writer produces (no `int("100")`); see
//! `docs/migration/phase-1.md`. Error messages name the field and never
//! repeat the value.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use sha2::{Digest, Sha256};
use toml::{Table, Value};

use crate::NodeName;

/// Version of the configuration document format (`version = 1`).
pub const FORMAT_VERSION: i64 = 1;

/// Lowest and highest Proxmox VMID.
const VMID_RANGE: std::ops::RangeInclusive<i64> = 100..=999_999_999;

/// The server configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub node: NodeName,
    /// The base VM may be configured after the server is installed.
    pub gold_vmid: Option<u32>,
    /// `homestack-storage-<N>` to storage IDs; node `...N` uses layout N.
    pub storage_layouts: BTreeMap<String, Vec<String>>,
    pub root_disk: String,
    pub home_disk: String,
    pub default_home_size: String,
    pub network: Network,
    pub cloud_init: CloudInit,
    pub user: User,
    pub repo_owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub prefix: String,
    pub cidr: u8,
    pub gateway: String,
    pub dns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudInit {
    pub snippet_storage: String,
    pub snippet_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
}

/// Why a configuration was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ConfigError {
    /// What the error is about: `section.key`, a section, `version` or
    /// `toml`. The parity corpus compares this with Python.
    pub field: String,
    /// Human-readable reason. Never contains a value from the document.
    pub message: String,
}

impl ConfigError {
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// SHA-256 of the configuration text, as lowercase hex.
#[must_use]
pub fn config_version(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest.iter() {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Keys allowed in `server.toml`, per table (`""` is the top level).
const SCHEMA: &[(&str, &[&str])] = &[
    (
        "",
        &[
            "version",
            "node",
            "storage_layouts",
            "root",
            "home",
            "network",
            "cloud_init",
            "user",
            "repo",
        ],
    ),
    ("node", &["name", "gold_vmid"]),
    ("root", &["disk"]),
    ("home", &["disk", "default_size"]),
    ("network", &["prefix", "cidr", "gateway", "dns"]),
    ("cloud_init", &["snippet_storage", "snippet_dir"]),
    ("user", &["name", "uid", "gid"]),
    ("repo", &["owner"]),
];

impl Config {
    /// Parse and validate `server.toml`.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let doc = fields::parse_document(text)?;
        if doc.contains_key("sync") {
            return Err(fields::obsolete_sync());
        }
        for (section, keys) in SCHEMA {
            if section.is_empty() {
                fields::reject_unknown(&doc, "", keys)?;
            } else if let Some(table) = fields::section(&doc, section)? {
                fields::reject_unknown(table, section, keys)?;
            }
        }
        fields::version(&doc)?;
        Self::from_sections(&doc)
    }

    /// Read the server sections of a document whose other keys are someone
    /// else's business (the Python `config.toml`). Checks run in the order of
    /// `config.load_config`.
    pub fn from_sections(doc: &Table) -> Result<Self, ConfigError> {
        use fields::{need, need_int, need_string, section};

        let dns = match need(doc, "network", "dns")? {
            Value::Array(items) => items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| ConfigError::new("network.dns", "[network] dns must be an array of strings"))?,
            _ => {
                return Err(ConfigError::new(
                    "network.dns",
                    "[network] dns must be an array of strings",
                ));
            }
        };
        if dns.is_empty() {
            return Err(ConfigError::new(
                "network.dns",
                "[network] dns must contain at least one DNS server",
            ));
        }

        let node_name = need_string(doc, "node", "name")?;
        let node = NodeName::parse(node_name).map_err(|_| {
            ConfigError::new(
                "node.name",
                "[node] name must be a Proxmox node name: letters, digits and inner hyphens",
            )
        })?;

        let storage_layouts = storage_layouts(doc)?;

        let repo_owner = match section(doc, "repo")?.and_then(|repo| repo.get("owner")) {
            None => None,
            Some(Value::String(owner)) if owner.trim().is_empty() => None,
            Some(Value::String(owner)) => Some(fields::repo_owner(owner)?),
            Some(_) => return Err(ConfigError::new("repo.owner", "[repo] owner must be a string")),
        };

        let user_name = need_string(doc, "user", "name")?.trim().to_owned();
        if user_name.is_empty() {
            return Err(ConfigError::new("user.name", "[user] name must be a non-empty string"));
        }

        let layout = active_layout_name(&node)?;
        if !storage_layouts.contains_key(&layout) {
            return Err(ConfigError::new(
                "storage_layouts",
                format!("Missing [storage_layouts] {layout} for the configured node"),
            ));
        }

        let root_disk = need_string(doc, "root", "disk")?.trim().to_owned();
        let home_disk = need_string(doc, "home", "disk")?.trim().to_owned();
        if root_disk.is_empty() || home_disk.is_empty() {
            return Err(ConfigError::new(
                "root.disk",
                "[root] disk and [home] disk must be non-empty",
            ));
        }
        if root_disk == home_disk {
            return Err(ConfigError::new(
                "root.disk",
                "[root] disk and [home] disk must use different slots",
            ));
        }

        let gold_vmid = section(doc, "node")?
            .and_then(|node| node.get("gold_vmid"))
            .map(|value| {
                value
                    .as_integer()
                    .filter(|vmid| VMID_RANGE.contains(vmid))
                    .and_then(|vmid| u32::try_from(vmid).ok())
                    .ok_or_else(|| {
                        ConfigError::new(
                            "node.gold_vmid",
                            "[node] gold_vmid must be a Proxmox VMID between 100 and 999999999",
                        )
                    })
            })
            .transpose()?;
        let default_home_size = need_string(doc, "home", "default_size")?.to_owned();

        let prefix = need_string(doc, "network", "prefix")?.to_owned();
        let cidr = u8::try_from(need_int(doc, "network", "cidr")?)
            .ok()
            .filter(|cidr| *cidr <= 32)
            .ok_or_else(|| ConfigError::new("network.cidr", "[network] cidr must be between 0 and 32"))?;
        let gateway = need_string(doc, "network", "gateway")?.to_owned();

        let snippet_storage = need_string(doc, "cloud_init", "snippet_storage")?.to_owned();
        let snippet_dir = need_string(doc, "cloud_init", "snippet_dir")?.to_owned();

        let id = |key: &str| {
            u32::try_from(need_int(doc, "user", key)?).map_err(|_| {
                ConfigError::new(
                    format!("user.{key}"),
                    format!("[user] {key} must be between 0 and 4294967295"),
                )
            })
        };
        let uid = id("uid")?;
        let gid = id("gid")?;

        Ok(Self {
            node,
            gold_vmid,
            storage_layouts,
            root_disk,
            home_disk,
            default_home_size,
            network: Network {
                prefix,
                cidr,
                gateway,
                dns,
            },
            cloud_init: CloudInit {
                snippet_storage,
                snippet_dir,
            },
            user: User {
                name: user_name,
                uid,
                gid,
            },
            repo_owner,
        })
    }

    /// Name of the layout the configured node uses.
    #[must_use]
    pub fn active_layout_name(&self) -> String {
        // Validated by `from_sections`.
        active_layout_name(&self.node).unwrap_or_default()
    }

    /// Storages of the configured node, the default first.
    #[must_use]
    pub fn active_layout(&self) -> &[String] {
        self.storage_layouts
            .get(&self.active_layout_name())
            .map_or(&[], Vec::as_slice)
    }

    /// Where `create` puts root and home (Python's `root_storage` and
    /// `home_storage`).
    #[must_use]
    pub fn default_storage(&self) -> &str {
        self.active_layout().first().map_or("", String::as_str)
    }

    /// `server.toml` text in the canonical layout.
    #[must_use]
    pub fn to_toml(&self) -> String {
        use fields::{toml_array, toml_string};
        let mut lines = vec![
            format!("version = {FORMAT_VERSION}"),
            String::new(),
            "[node]".to_owned(),
            format!("name = {}", toml_string(self.node.as_str())),
        ];
        if let Some(vmid) = self.gold_vmid {
            lines.push(format!("gold_vmid = {vmid}"));
        }
        lines.extend([String::new(), "[storage_layouts]".to_owned()]);
        for (layout, storages) in &self.storage_layouts {
            lines.push(format!("{layout} = {}", toml_array(storages)));
        }
        lines.extend([
            String::new(),
            "[root]".to_owned(),
            format!("disk = {}", toml_string(&self.root_disk)),
            String::new(),
            "[home]".to_owned(),
            format!("disk = {}", toml_string(&self.home_disk)),
            format!("default_size = {}", toml_string(&self.default_home_size)),
            String::new(),
            "[network]".to_owned(),
            format!("prefix = {}", toml_string(&self.network.prefix)),
            format!("cidr = {}", self.network.cidr),
            format!("gateway = {}", toml_string(&self.network.gateway)),
            format!("dns = {}", toml_array(&self.network.dns)),
            String::new(),
            "[cloud_init]".to_owned(),
            format!("snippet_storage = {}", toml_string(&self.cloud_init.snippet_storage)),
            format!("snippet_dir = {}", toml_string(&self.cloud_init.snippet_dir)),
            String::new(),
            "[user]".to_owned(),
            format!("name = {}", toml_string(&self.user.name)),
            format!("uid = {}", self.user.uid),
            format!("gid = {}", self.user.gid),
        ]);
        if let Some(owner) = &self.repo_owner {
            lines.extend([
                String::new(),
                "[repo]".to_owned(),
                format!("owner = {}", toml_string(owner)),
            ]);
        }
        lines.push(String::new());
        lines.join("\n")
    }
}

fn storage_layouts(doc: &Table) -> Result<BTreeMap<String, Vec<String>>, ConfigError> {
    let raw = match doc.get("storage_layouts") {
        None => return Ok(BTreeMap::new()),
        Some(Value::Table(raw)) => raw,
        Some(_) => {
            return Err(ConfigError::new(
                "storage_layouts",
                "[storage_layouts] must be a TOML table",
            ));
        }
    };
    let mut layouts = BTreeMap::new();
    for (name, storages) in raw {
        if !is_layout_name(name) {
            return Err(ConfigError::new(
                "storage_layouts",
                "Invalid HomeStack storage layout name; expected homestack-storage-<N>",
            ));
        }
        let field = format!("storage_layouts.{name}");
        let storages = match storages {
            Value::Array(items) if !items.is_empty() => items,
            _ => {
                return Err(ConfigError::new(
                    field,
                    format!("[storage_layouts] {name} must be a non-empty array"),
                ));
            }
        };
        let mut ids = Vec::with_capacity(storages.len());
        for item in storages {
            let id = item.as_str().map(str::trim).ok_or_else(|| {
                ConfigError::new(
                    &field,
                    format!("[storage_layouts] {name} must contain storage IDs as strings"),
                )
            })?;
            if id.is_empty() {
                return Err(ConfigError::new(
                    &field,
                    format!("[storage_layouts] {name} contains an empty storage ID"),
                ));
            }
            if ids.iter().any(|known| known == id) {
                return Err(ConfigError::new(
                    &field,
                    format!("[storage_layouts] {name} contains duplicate storage IDs"),
                ));
            }
            ids.push(id.to_owned());
        }
        layouts.insert(name.clone(), ids);
    }
    Ok(layouts)
}

/// `homestack-storage-<N>`, N without leading zeros.
fn is_layout_name(name: &str) -> bool {
    name.strip_prefix("homestack-storage-").is_some_and(|number| {
        number.bytes().all(|b| b.is_ascii_digit()) && number.bytes().next().is_some_and(|b| (b'1'..=b'9').contains(&b))
    })
}

/// `homestack-storage-<N>` for the trailing number of the node name, as
/// Python's `re.search(r"([1-9][0-9]*)$", node)`.
pub fn active_layout_name(node: &NodeName) -> Result<String, ConfigError> {
    let name = node.as_str();
    let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let number = name[name.len() - digits..].trim_start_matches('0');
    if number.is_empty() {
        return Err(ConfigError::new(
            "node.name",
            "Cannot derive HomeStack storage layout from node name; expected a numeric node suffix",
        ));
    }
    Ok(format!("homestack-storage-{number}"))
}

/// Field readers shared with the reader of the Python `config.toml`
/// (`hs-config`), so both report problems the same way.
pub mod fields {
    use toml::{Table, Value};

    use super::{ConfigError, FORMAT_VERSION};

    /// Parse TOML; a syntax error reports its line, not the text.
    pub fn parse_document(text: &str) -> Result<Table, ConfigError> {
        text.parse::<Table>().map_err(|error| {
            let line = error
                .span()
                .map(|span| text.get(..span.start).unwrap_or(text).matches('\n').count() + 1);
            let message = match line {
                Some(line) => format!("Invalid TOML configuration at line {line}"),
                None => "Invalid TOML configuration".to_owned(),
            };
            ConfigError::new("toml", message)
        })
    }

    pub fn obsolete_sync() -> ConfigError {
        ConfigError::new(
            "sync",
            "Obsolete [sync] configuration is not supported; use [[setup.items]] file entries",
        )
    }

    /// `version = 1`
    pub fn version(doc: &Table) -> Result<(), ConfigError> {
        match doc.get("version") {
            Some(Value::Integer(FORMAT_VERSION)) => Ok(()),
            _ => Err(ConfigError::new(
                "version",
                format!("Unsupported config version; expected {FORMAT_VERSION}"),
            )),
        }
    }

    /// A table, if present.
    pub fn section<'a>(doc: &'a Table, name: &str) -> Result<Option<&'a Table>, ConfigError> {
        match doc.get(name) {
            None => Ok(None),
            Some(Value::Table(table)) => Ok(Some(table)),
            Some(_) => Err(ConfigError::new(name, format!("[{name}] must be a TOML table"))),
        }
    }

    /// A required value, like Python's `_need`.
    pub fn need<'a>(doc: &'a Table, section_name: &str, key: &str) -> Result<&'a Value, ConfigError> {
        section(doc, section_name)?
            .and_then(|table| table.get(key))
            .ok_or_else(|| {
                ConfigError::new(
                    format!("{section_name}.{key}"),
                    format!("Missing configuration value [{section_name}] {key}"),
                )
            })
    }

    pub fn need_string<'a>(doc: &'a Table, section_name: &str, key: &str) -> Result<&'a str, ConfigError> {
        need(doc, section_name, key)?
            .as_str()
            .ok_or_else(|| type_error(section_name, key, "a string"))
    }

    pub fn need_int(doc: &Table, section_name: &str, key: &str) -> Result<i64, ConfigError> {
        need(doc, section_name, key)?
            .as_integer()
            .ok_or_else(|| type_error(section_name, key, "an integer"))
    }

    pub fn type_error(section_name: &str, key: &str, kind: &str) -> ConfigError {
        ConfigError::new(
            format!("{section_name}.{key}"),
            format!("[{section_name}] {key} must be {kind}"),
        )
    }

    /// Fail on a key that is not in `allowed`. The key is named only when
    /// it looks like a configuration key, so arbitrary input is not echoed.
    pub fn reject_unknown(table: &Table, section_name: &str, allowed: &[&str]) -> Result<(), ConfigError> {
        let Some(key) = table.keys().find(|key| !allowed.contains(&key.as_str())) else {
            return Ok(());
        };
        let plain = key.len() <= 64 && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        let (field, place) = match (section_name.is_empty(), plain) {
            (true, true) => (key.clone(), format!("'{key}'")),
            (true, false) => ("document".to_owned(), "at the top level".to_owned()),
            (false, true) => (format!("{section_name}.{key}"), format!("'{key}' in [{section_name}]")),
            (false, false) => (section_name.to_owned(), format!("in [{section_name}]")),
        };
        Err(ConfigError::new(field, format!("Unknown configuration key {place}")))
    }

    /// `[repo] owner`: a GitHub user or organization name, as Python's
    /// `GITHUB_OWNER_RE`. Surrounding whitespace is dropped.
    pub fn repo_owner(value: &str) -> Result<String, ConfigError> {
        let owner = value.trim();
        let bytes = owner.as_bytes();
        let valid = (1..=40).contains(&bytes.len())
            && bytes.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-')
            && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.last().is_some_and(u8::is_ascii_alphanumeric);
        if valid {
            Ok(owner.to_owned())
        } else {
            Err(ConfigError::new(
                "repo.owner",
                "Invalid GitHub repository owner; expected a user or organization name",
            ))
        }
    }

    /// A TOML basic string.
    #[must_use]
    pub fn toml_string(value: &str) -> String {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for c in value.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                '\r' => out.push_str("\\r"),
                c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    #[must_use]
    pub fn toml_array(values: &[String]) -> String {
        let items: Vec<String> = values.iter().map(|value| toml_string(value)).collect();
        format!("[{}]", items.join(", "))
    }
}
