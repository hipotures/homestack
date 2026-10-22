//! HomeStack server: an HTTP API in front of the Proxmox VE API.
//!
//! Every `/v1` route needs `Authorization: Bearer <token>`. Errors use the
//! document in [`hs_model::ErrorBody`]; every response carries
//! `x-request-id` and `Cache-Control: no-store`.

mod auth;
pub mod config_store;
mod error;
mod image_updates;
mod jobs;
pub mod lifecycle;
mod openapi;
pub mod operations;
pub mod precondition;
mod reads;
mod routes;
mod secrets;
pub mod settings;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::middleware;
use hs_model::ErrorCode;
use hs_pve::{PveClient, PveClientConfig, PveError, PveToken};
use tower_http::set_header::SetResponseHeaderLayer;
use url::Url;

pub use auth::{ApiAuth, ApiToken, ApiTokenError};
use config_store::{ConfigStore, StoreError};
use error::ApiError;
pub use openapi::openapi_json;
pub use operations::AppOptions;
use operations::OpsState;
pub use secrets::TlsPair;

/// Everything the server needs to run.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// What clients must present.
    pub api_token: ApiAuth,
    /// How to reach Proxmox.
    pub pve: PveSettings,
    /// Where the uploaded `server.toml` is kept.
    pub config_file: PathBuf,
    /// The Proxmox pools of this instance.
    pub pools: Pools,
    /// This instance among several (phase 11).
    pub instance: Instance,
}

/// How to reach Proxmox. Without `token` the instance waits for one through
/// `PUT /v1/secrets/pve-token` (or finds the one sent before in its state
/// directory).
#[derive(Debug, Clone)]
pub struct PveSettings {
    pub endpoints: Vec<Url>,
    /// From `HOMESTACK_PVE_TOKEN_FILE`; it then cannot be replaced.
    pub token: Option<PveToken>,
    pub extra_ca_pem: Option<String>,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
}

impl PveSettings {
    fn client_config(&self, token: PveToken) -> PveClientConfig {
        let mut config = PveClientConfig::new(self.endpoints.clone(), token);
        config.extra_ca_pem.clone_from(&self.extra_ca_pem);
        config.connect_timeout = self.connect_timeout;
        config.request_timeout = self.request_timeout;
        config
    }
}

impl From<PveClientConfig> for PveSettings {
    fn from(config: PveClientConfig) -> Self {
        Self {
            endpoints: config.endpoints,
            token: Some(config.token),
            extra_ca_pem: config.extra_ca_pem,
            connect_timeout: config.connect_timeout,
            request_timeout: config.request_timeout,
        }
    }
}

/// The pool this instance changes workspaces in and Gold's pool (phase 4b,
/// decision 9), as `deploy/pve/homestack-pve.sh` makes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pools {
    pub workspaces: String,
    pub gold: String,
}

impl Default for Pools {
    fn default() -> Self {
        Self {
            workspaces: hs_core::capabilities::DEFAULT_POOL.to_owned(),
            gold: hs_core::capabilities::DEFAULT_GOLD_POOL.to_owned(),
        }
    }
}

/// One instance among several (`phase-11.md`).
#[derive(Debug, Clone)]
pub struct Instance {
    /// `HOMESTACK_INSTANCE`: names the instance in the leases it writes.
    pub name: String,
    /// Where the secrets sent through the API are kept (`secrets/`,
    /// `tls/`); without it they cannot be sent.
    pub state_dir: Option<PathBuf>,
    /// The certificate of the environment, stored once at the first start.
    pub tls_bootstrap: Option<TlsPair>,
    /// Caddy's admin API, told to load a new certificate.
    pub caddy_admin: Option<Url>,
}

/// The name of an instance that was given none.
pub const DEFAULT_INSTANCE: &str = "homestack";

impl Default for Instance {
    fn default() -> Self {
        Self {
            name: DEFAULT_INSTANCE.to_owned(),
            state_dir: None,
            tls_bootstrap: None,
            caddy_admin: None,
        }
    }
}

/// Why the server cannot start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Pve(#[from] PveError),
    #[error(transparent)]
    Config(#[from] StoreError),
    #[error("{0}")]
    Secrets(String),
    #[error("Could not start service telemetry: {0}")]
    Telemetry(#[source] std::io::Error),
}

/// The Proxmox client, replaced when a new token is sent.
pub(crate) struct PveHandle {
    settings: PveSettings,
    current: RwLock<Option<(Arc<PveClient>, String)>>,
}

impl PveHandle {
    fn new(settings: PveSettings, stored: Option<PveToken>) -> Result<Self, PveError> {
        let token = settings.token.clone().or(stored);
        let current = match token {
            Some(token) => {
                let id = token.id().to_owned();
                Some((Arc::new(PveClient::new(settings.client_config(token))?), id))
            }
            None => None,
        };
        Ok(Self {
            settings,
            current: RwLock::new(current),
        })
    }

    pub(crate) fn client(&self) -> Result<Arc<PveClient>, ApiError> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|(client, _)| Arc::clone(client))
            .ok_or(ApiError::new(
                ErrorCode::PveTokenMissing,
                "The server has no Proxmox token yet; send one with PUT /v1/secrets/pve-token.",
            ))
    }

    pub(crate) fn token_id(&self) -> Option<String> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|(_, id)| id.clone())
    }

    /// Whether the token came from `HOMESTACK_PVE_TOKEN_FILE`.
    pub(crate) const fn fixed(&self) -> bool {
        self.settings.token.is_some()
    }

    pub(crate) fn replace(&self, token: PveToken) -> Result<(), PveError> {
        let id = token.id().to_owned();
        let client = Arc::new(PveClient::new(self.settings.client_config(token))?);
        *self.current.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((client, id));
        Ok(())
    }
}

#[derive(Clone)]
struct AppState {
    pve: Arc<PveHandle>,
    api_token: Arc<ApiAuth>,
    pools: Arc<Pools>,
    config: Arc<ConfigStore>,
    ops: Arc<OpsState>,
    instance: Arc<Instance>,
    image_updates: Arc<image_updates::ImageUpdates>,
    service_status: hs_service_status::Sampler,
}

impl AppState {
    /// The Proxmox client, or `503 pve_token_missing`.
    fn pve(&self) -> Result<Arc<PveClient>, ApiError> {
        self.pve.client()
    }
}

/// Build the HTTP application. Loads the stored configuration, if any.
pub fn app(config: ServerConfig) -> Result<Router, StartError> {
    app_with(config, AppOptions::default())
}

/// [`app`] with the operations clients may plan and the settings of the
/// operation model.
pub fn app_with(config: ServerConfig, options: AppOptions) -> Result<Router, StartError> {
    let store = ConfigStore::open(config.config_file)?;
    if let Some(current) = store.current() {
        tracing::info!(path = %store.path().display(), config_version = current.version, "configuration loaded");
    } else {
        tracing::warn!(path = %store.path().display(), "no configuration yet; upload one with PUT /v1/config");
    }
    let instance = config.instance;
    if let Some(dir) = &instance.state_dir {
        secrets::store_bootstrap_tls(dir, instance.tls_bootstrap.as_ref()).map_err(StartError::Secrets)?;
    }
    let stored = match (&config.pve.token, &instance.state_dir) {
        (None, Some(dir)) => secrets::stored_pve_token(dir).map_err(StartError::Secrets)?,
        _ => None,
    };
    let pve = PveHandle::new(config.pve, stored)?;
    if pve.token_id().is_none() {
        tracing::warn!("no Proxmox token yet; send one with PUT /v1/secrets/pve-token");
    }
    let state = AppState {
        pve: Arc::new(pve),
        pools: Arc::new(config.pools),
        api_token: Arc::new(config.api_token),
        config: Arc::new(store),
        ops: Arc::new(OpsState::new(options, instance.name.clone())),
        instance: Arc::new(instance),
        image_updates: Arc::default(),
        service_status: hs_service_status::Sampler::start().map_err(StartError::Telemetry)?,
    };
    image_updates::start(&state.pve, &state.image_updates);
    let v1 = routes::v1().route_layer(middleware::from_fn_with_state(state.clone(), auth::require_bearer));
    Ok(Router::new()
        .merge(routes::public())
        .merge(v1)
        .fallback(error::not_found)
        .method_not_allowed_fallback(error::method_not_allowed)
        .layer(middleware::from_fn(error::request_context))
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        .with_state(state))
}
