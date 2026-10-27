use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_PATH: &str = "/etc/homestack/pve-agent.toml";

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub cgroup_root: PathBuf,
    pub snapshot_path: PathBuf,
    pub interval_seconds: u64,
    pub lxc_filesystem_interval_seconds: u64,
    pub qga_interval_seconds: u64,
    pub qga_parallel: usize,
    pub debug: bool,
    pub otlp_endpoint: String,
    pub otlp_timeout_seconds: u64,
    pub node_name: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cgroup_root: "/sys/fs/cgroup".into(),
            snapshot_path: "/run/homestack-pve-agent/snapshot.json".into(),
            interval_seconds: 5,
            lxc_filesystem_interval_seconds: 30,
            qga_interval_seconds: 60,
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
        for (name, seconds) in [
            ("interval_seconds", self.interval_seconds),
            ("lxc_filesystem_interval_seconds", self.lxc_filesystem_interval_seconds),
            ("qga_interval_seconds", self.qga_interval_seconds),
            ("otlp_timeout_seconds", self.otlp_timeout_seconds),
        ] {
            if !(1..=86400).contains(&seconds) {
                return Err(format!("{name} must be between 1 and 86400 seconds"));
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
