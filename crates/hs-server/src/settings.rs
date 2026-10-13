//! Startup settings from environment variables.
//!
//! These say how this instance runs and reaches Proxmox. What HomeStack
//! does is in `server.toml`, uploaded through `PUT /v1/config` and kept in
//! `HOMESTACK_CONFIG_FILE`.
//!
//! | Variable | Meaning |
//! |---|---|
//! | `HOMESTACK_LISTEN` | Listen address, default `127.0.0.1:8080` |
//! | `HOMESTACK_API_TOKEN_FILE` | File with the client bearer token (required) |
//! | `HOMESTACK_PVE_ENDPOINTS` | Comma-separated Proxmox API origins, tried in order (required) |
//! | `HOMESTACK_PVE_TOKEN_FILE` | File with `USER@REALM!TOKENID=SECRET` (required) |
//! | `HOMESTACK_PVE_CA_FILE` | Extra PEM CA trusted for Proxmox, besides the system roots |
//! | `HOMESTACK_CONFIG_FILE` | Stored `server.toml`, default `/var/lib/homestack/server.toml` |
//! | `HOMESTACK_PVE_POOL` | Pool the instance changes workspaces in, default `homestack` |
//! | `HOMESTACK_PVE_GOLD_POOL` | Gold's pool, default `homestack-gold` |
//! | `HOMESTACK_LOG` | Log filter, default `info` |
//! | `HOMESTACK_LOG_FORMAT` | `text` (default) or `json` |

use std::net::SocketAddr;
use std::path::PathBuf;

use hs_pve::{PveClientConfig, PveToken};
use url::Url;

use crate::{ApiToken, Pools, ServerConfig};

/// Where `server.toml` is kept unless `HOMESTACK_CONFIG_FILE` says otherwise.
pub const DEFAULT_CONFIG_FILE: &str = "/var/lib/homestack/server.toml";

pub struct Settings {
    pub listen: SocketAddr,
    pub server: ServerConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

/// Read settings with the given variable lookup (the process environment in
/// production).
pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Result<Settings, String> {
    let required = |name: &str| {
        var(name)
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| format!("{name} is required"))
    };
    let listen = match var("HOMESTACK_LISTEN") {
        Some(value) => value
            .parse()
            .map_err(|_| "HOMESTACK_LISTEN must be an address like 127.0.0.1:8080".to_owned())?,
        None => SocketAddr::from(([127, 0, 0, 1], 8080)),
    };
    let api_token =
        ApiToken::from_file(&PathBuf::from(required("HOMESTACK_API_TOKEN_FILE")?)).map_err(|e| e.to_string())?;
    let endpoints = required("HOMESTACK_PVE_ENDPOINTS")?
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|e| {
            Url::parse(e)
                .map_err(|_| "HOMESTACK_PVE_ENDPOINTS must hold URLs like https://pve1.example:8006".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let token =
        PveToken::from_file(&PathBuf::from(required("HOMESTACK_PVE_TOKEN_FILE")?)).map_err(|e| e.to_string())?;
    let mut pve = PveClientConfig::new(endpoints, token);
    if let Some(path) = var("HOMESTACK_PVE_CA_FILE").filter(|v| !v.trim().is_empty()) {
        pve.extra_ca_pem =
            Some(std::fs::read_to_string(&path).map_err(|e| format!("HOMESTACK_PVE_CA_FILE {path}: {}", e.kind()))?);
    }
    Ok(Settings {
        listen,
        server: ServerConfig {
            api_token,
            pve,
            config_file: var("HOMESTACK_CONFIG_FILE")
                .filter(|v| !v.trim().is_empty())
                .map_or_else(|| PathBuf::from(DEFAULT_CONFIG_FILE), PathBuf::from),
            pools: pools(&var)?,
        },
    })
}

/// `HOMESTACK_PVE_POOL` and `HOMESTACK_PVE_GOLD_POOL`, checked like
/// Proxmox's pool names (`pve-poolid`: letters, digits, `.`, `-`, `_`, up to
/// three levels separated by `/`).
fn pools(var: &impl Fn(&str) -> Option<String>) -> Result<Pools, String> {
    let defaults = Pools::default();
    let read = |name: &str, default: String| {
        let value = var(name).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        match value {
            None => Ok(default),
            Some(value) => {
                let levels: Vec<&str> = value.split('/').collect();
                let valid = levels.len() <= 3
                    && levels.iter().all(|level| {
                        !level.is_empty() && level.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
                    });
                if valid {
                    Ok(value)
                } else {
                    Err(format!("{name} must be a Proxmox pool name like homestack"))
                }
            }
        }
    };
    let pools = Pools {
        workspaces: read("HOMESTACK_PVE_POOL", defaults.workspaces)?,
        gold: read("HOMESTACK_PVE_GOLD_POOL", defaults.gold)?,
    };
    if pools.workspaces == pools.gold {
        return Err("HOMESTACK_PVE_POOL and HOMESTACK_PVE_GOLD_POOL must differ (decision 9)".to_owned());
    }
    Ok(pools)
}

pub fn log_format(var: impl Fn(&str) -> Option<String>) -> Result<LogFormat, String> {
    match var("HOMESTACK_LOG_FORMAT").as_deref() {
        None | Some("text") => Ok(LogFormat::Text),
        Some("json") => Ok(LogFormat::Json),
        Some(_) => Err("HOMESTACK_LOG_FORMAT must be text or json".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        move |name| pairs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn pools_default_to_the_install_scripts() {
        assert_eq!(
            pools(&env(&[])),
            Ok(Pools {
                workspaces: "homestack".to_owned(),
                gold: "homestack-gold".to_owned()
            })
        );
    }

    #[test]
    fn a_test_instance_uses_the_test_pool() {
        let pools = pools(&env(&[("HOMESTACK_PVE_POOL", " homestack-test ")]));
        assert_eq!(pools.map(|p| p.workspaces), Ok("homestack-test".to_owned()));
    }

    #[test]
    fn pool_names_are_checked_like_proxmox() {
        for bad in ["a b", "a/b/c/d", "a//b", "pool!"] {
            assert!(pools(&env(&[("HOMESTACK_PVE_POOL", bad)])).is_err(), "{bad}");
        }
        assert!(pools(&env(&[("HOMESTACK_PVE_POOL", "team/homestack_1.x")])).is_ok());
        assert!(pools(&env(&[("HOMESTACK_PVE_GOLD_POOL", "homestack")])).is_err());
    }
}
