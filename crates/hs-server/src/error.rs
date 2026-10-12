//! Error responses and the per-request context (request ID, access log).
//!
//! Handlers and middleware return [`ApiError`]. It becomes a response with
//! the right status and a marker; [`request_context`] then writes the final
//! error document, which needs the request ID.

use std::borrow::Cow;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{MatchedPath, Request};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hs_model::config::ConfigError;
use hs_model::{ErrorBody, ErrorCode};
use hs_pve::PveError;

#[derive(Debug, Clone)]
pub struct ApiError {
    code: ErrorCode,
    message: Cow<'static, str>,
    field: Option<String>,
}

impl ApiError {
    pub const fn new(code: ErrorCode, message: &'static str) -> Self {
        Self {
            code,
            message: Cow::Borrowed(message),
            field: None,
        }
    }

    /// `config_invalid` naming the field. The message of a [`ConfigError`]
    /// never contains values from the document.
    pub fn config_invalid(error: ConfigError) -> Self {
        Self {
            code: ErrorCode::ConfigInvalid,
            message: Cow::Owned(error.message),
            field: Some(error.field),
        }
    }

    pub const fn config_missing() -> Self {
        Self::new(
            ErrorCode::ConfigMissing,
            "The server has no configuration yet; upload one with PUT /v1/config.",
        )
    }
}

const fn status(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
        ErrorCode::ValidationFailed | ErrorCode::ConfigInvalid => StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        ErrorCode::PreconditionRequired => StatusCode::PRECONDITION_REQUIRED,
        ErrorCode::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
        ErrorCode::ConfigMissing
        | ErrorCode::ConfigVersionMismatch
        | ErrorCode::PlanChanged
        | ErrorCode::PlanApplied
        | ErrorCode::Busy => StatusCode::CONFLICT,
        ErrorCode::PveUnavailable | ErrorCode::PveNodeUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::PveAuthFailed | ErrorCode::PvePermissionDenied | ErrorCode::PveError => StatusCode::BAD_GATEWAY,
        ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = status(self.code).into_response();
        if self.code == ErrorCode::Unauthorized {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response.extensions_mut().insert(self);
        response
    }
}

/// The API error code of a Proxmox failure.
pub const fn pve_error_code(error: &PveError) -> ErrorCode {
    match error {
        PveError::Unreachable(_) => ErrorCode::PveUnavailable,
        PveError::Unauthorized => ErrorCode::PveAuthFailed,
        PveError::Forbidden { .. } => ErrorCode::PvePermissionDenied,
        PveError::NodeUnreachable { .. } => ErrorCode::PveNodeUnavailable,
        PveError::Api { .. } | PveError::Response(_) => ErrorCode::PveError,
        PveError::Config(_) => ErrorCode::Internal,
    }
}

impl From<PveError> for ApiError {
    /// Details stay in the server log (the client already logged each failed
    /// endpoint); clients get a fixed message.
    fn from(error: PveError) -> Self {
        tracing::warn!(%error, "Proxmox request failed");
        match error {
            PveError::Unreachable(_) => Self::new(ErrorCode::PveUnavailable, "No Proxmox API endpoint is reachable."),
            PveError::Unauthorized => Self::new(ErrorCode::PveAuthFailed, "Proxmox rejected the server's API token."),
            PveError::Forbidden { .. } => Self::new(
                ErrorCode::PvePermissionDenied,
                "The server's Proxmox API token lacks a required privilege.",
            ),
            PveError::NodeUnreachable { .. } => Self::new(
                ErrorCode::PveNodeUnavailable,
                "Proxmox could not reach the requested node.",
            ),
            PveError::Api { .. } | PveError::Response(_) => Self::new(
                ErrorCode::PveError,
                "Proxmox returned an error or an unusable response.",
            ),
            PveError::Config(_) => Self::new(ErrorCode::Internal, "The server is misconfigured."),
        }
    }
}

pub async fn not_found() -> ApiError {
    ApiError::new(ErrorCode::NotFound, "No such resource.")
}

pub async fn method_not_allowed() -> ApiError {
    ApiError::new(
        ErrorCode::MethodNotAllowed,
        "This method is not allowed on this resource.",
    )
}

/// Give each request an ID, turn [`ApiError`] markers into error documents,
/// and log one line per request. Only the route template is logged, never the
/// raw path or headers.
pub async fn request_context(request: Request, next: Next) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    let method = request.method().clone();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("<unmatched>", MatchedPath::as_str)
        .to_owned();
    let started = Instant::now();

    let mut response = next.run(request).await;
    if let Some(error) = response.extensions_mut().remove::<ApiError>() {
        let body = ErrorBody::new(error.code, error.message, request_id.clone()).with_field(error.field);
        match serde_json::to_vec(&body) {
            Ok(json) => {
                *response.body_mut() = Body::from(json);
                response
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            }
            Err(serialize_error) => tracing::error!(%serialize_error, "could not serialize an error body"),
        }
    }
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    tracing::info!(
        request_id,
        %method,
        route,
        status = response.status().as_u16(),
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "request"
    );
    response
}
