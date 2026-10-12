//! HomeStack server: an HTTP API in front of the Proxmox VE API.
//!
//! Every `/v1` route needs `Authorization: Bearer <token>`. Errors use the
//! document in [`hs_model::ErrorBody`]; every response carries
//! `x-request-id` and `Cache-Control: no-store`.

mod auth;
pub mod config_store;
mod error;
mod openapi;
pub mod precondition;
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

/// Everything the server needs to run.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Token clients must present.
    pub api_token: ApiToken,
    /// How to reach Proxmox.
    pub pve: PveClientConfig,
    /// Where the uploaded `server.toml` is kept.
    pub config_file: PathBuf,
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
    config: Arc<ConfigStore>,
}

/// Build the HTTP application. Loads the stored configuration, if any.
pub fn app(config: ServerConfig) -> Result<Router, StartError> {
    let store = ConfigStore::open(config.config_file)?;
    if let Some(current) = store.current() {
        tracing::info!(path = %store.path().display(), config_version = current.version, "configuration loaded");
    } else {
        tracing::warn!(path = %store.path().display(), "no configuration yet; upload one with PUT /v1/config");
    }
    let state = AppState {
        pve_token_id: Arc::from(config.pve.token.id()),
        pve: Arc::new(PveClient::new(config.pve)?),
        api_token: Arc::new(config.api_token),
        config: Arc::new(store),
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
