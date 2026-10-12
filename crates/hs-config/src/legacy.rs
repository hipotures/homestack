//! The Python `config.toml`, read with the rules of `config.load_config`.
//!
//! Python ignores tables it does not know; so does this reader, and lists
//! them in [`Legacy::dropped`]. `[[setup.*]]` entries are carried over
//! without checking them; the setup catalog is validated where it is used
//! (phase 14). The parity corpus (`parity/config-corpus.toml`) checks that
//! Python and this reader reach the same verdict.

use hs_model::config::fields::{self, type_error};
use hs_model::config::{Config, ConfigError};
use toml::Value;

use crate::client::ClientConfig;

/// A valid Python configuration, split into its server and client parts.
#[derive(Debug, Clone, PartialEq)]
pub struct Legacy {
    pub server: Config,
    pub client: ClientConfig,
    /// Top-level tables and keys that go to neither file, sorted.
    pub dropped: Vec<String>,
}

/// Top-level keys that are carried over.
const KEPT: [&str; 13] = [
    "version",
    "node",
    "storage_layouts",
    "root",
    "home",
    "network",
    "cloud_init",
    "user",
    "repo",
    "workspace_ssh",
    "display",
    "sounds",
    "setup",
];

pub fn read(text: &str) -> Result<Legacy, ConfigError> {
    let doc = fields::parse_document(text)?;
    if doc.get("install_draft").and_then(Value::as_bool) == Some(true) {
        return Err(ConfigError::new(
            "install_draft",
            "HomeStack configuration is incomplete. Run 'homestack install' with the same --config path to continue.",
        ));
    }
    fields::version(&doc)?;
    if doc.contains_key("sync") {
        return Err(fields::obsolete_sync());
    }
    fields::section(&doc, "control")?;
    transport(&doc)?;

    let client = ClientConfig::from_sections(&doc)?;
    let server = Config::from_sections(&doc)?;
    if client.workspace_ssh.user != server.user.name {
        return Err(ConfigError::new(
            "workspace_ssh.user",
            "[workspace_ssh] user must match [user] name",
        ));
    }
    let dropped = doc
        .keys()
        .filter(|key| !KEPT.contains(&key.as_str()))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(Legacy {
        server,
        client,
        dropped,
    })
}

/// `[transport]`: only `herdr` with a workspace and a tab.
fn transport(doc: &toml::Table) -> Result<(), ConfigError> {
    let transport = fields::section(doc, "transport")?;
    let kind = match transport.and_then(|table| table.get("type")) {
        None => "",
        Some(Value::String(kind)) => kind.trim(),
        Some(_) => return Err(type_error("transport", "type", "a string")),
    };
    if kind.is_empty() {
        return Err(ConfigError::new(
            "transport.type",
            "Missing configuration value [transport] type",
        ));
    }
    if kind != "herdr" {
        return Err(ConfigError::new(
            "transport.type",
            "Unsupported transport type; only 'herdr' is implemented",
        ));
    }
    let herdr = match transport.and_then(|table| table.get("herdr")) {
        None => None,
        Some(Value::Table(herdr)) => Some(herdr),
        Some(_) => {
            return Err(ConfigError::new(
                "transport.herdr",
                "[transport.herdr] must be a TOML table",
            ));
        }
    };
    let get = |key: &str| herdr.and_then(|table| table.get(key));
    if get("debug").is_some_and(|debug| !debug.is_bool()) {
        return Err(ConfigError::new(
            "transport.herdr.debug",
            "[transport.herdr] debug must be true or false",
        ));
    }
    for key in ["workspace", "tab"] {
        match get(key) {
            Some(Value::String(value)) if !value.trim().is_empty() => {}
            Some(Value::String(_)) | None => {
                return Err(ConfigError::new(
                    format!("transport.herdr.{key}"),
                    format!("Missing configuration value [transport.herdr] {key}"),
                ));
            }
            Some(_) => return Err(type_error("transport.herdr", key, "a string")),
        }
    }
    Ok(())
}
