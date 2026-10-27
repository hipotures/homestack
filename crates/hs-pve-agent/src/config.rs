use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_PATH: &str = "/etc/homestack/pve-agent.toml";

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub cgroup_root: PathBuf,
    pub snapshot_path: PathBuf,
    /// The mode the daemon starts in; `homestack-pve-agent mode` switches it.
    pub default_mode: String,
    pub modes: BTreeMap<String, Mode>,
    pub qga_parallel: usize,
    pub debug: bool,
    pub otlp_endpoint: String,
    pub otlp_timeout_seconds: u64,
    pub node_name: String,
}

/// How often one mode measures.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mode {
    /// Every cycle reads cgroups and exports.
    pub interval_seconds: u64,
    pub lxc_filesystem_interval_seconds: u64,
    /// VM filesystems through Guest Agent and virtiofs shares.
    pub qga_interval_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cgroup_root: "/sys/fs/cgroup".into(),
            snapshot_path: "/run/homestack-pve-agent/snapshot.json".into(),
            default_mode: "common".into(),
            modes: BTreeMap::from([
                (
                    "common".into(),
                    Mode {
                        interval_seconds: 60,
                        lxc_filesystem_interval_seconds: 300,
                        qga_interval_seconds: 600,
                    },
                ),
                (
                    "realtime".into(),
                    Mode {
                        interval_seconds: 5,
                        lxc_filesystem_interval_seconds: 30,
                        qga_interval_seconds: 60,
                    },
                ),
            ]),
            qga_parallel: 4,
            debug: false,
            otlp_endpoint: String::new(),
            otlp_timeout_seconds: 3,
            node_name: String::new(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.qga_parallel == 0 {
            return Err("qga_parallel must be a positive integer".into());
        }
        if !self.modes.contains_key(&self.default_mode) {
            return Err(format!("default_mode {} is not one of [modes]", self.default_mode));
        }
        if !(1..=86400).contains(&self.otlp_timeout_seconds) {
            return Err("otlp_timeout_seconds must be between 1 and 86400 seconds".into());
        }
        for (name, mode) in &self.modes {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(format!("mode name {name:?} must use letters, digits, - or _"));
            }
            for (key, seconds) in [
                ("interval_seconds", mode.interval_seconds),
                ("lxc_filesystem_interval_seconds", mode.lxc_filesystem_interval_seconds),
                ("qga_interval_seconds", mode.qga_interval_seconds),
            ] {
                if !(1..=86400).contains(&seconds) {
                    return Err(format!("modes.{name}.{key} must be between 1 and 86400 seconds"));
                }
            }
        }
        if !self.otlp_endpoint.is_empty() {
            let url =
                reqwest::Url::parse(&self.otlp_endpoint).map_err(|_| "otlp_endpoint must be an HTTP metrics URL")?;
            if url.scheme() != "http"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || url.path() != "/v1/metrics"
            {
                return Err(
                    "otlp_endpoint must be http://COLLECTOR:4318/v1/metrics without credentials, query or fragment"
                        .into(),
                );
            }
        }
        if !self.cgroup_root.is_absolute()
            || !self.snapshot_path.is_absolute()
            || self.snapshot_path.file_name().is_none()
        {
            return Err("cgroup_root and snapshot_path must be absolute paths; snapshot_path must name a file".into());
        }
        Ok(())
    }

    /// The daemon's control socket, beside its snapshot.
    pub fn control_socket(&self) -> PathBuf {
        self.snapshot_path.with_file_name("control.sock")
    }

    pub fn node(&self) -> Result<String, String> {
        if !self.node_name.is_empty() {
            return Ok(self.node_name.clone());
        }
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").map_err(|e| e.to_string())?;
        let node = hostname.trim().split('.').next().unwrap_or_default();
        if node.is_empty() {
            return Err("cannot determine node name; set node_name in config".into());
        }
        Ok(node.to_owned())
    }
}
