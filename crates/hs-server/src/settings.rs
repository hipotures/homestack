//! Startup settings from environment variables.
//!
//! These say how this instance runs and reaches Proxmox. What HomeStack
//! does is in `server.toml`, uploaded through `PUT /v1/config` and kept in
//! `HOMESTACK_CONFIG_FILE`.
//!
//! | Variable | Meaning |
//! |---|---|
//! | `HOMESTACK_LISTEN` | Listen address, default `127.0.0.1:8080` |
//! | `HOMESTACK_HEARTBEAT_SECONDS` | Serving-path heartbeat interval, 1 to 3600 seconds; default 60 |
//! | `HOMESTACK_API_TOKEN_FILE` | File with the client bearer token |
//! | `HOMESTACK_API_TOKEN_SHA256` | Or the SHA-256 (hex) of that token; one of the two is required |
//! | `HOMESTACK_PVE_ENDPOINTS` | Comma-separated Proxmox API origins, tried in order (required) |
//! | `HOMESTACK_PVE_TOKEN_FILE` | File with `USER@REALM!TOKENID=SECRET`; without it the token is sent through `PUT /v1/secrets/pve-token` |
//! | `HOMESTACK_PVE_CA_FILE` | Extra PEM CA trusted for Proxmox, besides the system roots |
//! | `HOMESTACK_STATE_DIR` | State directory, default `/var/lib/homestack`: `server.toml`, `secrets/`, `tls/` |
//! | `HOMESTACK_CONFIG_FILE` | Stored `server.toml`, default `server.toml` in the state directory |
//! | `HOMESTACK_INSTANCE` | Name of the instance in its leases, default `homestack` |
//! | `HOMESTACK_TLS_CERT_B64`, `HOMESTACK_TLS_KEY_B64` | Certificate chain and key (PEM, base64), stored at the first start |
//! | `HOMESTACK_CADDY_ADMIN` | Caddy's admin API, told to load a new certificate |
//! | `HOMESTACK_PVE_POOL` | Pool the instance changes workspaces in, default `homestack` |
//! | `HOMESTACK_PVE_GOLD_POOL` | Gold's pool, default `homestack-gold` |
//! | `HOMESTACK_LOG` | Log filter, default `info` |
//! | `HOMESTACK_LOG_FORMAT` | `text` (default) or `json` |

use std::net::SocketAddr;
use std::path::PathBuf;

use base64::Engine;
use hs_model::jobs::Secret;
use hs_pve::PveToken;
use url::Url;

use crate::{ApiAuth, ApiToken, Instance, Pools, PveSettings, ServerConfig, TlsPair};

/// The state directory unless `HOMESTACK_STATE_DIR` says otherwise.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/homestack";
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
    let set = |name: &str| var(name).filter(|v| !v.trim().is_empty());
    let required = |name: &str| set(name).ok_or_else(|| format!("{name} is required"));
    let listen = match var("HOMESTACK_LISTEN") {
        Some(value) => value
            .parse()
            .map_err(|_| "HOMESTACK_LISTEN must be an address like 127.0.0.1:8080".to_owned())?,
        None => SocketAddr::from(([127, 0, 0, 1], 8080)),
    };
    let api_token = match (set("HOMESTACK_API_TOKEN_FILE"), set("HOMESTACK_API_TOKEN_SHA256")) {
        (Some(path), None) => ApiAuth::from(ApiToken::from_file(&PathBuf::from(path)).map_err(|e| e.to_string())?),
        (None, Some(hash)) => ApiAuth::from_sha256_hex(&hash).map_err(|e| e.to_string())?,
        (Some(_), Some(_)) => {
            return Err("set HOMESTACK_API_TOKEN_FILE or HOMESTACK_API_TOKEN_SHA256, not both".to_owned());
        }
        (None, None) => return Err("HOMESTACK_API_TOKEN_FILE or HOMESTACK_API_TOKEN_SHA256 is required".to_owned()),
    };
    let endpoints = required("HOMESTACK_PVE_ENDPOINTS")?
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|e| {
            Url::parse(e)
                .map_err(|_| "HOMESTACK_PVE_ENDPOINTS must hold URLs like https://pve1.example:8006".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let token = set("HOMESTACK_PVE_TOKEN_FILE")
        .map(|path| PveToken::from_file(&PathBuf::from(path)).map_err(|e| e.to_string()))
        .transpose()?;
    let mut pve = PveSettings {
        endpoints,
        token,
        extra_ca_pem: None,
        connect_timeout: hs_pve::DEFAULT_CONNECT_TIMEOUT,
        request_timeout: hs_pve::DEFAULT_REQUEST_TIMEOUT,
    };
    if let Some(path) = set("HOMESTACK_PVE_CA_FILE") {
        pve.extra_ca_pem =
            Some(std::fs::read_to_string(&path).map_err(|e| format!("HOMESTACK_PVE_CA_FILE {path}: {}", e.kind()))?);
    }
    let state_dir = PathBuf::from(set("HOMESTACK_STATE_DIR").unwrap_or_else(|| DEFAULT_STATE_DIR.to_owned()));
    Ok(Settings {
        listen,
        server: ServerConfig {
            heartbeat: std::sync::Arc::new(crate::health::Heartbeat::new(std::time::Duration::from_secs(
                set("HOMESTACK_HEARTBEAT_SECONDS")
                    .map(|value| {
                        value
                            .parse::<u64>()
                            .ok()
                            .filter(|seconds| (1..=3600).contains(seconds))
                            .ok_or_else(|| "HOMESTACK_HEARTBEAT_SECONDS must be an integer from 1 to 3600".to_owned())
                    })
                    .transpose()?
                    .unwrap_or(60),
            ))),
            api_token,
            pve,
            config_file: set("HOMESTACK_CONFIG_FILE").map_or_else(|| state_dir.join("server.toml"), PathBuf::from),
            pools: pools(&var)?,
            instance: Instance {
                name: instance_name(set("HOMESTACK_INSTANCE"))?,
                tls_bootstrap: tls_bootstrap(set("HOMESTACK_TLS_CERT_B64"), set("HOMESTACK_TLS_KEY_B64"))?,
                caddy_admin: set("HOMESTACK_CADDY_ADMIN")
                    .map(|url| {
                        Url::parse(url.trim())
                            .map_err(|_| "HOMESTACK_CADDY_ADMIN must be a URL like http://127.0.0.1:2019".to_owned())
                    })
                    .transpose()?,
                state_dir: Some(state_dir),
            },
        },
    })
}

/// `HOMESTACK_INSTANCE`: a DNS label (it is the instance's host name).
fn instance_name(value: Option<String>) -> Result<String, String> {
    let Some(name) = value.map(|v| v.trim().to_owned()) else {
        return Ok(crate::DEFAULT_INSTANCE.to_owned());
    };
    let valid = !name.is_empty()
        && name.len() <= 63
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if valid {
        Ok(name)
    } else {
        Err("HOMESTACK_INSTANCE must be a host name like api1 (lowercase letters, digits, -)".to_owned())
    }
}

/// `HOMESTACK_TLS_CERT_B64` and `HOMESTACK_TLS_KEY_B64`: both or neither.
fn tls_bootstrap(cert: Option<String>, key: Option<String>) -> Result<Option<TlsPair>, String> {
    let decode = |name: &str, text: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(text.trim())
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .ok_or_else(|| format!("{name} must be base64 of PEM text"))
    };
    match (cert, key) {
        (None, None) => Ok(None),
        (Some(cert), Some(key)) => {
            let cert = decode("HOMESTACK_TLS_CERT_B64", &cert)?;
            let key = Secret::new(decode("HOMESTACK_TLS_KEY_B64", &key)?);
            TlsPair::checked(cert, key).map(Some).map_err(str::to_owned)
        }
        _ => Err("set both HOMESTACK_TLS_CERT_B64 and HOMESTACK_TLS_KEY_B64, or neither".to_owned()),
    }
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
