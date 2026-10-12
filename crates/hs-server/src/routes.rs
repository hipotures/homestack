//! Route handlers.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::get;
use hs_model::health::Liveness;
use hs_model::pve::{NodeStatus, PveVersion};
use hs_model::{ErrorCode, NodeName};

use crate::AppState;
use crate::error::ApiError;

pub fn public() -> Router<AppState> {
    Router::new().route("/health/live", get(live))
}

pub fn v1() -> Router<AppState> {
    Router::new()
        .route("/v1/pve/version", get(pve_version))
        .route("/v1/pve/nodes/{node}/status", get(pve_node_status))
}

/// The HTTP server answers. Does not contact Proxmox.
#[utoipa::path(get, path = "/health/live", tag = "health", responses((status = 200, body = Liveness)))]
pub async fn live() -> Json<Liveness> {
    Json(Liveness::OK)
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
    Ok(Json(state.pve.version().await?))
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
    Ok(Json(state.pve.node_status(&node).await?))
}
