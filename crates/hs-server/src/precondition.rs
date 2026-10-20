//! The configuration version a state-changing request expects (mapping 8.2).
//!
//! Every request that changes state names the configuration it was planned
//! against in `HomeStack-Config-Version`. An instance with another version
//! refuses it (`409 config_version_mismatch`), so no instance acts on a
//! configuration the client did not see. Handlers take [`ExpectedConfig`]
//! as an extractor; the first state-changing routes arrive in phase 3.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::request::Parts;
use hs_model::ErrorCode;

use crate::AppState;
use crate::config_store::Stored;
use crate::error::ApiError;

/// Request header with the expected configuration version.
pub const CONFIG_VERSION_HEADER: &str = "homestack-config-version";

/// The stored configuration, once the request's expected version matches.
#[derive(Debug)]
pub struct ExpectedConfig(pub Arc<Stored>);

impl FromRequestParts<AppState> for ExpectedConfig {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        expected_config(&parts.headers, state).map(Self)
    }
}

/// The stored configuration when `headers` name its version.
pub(crate) fn expected_config(headers: &HeaderMap, state: &AppState) -> Result<Arc<Stored>, ApiError> {
    let current = state.config.current().ok_or_else(ApiError::config_missing)?;
    let expected = headers
        .get(CONFIG_VERSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .ok_or_else(|| {
            ApiError::new(
                ErrorCode::PreconditionRequired,
                "State-changing requests must send HomeStack-Config-Version.",
            )
        })?;
    if expected != current.version {
        return Err(ApiError::new(
            ErrorCode::ConfigVersionMismatch,
            "This server has another configuration version than the request expects.",
        ));
    }
    Ok(current)
}
