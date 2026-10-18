//! Scenario files in `scenarios/`.
//!
//! The `[pve]` table is the simulated cluster (read by `pve-sim`); the rest
//! identifies a frozen reference, the Rust request and the expected result.

use std::path::PathBuf;

use pve_sim::ClusterSpec;
use serde::Deserialize;

#[derive(Debug)]
pub struct Scenario {
    pub meta: Meta,
    pub homestack: HomeStack,
    pub parity: Vec<Parity>,
    pub pve: ClusterSpec,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    pub name: String,
    pub phase: String,
    pub description: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeStack {
    /// Installation input, relative to the repository root.
    pub config: PathBuf,
    /// Token the Rust server uses; defaults to `homestack@pve!server`.
    #[serde(default = "default_server_token")]
    pub server_token: String,
}

fn default_server_token() -> String {
    "homestack@pve!server".to_owned()
}

/// One Rust request compared with its frozen reference.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parity {
    /// Frozen Python output and operations, relative to the repository root.
    pub reference: PathBuf,
    /// Server request as `METHOD /path`, `@NAME ARGS` for a server
    /// function without a route, or `cli ARGS` for the Rust client's
    /// `homestack ARGS` against the server (`crates/hs-parity/tests/scenarios.rs`).
    pub rust: String,
    /// Rule in `parity/normalize.toml`.
    pub normalize: String,
    #[serde(default)]
    pub expect: Expect,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// Operations both runs must issue, in order.
    #[serde(default)]
    pub ops: Vec<String>,
    /// Normalized body both runs must produce.
    pub body: Option<toml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    scenario: Meta,
    homestack: HomeStack,
    #[serde(default)]
    parity: Vec<Parity>,
    #[allow(dead_code, reason = "parsed by pve-sim from the same text")]
    pve: toml::Value,
}

impl Scenario {
    pub fn parse(text: &str) -> Result<Self, String> {
        let document: Document = toml::from_str(text).map_err(|error| error.to_string())?;
        let pve = ClusterSpec::from_scenario(text)?;
        Ok(Self {
            meta: document.scenario,
            homestack: document.homestack,
            parity: document.parity,
            pve,
        })
    }

    /// `user@realm!id=secret` for a token defined in `[pve]`.
    pub fn token(&self, id: &str) -> Result<String, String> {
        self.pve
            .tokens
            .iter()
            .find(|token| token.id == id)
            .map(|token| format!("{}={}", token.id, token.secret))
            .ok_or_else(|| format!("scenario {}: no token {id} in [pve]", self.meta.name))
    }
}
