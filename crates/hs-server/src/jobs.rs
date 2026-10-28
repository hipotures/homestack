//! Routes of the operation model: plans, jobs and the job stream.

use std::sync::Arc;
use std::time::SystemTime;

use axum::Json;
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use hs_core::CoreError;
use hs_model::ErrorCode;
use hs_model::SCHEMA_VERSION;
use hs_model::jobs::{ApplyRequest, Job, Plan, PlanRequest, StreamMessage, StreamMessageType};
use serde::de::DeserializeOwned;
use tokio::sync::watch;

use crate::AppState;
use crate::error::ApiError;
use crate::operations::{ApplyRefusal, OpContext, OpFailure, timestamp};

/// Largest plan or apply request accepted.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

fn context(state: &AppState) -> Result<OpContext, ApiError> {
    Ok(OpContext {
        pve: state.pve()?,
        config: state.config.current(),
        pools: Arc::clone(&state.pools),
        secrets: hs_model::jobs::ApplySecrets::default(),
    })
}

/// A JSON body with unknown fields refused; nothing of it is echoed.
fn json_body<T: DeserializeOwned>(headers: &HeaderMap, body: &Bytes) -> Result<T, ApiError> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|kind| kind.trim() == "application/json")
        });
    if !json {
        return Err(ApiError::new(
            ErrorCode::ValidationFailed,
            "The request body must be JSON (Content-Type: application/json).",
        ));
    }
    serde_json::from_slice(body).map_err(|_| ApiError::new(ErrorCode::ValidationFailed, "Invalid request body."))
}

/// A failure while planning, as an API error.
fn plan_error(failure: OpFailure) -> ApiError {
    match failure {
        OpFailure::Invalid(message) => ApiError::new(ErrorCode::ValidationFailed, message),
        OpFailure::NotFound(message) => ApiError::new(ErrorCode::NotFound, message),
        OpFailure::Core(CoreError::Pve(error)) => error.into(),
        // The lifecycle operations compose these from validated values.
        OpFailure::Core(CoreError::Invalid(message)) => ApiError::with_message(ErrorCode::ValidationFailed, message),
        OpFailure::Core(CoreError::NotFound(message)) => ApiError::with_message(ErrorCode::NotFound, message),
        OpFailure::Core(CoreError::OutsidePool(message)) => ApiError::with_message(ErrorCode::OutsidePool, message),
        OpFailure::Core(CoreError::Busy(message)) => ApiError::with_message(ErrorCode::Busy, message),
        OpFailure::Core(CoreError::Refused(message) | CoreError::Failed(message)) => {
            ApiError::with_message(ErrorCode::OperationRefused, message)
        }
        OpFailure::Core(error) => {
            tracing::warn!(%error, "planning failed");
            ApiError::new(
                ErrorCode::ValidationFailed,
                "The operation cannot be planned in this state.",
            )
        }
    }
}

fn valid_id(id: &str) -> Result<(), ApiError> {
    uuid::Uuid::parse_str(id)
        .map(drop)
        .map_err(|_| ApiError::new(ErrorCode::NotFound, "No such resource."))
}

/// Plan an operation. Nothing is changed; the plan can be applied until
/// `expires_at`.
#[utoipa::path(
    post,
    path = "/v1/plans",
    tag = "operations",
    security(("bearer" = [])),
    request_body = PlanRequest,
    responses(
        (status = 201, body = Plan),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 404, body = hs_model::ErrorBody, description = "A VM named by the parameters does not exist"),
        (status = 409, body = hs_model::ErrorBody, description = "`outside_pool`: the VM is not in the instance's pool; `operation_refused`: the cluster's state does not allow the operation"),
        (status = 422, body = hs_model::ErrorBody, description = "Unknown operation or invalid parameters"),
        (status = 502, body = hs_model::ErrorBody, description = "Proxmox rejected the request or answered unusably"),
        (status = 503, body = hs_model::ErrorBody, description = "No Proxmox endpoint is reachable"),
    )
)]
pub async fn create_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Plan>), ApiError> {
    let request: PlanRequest = json_body(&headers, &body)?;
    match state
        .ops
        .plan(&context(&state)?, &request.operation, request.parameters)
        .await
    {
        None => Err(ApiError::new(ErrorCode::ValidationFailed, "Unknown operation.")),
        Some(Err(failure)) => Err(plan_error(failure)),
        Some(Ok(plan)) => Ok((StatusCode::CREATED, Json(plan))),
    }
}

/// A plan made earlier, until it expires.
#[utoipa::path(
    get,
    path = "/v1/plans/{id}",
    tag = "operations",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Plan ID")),
    responses(
        (status = 200, body = Plan),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 404, body = hs_model::ErrorBody, description = "No such plan, or it expired"),
    )
)]
pub async fn get_plan(State(state): State<AppState>, Path(id): Path<String>) -> Result<Json<Plan>, ApiError> {
    valid_id(&id)?;
    state
        .ops
        .get_plan(&id)
        .map(Json)
        .ok_or(ApiError::new(ErrorCode::NotFound, "No such plan; plans expire."))
}

/// Apply a plan: plan again, check that nothing changed, and start a job.
/// The job runs on the server whatever happens to this request.
#[utoipa::path(
    post,
    path = "/v1/plans/{id}/apply",
    tag = "operations",
    security(("bearer" = [])),
    params(
        ("id" = String, Path, description = "Plan ID"),
        ("HomeStack-Config-Version" = Option<String>, Header, description = "The configuration version the plan was made with; required once a configuration is stored"),
    ),
    request_body = ApplyRequest,
    responses(
        (status = 202, body = Job, description = "The job was started; follow it at `Location`"),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 404, body = hs_model::ErrorBody, description = "No such plan, or it expired"),
        (status = 409, body = hs_model::ErrorBody, description = "`plan_changed`, `plan_applied`, `busy`, `config_version_mismatch`, `outside_pool` or `operation_refused`"),
        (status = 428, body = hs_model::ErrorBody, description = "`HomeStack-Config-Version` is missing"),
        (status = 422, body = hs_model::ErrorBody, description = "Invalid request body; `secret_required`: the request lacks a secret the plan's `summary.secrets_required` names, or sends one it does not"),
        (status = 502, body = hs_model::ErrorBody, description = "Proxmox rejected the request or answered unusably"),
        (status = 503, body = hs_model::ErrorBody, description = "No Proxmox endpoint is reachable"),
    )
)]
pub async fn apply_plan(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    valid_id(&id)?;
    let request: ApplyRequest = json_body(&headers, &body)?;
    if state.config.current().is_some() {
        crate::precondition::expected_config(&headers, &state)?;
    }
    let mut ctx = context(&state)?;
    ctx.secrets = request.secrets.unwrap_or_default();
    let job = state
        .ops
        .apply(ctx, &id, &request.digest)
        .await
        .map_err(|refusal| match refusal {
            ApplyRefusal::Unknown => ApiError::new(ErrorCode::NotFound, "No such plan; plans expire."),
            ApplyRefusal::Changed => ApiError::new(
                ErrorCode::PlanChanged,
                "The cluster changed since the plan was made, or the digest differs; make a new plan.",
            ),
            ApplyRefusal::ConfigChanged => ApiError::new(
                ErrorCode::ConfigVersionMismatch,
                "The configuration changed since the plan was made; make a new plan.",
            ),
            ApplyRefusal::Applied => ApiError::new(ErrorCode::PlanApplied, "The plan was already applied."),
            ApplyRefusal::Busy(None) => ApiError::new(ErrorCode::Busy, "Another job is working on a VM of this plan."),
            ApplyRefusal::Busy(Some(message)) => ApiError::with_message(ErrorCode::Busy, message),
            ApplyRefusal::Secrets(message) => ApiError::new(ErrorCode::SecretRequired, message),
            ApplyRefusal::Failure(failure) => plan_error(failure),
        })?;
    let location = format!("/v1/jobs/{}", job.id);
    Ok((StatusCode::ACCEPTED, [(header::LOCATION, location)], Json(job)).into_response())
}

/// Jobs kept by this server instance, newest first.
#[utoipa::path(
    get,
    path = "/v1/jobs",
    tag = "operations",
    security(("bearer" = [])),
    responses(
        (status = 200, body = Vec<Job>),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
    )
)]
pub async fn list_jobs(State(state): State<AppState>) -> Json<Vec<Job>> {
    Json(state.ops.jobs())
}

fn find_job(state: &AppState, id: &str) -> Result<watch::Receiver<Job>, ApiError> {
    valid_id(id)?;
    state
        .ops
        .job(id)
        .ok_or(ApiError::new(ErrorCode::NotFound, "No such job."))
}

/// One job.
#[utoipa::path(
    get,
    path = "/v1/jobs/{id}",
    tag = "operations",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Job ID")),
    responses(
        (status = 200, body = Job),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 404, body = hs_model::ErrorBody, description = "No such job"),
    )
)]
pub async fn get_job(State(state): State<AppState>, Path(id): Path<String>) -> Result<Json<Job>, ApiError> {
    let job = find_job(&state, &id)?;
    let current = job.borrow().clone();
    Ok(Json(current))
}

/// WebSocket: a `snapshot` of the job, a `job` message after every change
/// (the last one finished), and a `heartbeat` whenever nothing changed for
/// five seconds. The server closes the stream after the final state.
/// Messages: `StreamMessage`.
#[utoipa::path(
    get,
    path = "/v1/jobs/{id}/events",
    tag = "operations",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Job ID")),
    responses(
        (status = 101, description = "WebSocket of `StreamMessage`"),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 404, body = hs_model::ErrorBody, description = "No such job"),
    )
)]
pub async fn job_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let job = find_job(&state, &id)?;
    let instance = state.ops.instance_id.clone();
    let heartbeat = state.ops.options.heartbeat;
    Ok(upgrade
        .max_message_size(4096)
        .on_upgrade(move |socket| stream(socket, job, instance, heartbeat)))
}

struct Stream {
    socket: WebSocket,
    instance: String,
    sequence: u64,
}

impl Stream {
    async fn send(&mut self, kind: StreamMessageType, data: Option<Job>) -> bool {
        self.sequence += 1;
        let message = StreamMessage {
            schema_version: SCHEMA_VERSION,
            kind,
            server_instance_id: self.instance.clone(),
            stream_sequence: self.sequence,
            server_time: timestamp(SystemTime::now()),
            data,
        };
        match serde_json::to_string(&message) {
            Ok(text) => self.socket.send(Message::Text(text.into())).await.is_ok(),
            Err(_) => false,
        }
    }
}

async fn stream(socket: WebSocket, mut job: watch::Receiver<Job>, instance: String, heartbeat: std::time::Duration) {
    let mut out = Stream {
        socket,
        instance,
        sequence: 0,
    };
    let snapshot = job.borrow_and_update().clone();
    let mut finished = snapshot.state.finished();
    if !out.send(StreamMessageType::Snapshot, Some(snapshot)).await {
        return;
    }
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + heartbeat, heartbeat);
    while !finished {
        tokio::select! {
            changed = job.changed() => {
                if changed.is_err() {
                    break;
                }
                let current = job.borrow_and_update().clone();
                finished = current.state.finished();
                if !out.send(StreamMessageType::Job, Some(current)).await {
                    return;
                }
                ticker.reset();
            }
            _ = ticker.tick() => {
                if !out.send(StreamMessageType::Heartbeat, None).await {
                    return;
                }
            }
            incoming = out.socket.recv() => {
                // The stream is one-way; a close or a broken socket ends it.
                if matches!(incoming, None | Some(Err(_) | Ok(Message::Close(_)))) {
                    return;
                }
            }
        }
    }
    let _ = out
        .socket
        .send(Message::Close(Some(CloseFrame {
            code: axum::extract::ws::close_code::NORMAL,
            reason: "job finished".into(),
        })))
        .await;
}
