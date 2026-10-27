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
        ] {
            if !(1..=86400).contains(&seconds) {
                return Err(format!("{name} must be between 1 and 86400 seconds"));
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
}
