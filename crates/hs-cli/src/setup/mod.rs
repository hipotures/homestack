//! `homestack setup` (phase 14, decision 7 = A): the setup stays in the
//! client and reaches the workspace over SSH with the hardware key, as the
//! Python HomeStack does. The program run on the workspace
//! (`guest/setup_guest.py`) is Python's, sent unchanged.

pub mod backup;
pub mod catalog;
pub mod cli;
pub mod definitions;
pub mod documents;
pub mod engine;
pub mod files;
pub mod guest;
pub mod herdr;
pub mod plan;
pub mod repo;
pub mod show;
pub mod text;
pub mod workspace;

use std::path::PathBuf;

use definitions::SetupConfig;

/// BK's files in the home (Python `backup.MANAGED_ASSETS`).
pub const BACKUP_PATHS: [&str; 3] = [
    ".local/bin/bk",
    ".config/systemd/user/backup.service",
    ".config/systemd/user/backup.timer",
];
/// The guest file BK's retrieval keys go into.
pub const AUTHORIZED_KEYS_PATH: &str = ".ssh/authorized_keys";
/// Herdr's user unit (Python `setup_herdr.UNIT_PATH`).
pub const HERDR_UNIT_PATH: &str = ".config/systemd/user/herdr.service";

/// `[repo]` of `client.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSettings {
    pub owner: Option<String>,
    /// `~/...`.
    pub checkout_root: String,
    /// `recent`, `created` or `name`.
    pub sort: String,
}

impl Default for RepoSettings {
    fn default() -> Self {
        Self {
            owner: None,
            checkout_root: "~/DEV".to_owned(),
            sort: "recent".to_owned(),
        }
    }
}

/// What the setup reads of the configuration (Python's `Config` fields).
#[derive(Debug, Clone)]
pub struct Settings {
    /// The desktop's home: sources of copied files.
    pub home: PathBuf,
    /// The workspace user (`[user]` of the server).
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub setup: SetupConfig,
    pub repo: RepoSettings,
    /// `client.toml`: the scope of catalog snapshots.
    pub config_path: PathBuf,
    /// `XDG_STATE_HOME`, else `~/.local/state`.
    pub state_home: PathBuf,
    /// `[workspace_ssh]` keys: what the desktop's alias of a workspace uses.
    pub keys: Keys,
}

impl Settings {
    /// `/home/<user>` on the workspace.
    #[must_use]
    pub fn guest_home(&self) -> String {
        format!("/home/{}", self.user)
    }
}

/// The resolved workspace a setup runs on (Python's target dict).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub vmid: u32,
    pub name: String,
    pub node: String,
    pub status: String,
    pub alien: bool,
    pub ip: String,
    pub user: String,
    pub home: String,
    /// `setup --local`: the desktop's own user, no SSH.
    pub local: bool,
}

impl Target {
    /// The target as Python's plans and results show it.
    #[must_use]
    pub fn json(&self) -> serde_json::Value {
        if self.local {
            return serde_json::json!({
                "vmid": 0, "name": self.name, "local": true, "user": self.user, "home": self.home,
            });
        }
        serde_json::json!({
            "vmid": self.vmid, "name": self.name, "node": self.node, "status": self.status,
            "alien": self.alien, "ip": self.ip, "user": self.user, "home": self.home,
        })
    }
}

/// A TOML value as JSON (tables keep no order in JSON).
#[must_use]
pub fn json_of(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(text) => serde_json::Value::String(text.clone()),
        toml::Value::Integer(number) => serde_json::Value::from(*number),
        toml::Value::Float(number) => serde_json::Value::from(*number),
        toml::Value::Boolean(flag) => serde_json::Value::Bool(*flag),
        toml::Value::Datetime(datetime) => serde_json::Value::String(datetime.to_string()),
        toml::Value::Array(items) => serde_json::Value::Array(items.iter().map(json_of).collect()),
        toml::Value::Table(table) => {
            serde_json::Value::Object(table.iter().map(|(key, value)| (key.clone(), json_of(value))).collect())
        }
    }
}

/// SHA-256 as lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    use std::fmt::Write;
    let mut hex = String::with_capacity(64);
    for byte in sha2::Sha256::digest(bytes).iter() {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Now as `YYYY-MM-DDTHH:MM:SSZ`.
#[must_use]
pub fn utc_now() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

/// The keys of `[workspace_ssh]` the desktop's SSH alias must offer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keys {
    pub identity_files: Vec<String>,
    pub identities_only: bool,
}
