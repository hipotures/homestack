//! HomeStack server: an HTTP API in front of the Proxmox VE API.
//!
//! Every `/v1` route needs `Authorization: Bearer <token>`. Errors use the
//! document in [`hs_model::ErrorBody`]; every response carries
//! `x-request-id` and `Cache-Control: no-store`.

mod auth;
pub mod config_store;
mod error;
mod jobs;
pub mod lifecycle;
mod openapi;
pub mod operations;
pub mod precondition;
mod reads;
mod routes;
pub mod settings;

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::middleware;
use hs_pve::{PveClient, PveClientConfig, PveError};
use tower_http::set_header::SetResponseHeaderLayer;

pub use auth::{ApiToken, ApiTokenError};
use config_store::{ConfigStore, StoreError};
pub use openapi::openapi_json;
pub use operations::AppOptions;
use operations::OpsState;

/// Everything the server needs to run.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Token clients must present.
    pub api_token: ApiToken,
    /// How to reach Proxmox.
    pub pve: PveClientConfig,
    /// Where the uploaded `server.toml` is kept.
    pub config_file: PathBuf,
    /// The Proxmox pools of this instance.
    pub pools: Pools,
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

/// Why the server cannot start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Pve(#[from] PveError),
    #[error(transparent)]
    Config(#[from] StoreError),
}

#[derive(Clone)]
struct AppState {
    pve: Arc<PveClient>,
    api_token: Arc<ApiToken>,
    pve_token_id: Arc<str>,
    pools: Arc<Pools>,
    config: Arc<ConfigStore>,
    ops: Arc<OpsState>,
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
    let state = AppState {
        pve_token_id: Arc::from(config.pve.token.id()),
        pools: Arc::new(config.pools),
        pve: Arc::new(PveClient::new(config.pve)?),
        api_token: Arc::new(config.api_token),
        config: Arc::new(store),
        ops: Arc::new(OpsState::new(options)),
    };
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
