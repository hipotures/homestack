//! HomeStack server: an HTTP API in front of the Proxmox VE API.
//!
//! Every `/v1` route needs `Authorization: Bearer <token>`. Errors use the
//! document in [`hs_model::ErrorBody`]; every response carries
//! `x-request-id` and `Cache-Control: no-store`.

mod auth;
mod error;
mod openapi;
mod routes;
pub mod settings;

use std::sync::Arc;

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::middleware;
use hs_pve::{PveClient, PveClientConfig, PveError};
use tower_http::set_header::SetResponseHeaderLayer;

pub use auth::{ApiToken, ApiTokenError};
pub use openapi::openapi_json;

/// Everything the server needs to run.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Token clients must present.
    pub api_token: ApiToken,
    /// How to reach Proxmox.
    pub pve: PveClientConfig,
}

#[derive(Clone)]
struct AppState {
    pve: Arc<PveClient>,
    api_token: Arc<ApiToken>,
}

/// Build the HTTP application.
pub fn app(config: ServerConfig) -> Result<Router, PveError> {
    let state = AppState {
        pve: Arc::new(PveClient::new(config.pve)?),
        api_token: Arc::new(config.api_token),
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
