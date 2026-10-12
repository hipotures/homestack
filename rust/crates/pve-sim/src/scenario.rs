//! The `[pve]` part of a scenario: the initial state of the simulated cluster.
//!
//! Unknown keys are errors, so a typo cannot silently change a scenario.

use std::collections::BTreeMap;

use hs_model::NodeName;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterSpec {
    pub version: VersionSpec,
    #[serde(default)]
    pub nodes: Vec<NodeSpec>,
    #[serde(default)]
    pub tokens: Vec<TokenSpec>,
    #[serde(default)]
    pub tasks: Vec<TaskSpec>,
    #[serde(default)]
    pub faults: Vec<FaultSpec>,
}

/// Body of `GET /version`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionSpec {
    pub release: String,
    pub version: String,
    pub repoid: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSpec {
    pub name: NodeName,
    #[serde(default)]
    pub status: NodeState,
    pub pveversion: String,
    pub kversion: String,
    pub uptime: u64,
    #[serde(default)]
    pub cpu: f64,
    #[serde(default = "default_maxcpu")]
    pub maxcpu: u32,
    #[serde(default)]
    pub mem: u64,
    #[serde(default)]
    pub maxmem: u64,
    #[serde(default)]
    pub disk: u64,
    #[serde(default)]
    pub maxdisk: u64,
}

const fn default_maxcpu() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeState {
    #[default]
    Online,
    Offline,
}

impl NodeState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Offline => "offline",
        }
    }
}

/// An API token and its effective ACL.
///
/// The ACL is what the token can actually do, i.e. already the intersection
/// of user and token privileges for a privilege-separated token. A privilege
/// granted on a path also applies below it (`propagate=1`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenSpec {
    /// `user@realm!tokenid`
    pub id: String,
    pub secret: String,
    #[serde(default)]
    pub acl: BTreeMap<String, Vec<String>>,
}

/// A task that exists when the scenario starts.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub node: NodeName,
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub user: String,
    #[serde(default)]
    pub log: Vec<String>,
    /// Number of status polls after which the task reports `stopped`.
    #[serde(default)]
    pub polls_until_done: u32,
    #[serde(default = "default_exitstatus")]
    pub exitstatus: String,
}

fn default_exitstatus() -> String {
    "OK".to_owned()
}

/// Make the `occurrence`-th call of `op` (1-based) fail with `status`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaultSpec {
    pub op: String,
    pub occurrence: u32,
    pub status: u16,
    pub message: String,
}

impl ClusterSpec {
    /// Read the `[pve]` table of a scenario file.
    pub fn from_scenario(text: &str) -> Result<Self, String> {
        let mut table: toml::Table = toml::from_str(text).map_err(|error| error.to_string())?;
        let pve = table.remove("pve").ok_or("the scenario has no [pve] table")?;
        pve.try_into().map_err(|error: toml::de::Error| error.to_string())
    }
}
