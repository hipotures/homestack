//! Read routes (phase 2): status, workspaces, storage, detached volumes,
//! discovery, network profiles and Gold readiness. The logic is
//! `hs_core::status`, `hs_core::discovery` and `hs_core::gold`; these
//! handlers add the configuration, the error mapping and what only the
//! server can know: whether its token may list VM volumes at all.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use hs_core::CoreError;
use hs_core::discovery::{discovery as read_discovery, workspace_networks};
use hs_core::gold::gold_readiness as read_gold_readiness;
use hs_core::inventory::{cluster_node_statuses, cluster_vm_resources, storage_ids_for_node};
use hs_core::status::{
    Inventory, global_status, orphaned_volumes, resolve_workspace_target, storage_capacities, workspace_status,
};
use hs_model::ErrorCode;
use hs_model::config::Config;
use hs_model::status::{
    Discovery, GlobalStatus, GoldReadiness, LayoutCapacity, NetworkCandidate, NodeState, Orphans, WorkspaceStatus,
};
use serde::Deserialize;

use crate::AppState;
use crate::config_store::Stored;
use crate::error::ApiError;

fn configuration(state: &AppState) -> Result<Arc<Stored>, ApiError> {
    state.config.current().ok_or_else(ApiError::config_missing)
}

/// Fixed messages: `hs-core` errors may repeat names from the request.
fn core_error(error: CoreError, not_found: &'static str) -> ApiError {
    match error {
        CoreError::Pve(error) => error.into(),
        CoreError::NotFound(_) | CoreError::Refused(_) | CoreError::OutsidePool(_) => {
            ApiError::new(ErrorCode::NotFound, not_found)
        }
        CoreError::Invalid(_) => ApiError::new(ErrorCode::ValidationFailed, "Invalid workspace target."),
        CoreError::Ambiguous(_) => ApiError::new(
            ErrorCode::ValidationFailed,
            "The workspace name matches several VMs; use the VMID.",
        ),
        CoreError::Unusable(detail) | CoreError::Failed(detail) => {
            tracing::warn!(detail, "unusable Proxmox data");
            ApiError::new(ErrorCode::PveError, "Proxmox returned data HomeStack cannot use.")
        }
    }
}

/// Storages whose VM volumes the server's token cannot list: Proxmox shows
/// a VM volume only with `Datastore.Allocate` on the storage, or
/// `VM.Config.Disk` on the VM (`PVE::Storage::check_volume_access`), and a
/// detached volume's VM may be gone, so only `/vms` covers it.
async fn listing_gaps(state: &AppState, cfg: &Config, nodes: &[NodeState]) -> Result<Vec<String>, ApiError> {
    let mut cache: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut privileges = async |path: String| -> Result<BTreeSet<String>, ApiError> {
        if let Some(known) = cache.get(&path) {
            return Ok(known.clone());
        }
        let found = state.pve.permissions(&path).await?;
        cache.insert(path, found.clone());
        Ok(found)
    };
    let mut gaps = Vec::new();
    for node in nodes.iter().filter(|n| n.online) {
        for storage in storage_ids_for_node(cfg, &node.node) {
            if privileges(format!("/storage/{storage}"))
                .await?
                .contains("Datastore.Allocate")
                || privileges("/vms".to_owned()).await?.contains("VM.Config.Disk")
            {
                continue;
            }
            gaps.push(format!(
                "The server's Proxmox token cannot list the volumes of VMs outside pool {}, \
                 deleted VMs included, on {storage} ({}) (decision 10): a complete scan \
                 needs VM.Config.Disk on /vms or Datastore.Allocate on /storage/{storage}",
                state.pools.workspaces, node.node
            ));
        }
    }
    Ok(gaps)
}

/// Global status, as `homestack status --json`.
#[utoipa::path(
    get,
    path = "/v1/status",
    tag = "status",
    security(("bearer" = [])),
    responses(
        (status = 200, body = GlobalStatus),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 409, body = hs_model::ErrorBody, description = "No configuration has been uploaded"),
        (status = 502, body = hs_model::ErrorBody, description = "Proxmox rejected the request or answered unusably"),
        (status = 503, body = hs_model::ErrorBody, description = "No Proxmox endpoint is reachable"),
    )
)]
pub async fn status(State(state): State<AppState>) -> Result<Json<GlobalStatus>, ApiError> {
    let stored = configuration(&state)?;
    let mut status = global_status(state.pve.as_ref(), &stored.config)
        .await
        .map_err(|e| core_error(e, "No such resource."))?;
    let gaps = listing_gaps(&state, &stored.config, &status.summary.nodes).await?;
    if !gaps.is_empty() {
        status.summary.orphan_scan_complete = false;
        status.warnings.extend(
            gaps.iter()
                .map(|gap| format!("HomeStack orphan scan incomplete: {gap}")),
        );
    }
    Ok(Json(status))
}

/// One VM by VMID or workspace name, as `homestack status TARGET --json`.
#[utoipa::path(
    get,
    path = "/v1/workspaces/{target}",
    tag = "status",
    security(("bearer" = [])),
    params(("target" = String, Path, description = "VMID, or the name of a VM tagged homestack-ws or homestack-alien")),
    responses(
        (status = 200, body = WorkspaceStatus),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 404, body = hs_model::ErrorBody, description = "No such VM or workspace"),
        (status = 409, body = hs_model::ErrorBody, description = "No configuration has been uploaded"),
        (status = 422, body = hs_model::ErrorBody, description = "Invalid or ambiguous target"),
        (status = 502, body = hs_model::ErrorBody, description = "Proxmox rejected the request or answered unusably"),
        (status = 503, body = hs_model::ErrorBody, description = "No Proxmox endpoint is reachable"),
    )
)]
pub async fn workspace(
    State(state): State<AppState>,
    Path(target): Path<String>,
) -> Result<Json<WorkspaceStatus>, ApiError> {
    let stored = configuration(&state)?;
    let pve = state.pve.as_ref();
    let not_found = "No such workspace.";
    let vmid = resolve_workspace_target(pve, &stored.config, &target)
        .await
        .map_err(|e| core_error(e, not_found))?;
    workspace_status(pve, &stored.config, vmid)
        .await
        .map_err(|e| core_error(e, not_found))?
        .map(Json)
        .ok_or(ApiError::new(ErrorCode::NotFound, not_found))
}

/// Capacity of every storage of every node's `homestack-storage-<N>` layout.
#[utoipa::path(
    get,
    path = "/v1/storage",
    tag = "status",
    security(("bearer" = [])),
    responses(
        (status = 200, body = Vec<LayoutCapacity>),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 409, body = hs_model::ErrorBody, description = "No configuration has been uploaded"),
        (status = 502, body = hs_model::ErrorBody, description = "Proxmox rejected the request or answered unusably"),
        (status = 503, body = hs_model::ErrorBody, description = "No Proxmox endpoint is reachable"),
    )
)]
pub async fn storage(State(state): State<AppState>) -> Result<Json<Vec<LayoutCapacity>>, ApiError> {
    let stored = configuration(&state)?;
    let pve = state.pve.as_ref();
    let nodes = cluster_node_statuses(pve)
        .await
        .map_err(|e| core_error(e, "No such resource."))?;
    let (layouts, warnings) = storage_capacities(pve, &stored.config, &nodes).await;
    for warning in warnings {
        tracing::warn!(warning, "storage capacity");
    }
    Ok(Json(layouts))
}

/// HomeStack volumes no VM uses.
#[utoipa::path(
    get,
    path = "/v1/orphans",
    tag = "status",
    security(("bearer" = [])),
    responses(
        (status = 200, body = Orphans),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 409, body = hs_model::ErrorBody, description = "No configuration has been uploaded"),
        (status = 502, body = hs_model::ErrorBody, description = "Proxmox rejected the request or answered unusably"),
        (status = 503, body = hs_model::ErrorBody, description = "No Proxmox endpoint is reachable"),
    )
)]
pub async fn orphans(State(state): State<AppState>) -> Result<Json<Orphans>, ApiError> {
    let stored = configuration(&state)?;
    let pve = state.pve.as_ref();
    let inventory = Inventory::read(pve, &stored.config)
        .await
        .map_err(|e| core_error(e, "No such resource."))?;
    let (volumes, mut warnings, complete) =
        orphaned_volumes(pve, &stored.config, &inventory.attached_volumes(), &inventory.nodes).await;
    let gaps = listing_gaps(&state, &stored.config, &inventory.nodes).await?;
    let complete = complete && gaps.is_empty();
    warnings.extend(gaps);
    Ok(Json(Orphans {
        volumes,
        complete,
        warnings,
    }))
}

/// Nodes, Gold candidates and storage definitions; needs no configuration.
#[utoipa::path(
    get,
    path = "/v1/discovery",
    tag = "status",
    security(("bearer" = [])),
    responses(
        (status = 200, body = Discovery),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 502, body = hs_model::ErrorBody, description = "Proxmox rejected the request or answered unusably"),
        (status = 503, body = hs_model::ErrorBody, description = "No Proxmox endpoint is reachable"),
    )
)]
pub async fn discovery(State(state): State<AppState>) -> Result<Json<Discovery>, ApiError> {
    read_discovery(state.pve.as_ref())
        .await
        .map(Json)
        .map_err(|e| core_error(e, "No such resource."))
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct GoldQuery {
    /// Gold VMID; defaults to `[node] gold_vmid` of the configuration.
    #[serde(default)]
    gold: Option<String>,
    /// Same as `gold`.
    #[serde(default)]
    vmid: Option<String>,
}

/// The Gold VMID from the query or the configuration, and its node.
async fn gold_vm(
    state: &AppState,
    query: &GoldQuery,
) -> Result<(u32, String, Vec<hs_core::inventory::StatusResource>), ApiError> {
    let vmid = match query.gold.as_ref().or(query.vmid.as_ref()) {
        Some(text) => text
            .parse::<u32>()
            .map_err(|_| ApiError::new(ErrorCode::ValidationFailed, "Invalid VMID."))?,
        None => configuration(state)?.config.gold_vmid,
    };
    let resources = cluster_vm_resources(state.pve.as_ref())
        .await
        .map_err(|e| core_error(e, "No such resource."))?;
    let node = resources
        .iter()
        .find(|r| r.vmid == vmid)
        .map(|r| r.node.clone())
        .filter(|node| !node.is_empty())
        .ok_or(ApiError::new(ErrorCode::NotFound, "No such VM."))?;
    Ok((vmid, node, resources))
}

/// Network profiles a workspace could use, for the Gold VM's bridge.
#[utoipa::path(
    get,
    path = "/v1/networks",
    tag = "status",
    security(("bearer" = [])),
    params(GoldQuery),
    responses(
        (status = 200, body = Vec<NetworkCandidate>),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 404, body = hs_model::ErrorBody, description = "No such Gold VM"),
        (status = 409, body = hs_model::ErrorBody, description = "No Gold given and no configuration uploaded"),
        (status = 422, body = hs_model::ErrorBody, description = "Invalid VMID"),
        (status = 502, body = hs_model::ErrorBody, description = "Proxmox rejected the request or answered unusably"),
        (status = 503, body = hs_model::ErrorBody, description = "No Proxmox endpoint is reachable"),
    )
)]
pub async fn networks(
    State(state): State<AppState>,
    Query(query): Query<GoldQuery>,
) -> Result<Json<Vec<NetworkCandidate>>, ApiError> {
    let (vmid, node, resources) = gold_vm(&state, &query).await?;
    workspace_networks(state.pve.as_ref(), &resources, vmid, &node)
        .await
        .map(Json)
        .map_err(|e| core_error(e, "No such VM."))
}

/// Gold readiness: the Proxmox part and, when Gold runs, the guest contract
/// (commands through the guest agent; skipped without
/// `VM.GuestAgent.Unrestricted` on Gold). The account checks use `[user]` of
/// the configuration, or the installer's defaults (`user`, 1000:1000) before
/// one is uploaded. A stopped Gold is not started.
#[utoipa::path(
    get,
    path = "/v1/gold/readiness",
    tag = "status",
    security(("bearer" = [])),
    params(GoldQuery),
    responses(
        (status = 200, body = GoldReadiness),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 404, body = hs_model::ErrorBody, description = "No such VM"),
        (status = 409, body = hs_model::ErrorBody, description = "No VMID given and no configuration uploaded"),
        (status = 422, body = hs_model::ErrorBody, description = "Invalid VMID"),
        (status = 502, body = hs_model::ErrorBody, description = "Proxmox rejected the request or answered unusably"),
        (status = 503, body = hs_model::ErrorBody, description = "No Proxmox endpoint is reachable"),
    )
)]
pub async fn gold_readiness(
    State(state): State<AppState>,
    Query(query): Query<GoldQuery>,
) -> Result<Json<GoldReadiness>, ApiError> {
    let (vmid, node, _) = gold_vm(&state, &query).await?;
    let (root_disk, user) = state.config.current().map_or_else(
        || {
            let user = hs_model::config::User {
                name: "user".to_owned(),
                uid: 1000,
                gid: 1000,
            };
            ("scsi0".to_owned(), user)
        },
        |stored| (stored.config.root_disk.clone(), stored.config.user.clone()),
    );
    read_gold_readiness(state.pve.as_ref(), &root_disk, &user, &node, vmid)
        .await
        .map(Json)
        .map_err(|e| core_error(e, "No such VM."))
}
