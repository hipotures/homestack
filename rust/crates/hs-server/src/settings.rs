//! Startup settings from environment variables.
//!
//! Phase 1 of the migration replaces most of these with `server.toml`.
//!
//! | Variable | Meaning |
//! |---|---|
//! | `HOMESTACK_LISTEN` | Listen address, default `127.0.0.1:8080` |
//! | `HOMESTACK_API_TOKEN_FILE` | File with the client bearer token (required) |
//! | `HOMESTACK_PVE_ENDPOINTS` | Comma-separated Proxmox API origins, tried in order (required) |
//! | `HOMESTACK_PVE_TOKEN_FILE` | File with `USER@REALM!TOKENID=SECRET` (required) |
//! | `HOMESTACK_PVE_CA_FILE` | Extra PEM CA trusted for Proxmox, besides the system roots |
//! | `HOMESTACK_LOG` | Log filter, default `info` |
//! | `HOMESTACK_LOG_FORMAT` | `text` (default) or `json` |

use std::net::SocketAddr;
use std::path::PathBuf;

use hs_pve::{PveClientConfig, PveToken};
use url::Url;

use crate::{ApiToken, ServerConfig};

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
        server: ServerConfig { api_token, pve },
    })
}

pub fn log_format(var: impl Fn(&str) -> Option<String>) -> Result<LogFormat, String> {
    match var("HOMESTACK_LOG_FORMAT").as_deref() {
        None | Some("text") => Ok(LogFormat::Text),
        Some("json") => Ok(LogFormat::Json),
        Some(_) => Err("HOMESTACK_LOG_FORMAT must be text or json".to_owned()),
    }
}
