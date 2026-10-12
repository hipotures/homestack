//! HTTP routes of the simulated `/api2/json`.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hs_model::NodeName;
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::ops::Op;
use crate::scenario::{NodeState, TokenSpec};
use crate::state::{ClusterState, has_privilege};

type Shared = Arc<Mutex<ClusterState>>;

const PREFIX: &str = "/api2/json";

pub fn router(state: Shared) -> Router {
    let api = Router::new()
        .route("/version", get(version))
        .route("/nodes", get(nodes))
        .route("/nodes/{node}/status", get(node_status))
        .route("/nodes/{node}/tasks/{upid}/status", get(task_status))
        .route("/nodes/{node}/tasks/{upid}/log", get(task_log))
        .fallback(not_implemented);
    Router::new()
        .nest(PREFIX, api)
        .fallback(not_implemented)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            authenticate_and_record,
        ))
        .with_state(state)
}

fn lock(state: &Shared) -> MutexGuard<'_, ClusterState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Data response: `{"data": ...}`.
fn data(value: Value) -> Response {
    axum::Json(json!({ "data": value })).into_response()
}

/// Error response shaped like PVE's: `{"data":null,"message":"..."}`.
///
/// PVE ends most messages with a newline (403, 500) but not all of them
/// (501), so callers pass the exact text.
fn pve_error(status: u16, message: &str) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, axum::Json(json!({ "data": null, "message": message }))).into_response()
}

/// A PVE error response not yet rendered.
struct Failure {
    status: u16,
    message: String,
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        pve_error(self.status, &self.message)
    }
}

/// Reject unauthenticated calls the way PVE does (401, empty body), record
/// the rest as operations and apply injected faults.
async fn authenticate_and_record(State(state): State<Shared>, mut request: Request, next: Next) -> Response {
    let header_value = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let token = lock(&state).authenticate(header_value).cloned();
    let Some(token) = token else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let path = percent_decode_str(request.uri().path())
        .decode_utf8_lossy()
        .into_owned();
    let path = path.strip_prefix(PREFIX).unwrap_or(&path).to_owned();
    let query = form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes()).into_owned();
    let fault = lock(&state).record(Op::get(&path, query));
    if let Some((status, message)) = fault {
        return pve_error(status, &message);
    }
    request.extensions_mut().insert(token);
    next.run(request).await
}

fn token(request_token: Option<axum::Extension<TokenSpec>>) -> Option<TokenSpec> {
    request_token.map(|axum::Extension(token)| token)
}

async fn not_implemented(request: Request) -> Response {
    let path = request
        .uri()
        .path()
        .strip_prefix(PREFIX)
        .unwrap_or(request.uri().path())
        .to_owned();
    pve_error(501, &format!("Method '{} {path}' not implemented", request.method()))
}

async fn version(State(state): State<Shared>) -> Response {
    let state = lock(&state);
    data(json!({
        "release": state.version.release,
        "version": state.version.version,
        "repoid": state.version.repoid,
    }))
}

async fn nodes(State(state): State<Shared>) -> Response {
    let state = lock(&state);
    let nodes: Vec<Value> = state
        .nodes
        .values()
        .map(|node| match node.status {
            NodeState::Online => json!({
                "node": node.name,
                "id": format!("node/{}", node.name),
                "type": "node",
                "status": node.status.as_str(),
                "level": "",
                "uptime": node.uptime,
                "cpu": node.cpu,
                "maxcpu": node.maxcpu,
                "mem": node.mem,
                "maxmem": node.maxmem,
                "disk": node.disk,
                "maxdisk": node.maxdisk,
            }),
            NodeState::Offline => json!({
                "node": node.name,
                "id": format!("node/{}", node.name),
                "type": "node",
                "status": node.status.as_str(),
            }),
        })
        .collect();
    data(Value::Array(nodes))
}

/// Checks shared by every `/nodes/{node}/...` route: the privilege first,
/// then whether the request can be proxied to the node.
fn reach_node(
    state: &ClusterState,
    token: Option<&TokenSpec>,
    node: &str,
    privilege: &str,
) -> Result<NodeName, Failure> {
    let acl_path = format!("/nodes/{node}");
    if !token.is_some_and(|token| has_privilege(token, &acl_path, privilege)) {
        return Err(Failure {
            status: 403,
            message: format!("Permission check failed ({acl_path}, {privilege})\n"),
        });
    }
    let known = NodeName::parse(node).ok().filter(|name| state.nodes.contains_key(name));
    let Some(name) = known else {
        // Observed on PVE 9.2 for a name that is not a cluster node.
        return Err(Failure {
            status: 500,
            message: format!(
                "hostname lookup '{node}' failed - failed to get address info for: {node}: Name or service not known\n"
            ),
        });
    };
    if state
        .nodes
        .get(&name)
        .is_some_and(|spec| spec.status == NodeState::Offline)
    {
        // Not observed live: PVE reports proxy failures with status 595.
        return Err(Failure {
            status: 595,
            message: "No route to host\n".to_owned(),
        });
    }
    Ok(name)
}

async fn node_status(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path(node): Path<String>,
) -> Response {
    let state = lock(&state);
    let name = match reach_node(&state, token(request_token).as_ref(), &node, "Sys.Audit") {
        Ok(name) => name,
        Err(failure) => return failure.into_response(),
    };
    let Some(spec) = state.nodes.get(&name) else {
        return pve_error(500, "node vanished");
    };
    data(json!({
        "pveversion": spec.pveversion,
        "kversion": spec.kversion,
        "uptime": spec.uptime,
        "cpu": spec.cpu,
        "memory": { "total": spec.maxmem, "used": spec.mem, "free": spec.maxmem.saturating_sub(spec.mem) },
    }))
}

async fn task_status(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, upid)): Path<(String, String)>,
) -> Response {
    let mut state = lock(&state);
    let name = match reach_node(&state, token(request_token).as_ref(), &node, "Sys.Audit") {
        Ok(name) => name,
        Err(failure) => return failure.into_response(),
    };
    let Some(task) = state.task_mut(&name, &upid) else {
        return pve_error(500, "no such task\n");
    };
    task.polls = task.polls.saturating_add(1);
    let mut body = json!({
        "upid": task.upid,
        "node": task.spec.node,
        "pid": task.pid,
        "pstart": task.pstart,
        "starttime": task.starttime,
        "type": task.spec.kind,
        "id": task.spec.id,
        "user": task.spec.user,
        "status": if task.done() { "stopped" } else { "running" },
    });
    if task.done() {
        body["exitstatus"] = Value::String(task.spec.exitstatus.clone());
    }
    data(body)
}

#[derive(Deserialize)]
struct LogQuery {
    #[serde(default)]
    start: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

const fn default_limit() -> usize {
    50
}

async fn task_log(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, upid)): Path<(String, String)>,
    Query(query): Query<LogQuery>,
) -> Response {
    let mut state = lock(&state);
    let name = match reach_node(&state, token(request_token).as_ref(), &node, "Sys.Audit") {
        Ok(name) => name,
        Err(failure) => return failure.into_response(),
    };
    let Some(task) = state.task_mut(&name, &upid) else {
        return pve_error(500, "no such task\n");
    };
    let lines: Vec<Value> = task
        .visible_log()
        .iter()
        .enumerate()
        .skip(query.start)
        .take(query.limit)
        .map(|(index, line)| json!({ "n": index + 1, "t": line }))
        .collect();
    data(Value::Array(lines))
}
