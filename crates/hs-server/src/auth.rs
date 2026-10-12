//! Bearer token authentication.

use std::path::Path;

use axum::extract::{Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::Response;
use hs_model::ErrorCode;
use subtle::ConstantTimeEq;

use crate::AppState;
use crate::error::ApiError;

/// Minimum token length, as in mypowers.
const MIN_LENGTH: usize = 32;

/// The token clients present. `Debug` never shows it.
#[derive(Clone)]
pub struct ApiToken(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiTokenError {
    #[error("the API token must be at least {MIN_LENGTH} printable ASCII characters without spaces")]
    Format,
    #[error("could not read the API token file {path}: {reason}")]
    File { path: String, reason: String },
}

impl ApiToken {
    /// Parse a token, ignoring surrounding whitespace.
    pub fn parse(text: &str) -> Result<Self, ApiTokenError> {
        let text = text.trim();
        if text.len() >= MIN_LENGTH && text.bytes().all(|b| b.is_ascii_graphic()) {
            Ok(Self(text.to_owned()))
        } else {
            Err(ApiTokenError::Format)
        }
    }

    pub fn from_file(path: &Path) -> Result<Self, ApiTokenError> {
        let text = std::fs::read_to_string(path).map_err(|error| ApiTokenError::File {
            path: path.display().to_string(),
            reason: error.kind().to_string(),
        })?;
        Self::parse(&text)
    }

    fn matches(&self, candidate: &str) -> bool {
        self.0.as_bytes().ct_eq(candidate.as_bytes()).into()
    }
}

impl std::fmt::Debug for ApiToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiToken(<redacted>)")
    }
}

pub async fn require_bearer(State(state): State<AppState>, request: Request, next: Next) -> Result<Response, ApiError> {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim());
    match presented {
        Some(token) if state.api_token.matches(token) => Ok(next.run(request).await),
        _ => Err(ApiError::new(
            ErrorCode::Unauthorized,
            "A valid bearer token is required.",
        )),
    }
}
