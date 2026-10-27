//! Route handlers.

use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use hs_core::capabilities::{evaluate, installation, probe_paths};
use hs_model::capabilities::Capabilities;
use hs_model::health::{Liveness, LivenessStatus};
use hs_model::pve::{NodeStatus, PveVersion};
use hs_model::server_config::{ConfigUpdated, ConfigVersion};
use hs_model::{ErrorCode, NodeName};

use crate::AppState;
use crate::config_store::{Precondition, ReplaceError};
use crate::error::ApiError;

/// Largest `server.toml` accepted by `PUT /v1/config`.
pub const MAX_CONFIG_BYTES: usize = 256 * 1024;

pub fn public() -> Router<AppState> {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route(crate::health::PROBE_PATH, get(heartbeat_probe))
        .route(hs_service_status::PATH, get(service_status))
}

/// This process's liveness and RSS; `pass` is the same verdict as
/// `/health/live`.
#[utoipa::path(get, path = "/.well-known/homestack/status", tag = "health", responses((status = 200, body = hs_service_status::Report)))]
pub async fn service_status(State(state): State<AppState>) -> Json<hs_service_status::Report> {
    let status = match state.heartbeat.snapshot().status {
        LivenessStatus::Pass => hs_service_status::Status::Pass,
        LivenessStatus::Fail => hs_service_status::Status::Fail,
    };
    Json(hs_service_status::report(status))
}

pub fn v1() -> Router<AppState> {
    Router::new()
        .route("/v1/pve/version", get(pve_version))
        .route("/v1/pve/nodes/{node}/status", get(pve_node_status))
        .route(
            "/v1/config",
            get(config_get)
                .put(config_put)
                .layer(DefaultBodyLimit::max(MAX_CONFIG_BYTES)),
        )
        .route("/v1/config/version", get(config_version))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/images/check", get(crate::image_updates::progress))
        .route("/v1/status", get(crate::reads::status))
        .route("/v1/workspaces/{target}", get(crate::reads::workspace))
        .route("/v1/storage", get(crate::reads::storage))
        .route("/v1/storage/overview", get(crate::reads::storage_overview))
        .route("/v1/orphans", get(crate::reads::orphans))
        .route("/v1/discovery", get(crate::reads::discovery))
        .route("/v1/networks", get(crate::reads::networks))
        .route("/v1/gold/readiness", get(crate::reads::gold_readiness))
        .route(
            "/v1/plans",
            post(crate::jobs::create_plan).layer(DefaultBodyLimit::max(crate::jobs::MAX_REQUEST_BYTES)),
        )
        .route("/v1/plans/{id}", get(crate::jobs::get_plan))
        .route(
            "/v1/plans/{id}/apply",
            post(crate::jobs::apply_plan).layer(DefaultBodyLimit::max(crate::jobs::MAX_REQUEST_BYTES)),
        )
        .route("/v1/jobs", get(crate::jobs::list_jobs))
        .route("/v1/jobs/{id}", get(crate::jobs::get_job))
        .route("/v1/jobs/{id}/events", get(crate::jobs::job_events))
        .merge(crate::secrets::routes())
}

/// The process answers and its serving-path heartbeat is fresh.
#[utoipa::path(get, path = "/health/live", tag = "health", responses(
    (status = 200, body = Liveness, content_type = "application/health+json"),
    (status = 503, body = Liveness, content_type = "application/health+json"),
))]
pub async fn live(State(state): State<AppState>) -> Response {
    health_response(&state)
}

/// The API can serve requests, including configuration during bootstrap.
/// Proxmox availability is reported by the operations that need it.
#[utoipa::path(get, path = "/health/ready", tag = "health", responses(
    (status = 200, body = Liveness, content_type = "application/health+json"),
    (status = 503, body = Liveness, content_type = "application/health+json"),
))]
pub async fn ready(State(state): State<AppState>) -> Response {
    health_response(&state)
}

fn health_response(state: &AppState) -> Response {
    let health = state.heartbeat.snapshot();
    let status = match health.status {
        LivenessStatus::Pass => StatusCode::OK,
        LivenessStatus::Fail => StatusCode::SERVICE_UNAVAILABLE,
    };
    (
        status,
        [(header::CONTENT_TYPE, "application/health+json")],
        Json(health),
    )
        .into_response()
}

async fn heartbeat_probe(State(state): State<AppState>) -> StatusCode {
    // The same in-memory configuration read used by /v1/config/version;
    // an unconfigured bootstrap API is still a functioning process.
    let _ = state.config.current();
    StatusCode::NO_CONTENT
}

/// Proxmox VE version, from `GET /version` on the first reachable endpoint.
#[utoipa::path(
    get,
    path = "/v1/pve/version",
    tag = "pve",
    security(("bearer" = [])),
    responses(
        (status = 200, body = PveVersion),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 502, body = hs_model::ErrorBody, description = "Proxmox rejected the request or answered unusably"),
        (status = 503, body = hs_model::ErrorBody, description = "No Proxmox endpoint is reachable"),
    )
)]
pub async fn pve_version(State(state): State<AppState>) -> Result<Json<PveVersion>, ApiError> {
    Ok(Json(state.pve()?.version().await?))
}

/// Version, kernel and uptime of one node, from `GET /nodes/{node}/status`.
#[utoipa::path(
    get,
    path = "/v1/pve/nodes/{node}/status",
    tag = "pve",
    security(("bearer" = [])),
    params(("node" = String, Path, description = "Proxmox node name")),
    responses(
        (status = 200, body = NodeStatus),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 422, body = hs_model::ErrorBody, description = "Invalid node name"),
        (status = 502, body = hs_model::ErrorBody, description = "Proxmox rejected the request or answered unusably"),
        (status = 503, body = hs_model::ErrorBody, description = "No Proxmox endpoint is reachable, or the node is not"),
    )
)]
pub async fn pve_node_status(
    State(state): State<AppState>,
    Path(node): Path<String>,
) -> Result<Json<NodeStatus>, ApiError> {
    let node = NodeName::parse(&node).map_err(|_| ApiError::new(ErrorCode::ValidationFailed, "Invalid node name."))?;
    Ok(Json(state.pve()?.node_status(&node).await?))
}

/// The stored `server.toml`, as uploaded.
#[utoipa::path(
    get,
    path = "/v1/config",
    tag = "config",
    security(("bearer" = [])),
    responses(
        (status = 200, content_type = "application/toml", body = String,
            headers(("etag" = String, description = "Quoted configuration version"))),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 404, body = hs_model::ErrorBody, description = "No configuration has been uploaded"),
    )
)]
pub async fn config_get(State(state): State<AppState>) -> Result<Response, ApiError> {
    let current = state.config.current().ok_or(ApiError::new(
        ErrorCode::NotFound,
        "No configuration has been uploaded.",
    ))?;
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/toml; charset=utf-8"),
            ),
            (header::ETAG, etag(&current.version)?),
        ],
        current.text.clone(),
    )
        .into_response())
}

/// Replace the configuration.
///
/// The body is `server.toml`. Send `If-None-Match: *` for the first upload
/// and `If-Match: "<config_version>"` (or `*`) afterwards. The text is
/// validated like Python's `config.load_config` and stored atomically; the
/// previous file is kept as a backup. Uploading the stored text again
/// changes nothing.
#[utoipa::path(
    put,
    path = "/v1/config",
    tag = "config",
    security(("bearer" = [])),
    params(
        ("if-match" = Option<String>, Header, description = "Quoted current version, or `*`"),
        ("if-none-match" = Option<String>, Header, description = "`*` for the first upload"),
    ),
    request_body(content = String, content_type = "application/toml", description = "server.toml"),
    responses(
        (status = 200, body = ConfigUpdated, headers(("etag" = String, description = "Quoted new version"))),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 412, body = hs_model::ErrorBody, description = "The stored version is not the one in If-Match, or a configuration exists despite If-None-Match"),
        (status = 413, body = hs_model::ErrorBody, description = "Larger than 256 KiB"),
        (status = 422, body = hs_model::ErrorBody, description = "Invalid configuration; `error.field` names the problem"),
        (status = 428, body = hs_model::ErrorBody, description = "Neither If-Match nor If-None-Match: * was sent"),
    )
)]
pub async fn config_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let precondition = precondition(&headers)?;
    let body = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(ErrorCode::PayloadTooLarge, "The configuration is larger than 256 KiB.")
        } else {
            ApiError::new(ErrorCode::ValidationFailed, "The request body could not be read.")
        }
    })?;
    let text = String::from_utf8(body.to_vec()).map_err(|_| {
        ApiError::config_invalid(hs_model::config::ConfigError::new(
            "toml",
            "The configuration must be UTF-8 text",
        ))
    })?;
    let store = state.config.clone();
    let replaced = tokio::task::spawn_blocking(move || store.replace(text, &precondition))
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal, "The configuration could not be stored."))?
        .map_err(|error| match error {
            ReplaceError::Precondition => ApiError::new(
                ErrorCode::PreconditionFailed,
                "The stored configuration is not the version the request expects.",
            ),
            ReplaceError::Invalid(error) => ApiError::config_invalid(error),
            ReplaceError::Io(error) => {
                tracing::error!(%error, "could not store the configuration");
                ApiError::new(ErrorCode::Internal, "The configuration could not be stored.")
            }
        })?;
    if replaced.changed {
        tracing::info!(config_version = replaced.version, "configuration replaced");
    }
    Ok((
        [(header::ETAG, etag(&replaced.version)?)],
        Json(ConfigUpdated {
            config_version: replaced.version,
            changed: replaced.changed,
        }),
    )
        .into_response())
}

/// Version of the stored configuration, to compare instances.
#[utoipa::path(
    get,
    path = "/v1/config/version",
    tag = "config",
    security(("bearer" = [])),
    responses(
        (status = 200, body = ConfigVersion),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
    )
)]
pub async fn config_version(State(state): State<AppState>) -> Json<ConfigVersion> {
    Json(ConfigVersion {
        config_version: state.config.current().map(|current| current.version.clone()),
    })
}

/// What the server's Proxmox token can do with the stored configuration.
///
/// Asks Proxmox (`GET /access/permissions`) for the token's privileges on
/// every ACL path the operations need, then lists per operation what is
/// required and what is missing. Changes need privileges on the instance's
/// pool (`HOMESTACK_PVE_POOL`). `installation` says which of the objects
/// `deploy/pve/homestack-pve.sh install` makes the token can see.
#[utoipa::path(
    get,
    path = "/v1/capabilities",
    tag = "config",
    security(("bearer" = [])),
    responses(
        (status = 200, body = Capabilities),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 409, body = hs_model::ErrorBody, description = "No configuration has been uploaded"),
        (status = 502, body = hs_model::ErrorBody, description = "Proxmox rejected the request or answered unusably"),
        (status = 503, body = hs_model::ErrorBody, description = "No Proxmox endpoint is reachable"),
    )
)]
pub async fn capabilities(State(state): State<AppState>) -> Result<Json<Capabilities>, ApiError> {
    let current = state.config.current().ok_or_else(ApiError::config_missing)?;
    let mut granted: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let pools = &state.pools;
    for path in probe_paths(&current.config, &pools.workspaces) {
        let privileges = state.pve()?.permissions(&path).await?;
        granted.insert(path, privileges);
    }
    let operations = evaluate(&current.config, &pools.workspaces, |path, privilege| {
        granted
            .get(path)
            .is_some_and(|privileges| privileges.contains(privilege))
    });
    let installation = installation(state.pve()?.as_ref(), &current.config, &pools.workspaces, &pools.gold).await;
    Ok(Json(Capabilities {
        token_id: state.pve.token_id().unwrap_or_default(),
        config_version: current.version.clone(),
        operations,
        installation,
    }))
}

/// `If-Match` (a quoted version or `*`) or `If-None-Match: *`.
fn precondition(headers: &HeaderMap) -> Result<Precondition, ApiError> {
    let text = |name: header::HeaderName| headers.get(name).map(|value| value.to_str().map(str::trim));
    if let Some(value) = text(header::IF_MATCH) {
        return Ok(match value {
            Ok("*") => Precondition::Exists(None),
            Ok(tag) => Precondition::Exists(Some(tag.trim_matches('"').to_owned())),
            Err(_) => Precondition::Exists(Some(String::new())),
        });
    }
    match text(header::IF_NONE_MATCH) {
        Some(Ok("*")) => Ok(Precondition::Absent),
        _ => Err(ApiError::new(
            ErrorCode::PreconditionRequired,
            "Send If-Match with the current configuration version, or If-None-Match: * for the first upload.",
        )),
    }
}

fn etag(version: &str) -> Result<HeaderValue, ApiError> {
    HeaderValue::from_str(&format!("\"{version}\""))
        .map_err(|_| ApiError::new(ErrorCode::Internal, "The configuration version is not a valid header."))
}
