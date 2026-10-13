//! HTTP routes of the simulated `/api2/json`.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use base64::Engine;
use hs_model::NodeName;
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::guest;
use crate::ops::Op;
use crate::scenario::{
    FilesystemSpec, InterfaceSpec, NodeState, TaskSpec, TokenSpec, UserEntry, VmSpec, VmState, VolumeSpec,
};
use crate::state::{ClusterState, PRIVILEGES, config_digest, has_any_privilege, has_privilege, volume_owner};

type Shared = Arc<Mutex<ClusterState>>;

const PREFIX: &str = "/api2/json";

pub fn router(state: Shared) -> Router {
    let api = Router::new()
        .route("/version", get(version))
        .route("/nodes", get(nodes))
        .route("/access/permissions", get(permissions))
        .route("/cluster/resources", get(cluster_resources))
        .route("/storage", get(storage_definitions))
        .route("/pools", get(pools))
        .route("/nodes/{node}/network", get(node_network))
        .route("/nodes/{node}/dns", get(node_dns))
        .route("/nodes/{node}/storage", get(node_storages))
        .route("/nodes/{node}/storage/{storage}/status", get(storage_status))
        .route(
            "/nodes/{node}/storage/{storage}/content",
            get(storage_content).post(volume_alloc),
        )
        .route("/nodes/{node}/storage/{storage}/content/{volume}", delete(volume_free))
        .route("/nodes/{node}/qemu/{vmid}", delete(vm_destroy))
        .route("/nodes/{node}/qemu/{vmid}/clone", post(vm_clone))
        .route(
            "/nodes/{node}/qemu/{vmid}/config",
            get(vm_config).put(vm_config_update).post(vm_config_post),
        )
        .route("/nodes/{node}/qemu/{vmid}/status/current", get(vm_status))
        .route("/nodes/{node}/qemu/{vmid}/status/{action}", post(vm_power))
        .route("/nodes/{node}/qemu/{vmid}/agent/file-read", get(agent_file_read))
        .route("/nodes/{node}/qemu/{vmid}/agent/ping", post(agent_ping))
        .route("/nodes/{node}/qemu/{vmid}/agent/exec", post(agent_exec))
        .route("/nodes/{node}/qemu/{vmid}/agent/exec-status", get(agent_exec_status))
        .route("/nodes/{node}/qemu/{vmid}/agent/{command}", get(agent_read))
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
async fn authenticate_and_record(State(state): State<Shared>, request: Request, next: Next) -> Response {
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
    let query: Vec<(String, String)> = form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
        .into_owned()
        .collect();
    let (parts, body) = request.into_parts();
    let Ok(body) = axum::body::to_bytes(body, 1 << 20).await else {
        return pve_error(400, "request body too large\n");
    };
    let op = operation(&parts.method, &path, query, &body);
    let fault = lock(&state).record(op);
    if let Some((status, message)) = fault {
        return pve_error(status, &message);
    }
    let mut request = Request::from_parts(parts, Body::from(body));
    request.extensions_mut().insert(token);
    next.run(request).await
}

/// The canonical operation of a call (`parity/ops.md`).
fn operation(method: &Method, path: &str, query: Vec<(String, String)>, body: &Bytes) -> Op {
    let segments: Vec<&str> = path.split('/').collect();
    let agent = match segments.as_slice() {
        ["", "nodes", _, "qemu", vmid, "agent", command] => Some(((*vmid).to_owned(), *command)),
        _ => None,
    };
    match (method, agent) {
        (&Method::GET, Some((vmid, "exec-status"))) => Op::new("guest_exec_status", [("vmid", vmid)]),
        (&Method::POST, Some((vmid, "ping"))) => Op::new("guest_ping", [("vmid", vmid)]),
        (&Method::POST, Some((vmid, "exec"))) => Op::new("guest_exec", [("cmd", exec_shell(body)), ("vmid", vmid)]),
        (&Method::GET, _) => Op::get(path, query),
        (method, _) => Op::new(&method.as_str().to_ascii_lowercase(), [("path", path.to_owned())]),
    }
}

/// The shell command of an `exec` body: `command` as a JSON array or string,
/// or repeated form fields. `["/bin/sh", "-lc", CMD]` is reported as `CMD`.
fn exec_shell(body: &Bytes) -> String {
    let words: Vec<String> = match serde_json::from_slice::<Value>(body) {
        Ok(json) => match &json["command"] {
            Value::Array(items) => items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect(),
            Value::String(text) => vec![text.clone()],
            _ => Vec::new(),
        },
        Err(_) => form_urlencoded::parse(body)
            .filter(|(key, _)| key == "command")
            .map(|(_, value)| value.into_owned())
            .collect(),
    };
    match words.as_slice() {
        [shell, flag, command] if shell == "/bin/sh" && flag == "-lc" => command.clone(),
        _ => words.join(" "),
    }
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

#[derive(Deserialize)]
struct PermissionsQuery {
    path: Option<String>,
}

/// The token's effective privileges: on `path`, or on every path of its ACL.
/// Any token may read its own; PVE answers for paths that do not exist.
async fn permissions(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Query(query): Query<PermissionsQuery>,
) -> Response {
    let state = lock(&state);
    let Some(token) = token(request_token) else {
        return pve_error(401, "");
    };
    let paths: Vec<String> = match query.path {
        Some(path) => vec![path],
        None => token.acl.keys().cloned().collect(),
    };
    let mut body = serde_json::Map::new();
    for path in paths {
        let privileges: serde_json::Map<String, Value> = PRIVILEGES
            .iter()
            .filter(|privilege| state.allows(&token, &path, privilege))
            .map(|privilege| ((*privilege).to_owned(), json!(1)))
            .collect();
        body.insert(path, Value::Object(privileges));
    }
    data(Value::Object(body))
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
    reach(state, token, node, Some((&format!("/nodes/{node}"), &[privilege])))
}

/// `reach_node` with any ACL check: `None` for routes every user may call,
/// else the path and the privileges of which one is enough.
fn reach(
    state: &ClusterState,
    token: Option<&TokenSpec>,
    node: &str,
    check: Option<(&str, &[&str])>,
) -> Result<NodeName, Failure> {
    if let Some((acl_path, privileges)) = check
        && !token.is_some_and(|token| state.allows_any(token, acl_path, privileges))
    {
        return Err(Failure {
            status: 403,
            message: format!("Permission check failed ({acl_path}, {})\n", privileges.join("|")),
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
    let visible = task.visible_log();
    if visible.is_empty() {
        // PVE's `dump_logfile` answers an empty log with a made-up line
        // (observed on PVE 9.2 while a task had not written yet).
        return data(json!([{ "n": 1, "t": "no content" }]));
    }
    let lines: Vec<Value> = visible
        .iter()
        .enumerate()
        .skip(query.start)
        .take(query.limit)
        .map(|(index, line)| json!({ "n": index + 1, "t": line }))
        .collect();
    data(Value::Array(lines))
}

fn query_value<'a>(query: &'a [(String, String)], key: &str) -> Option<&'a str> {
    query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// `GET /cluster/resources`: every user may call it; VMs appear only with
/// `VM.Audit` on them. VMs on an offline node report status `unknown` (not
/// observed live).
async fn cluster_resources(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Query(query): Query<Vec<(String, String)>>,
) -> Response {
    let state = lock(&state);
    let Some(token) = token(request_token) else {
        return pve_error(401, "");
    };
    if query_value(&query, "type").is_some_and(|kind| kind != "vm") {
        return data(Value::Array(Vec::new()));
    }
    let list: Vec<Value> = state
        .vms
        .values()
        .filter(|vm| state.allows(&token, &format!("/vms/{}", vm.vmid), "VM.Audit"))
        .map(|vm| {
            let online = state
                .nodes
                .get(&vm.node)
                .is_some_and(|node| node.status == NodeState::Online);
            let status = if online { vm.status.as_str() } else { "unknown" };
            let mut item = json!({
                "id": format!("qemu/{}", vm.vmid),
                "type": "qemu",
                "vmid": vm.vmid,
                "node": vm.node,
                "status": status,
                "template": 0,
                "uptime": 0,
            });
            for key in ["name", "tags"] {
                if let Some(value) = vm.config.get(key) {
                    item[key] = toml_to_json(value);
                }
            }
            // `pool` only with Pool.Audit on it (observed in phase 3).
            if let Some(pool) = state.pool_of(vm.vmid)
                && has_privilege(&token, &format!("/pool/{pool}"), "Pool.Audit")
            {
                item["pool"] = json!(pool);
            }
            item
        })
        .collect();
    data(Value::Array(list))
}

/// `GET /pools`: the pools with `Pool.Audit` on them; with `poolid`, that
/// pool and its `members` (PVE 9.2).
async fn pools(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Query(query): Query<Vec<(String, String)>>,
) -> Response {
    let state = lock(&state);
    let Some(token) = token(request_token) else {
        return pve_error(401, "");
    };
    let wanted = query_value(&query, "poolid");
    let list: Vec<Value> = state
        .pools
        .iter()
        .filter(|pool| wanted.is_none_or(|id| id == pool.poolid))
        .filter(|pool| has_privilege(&token, &format!("/pool/{}", pool.poolid), "Pool.Audit"))
        .map(|pool| {
            let mut item = json!({"poolid": pool.poolid, "comment": pool.comment});
            if wanted.is_some() {
                let members: Vec<Value> = pool
                    .vms
                    .iter()
                    .filter_map(|vmid| state.vms.get(vmid))
                    .map(|vm| json!({"id": format!("qemu/{}", vm.vmid), "type": "qemu", "vmid": vm.vmid, "node": vm.node}))
                    .collect();
                item["members"] = Value::Array(members);
            }
            item
        })
        .collect();
    if let Some(id) = wanted
        && list.is_empty()
    {
        // The message seen with `pvesh` on PVE 9.2.21; the status is not observed.
        return pve_error(500, &format!("pool '{id}' does not exist\n"));
    }
    data(Value::Array(list))
}

/// `GET /storage`: definitions with `Datastore.Audit` or
/// `Datastore.AllocateSpace` on them.
async fn storage_definitions(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
) -> Response {
    let state = lock(&state);
    let Some(token) = token(request_token) else {
        return pve_error(401, "");
    };
    let list: Vec<Value> = state
        .storages
        .iter()
        .filter(|s| storage_visible(&token, &s.storage))
        .map(|s| {
            let mut item = json!({
                "storage": s.storage,
                "type": s.kind,
                "content": s.content,
                "digest": "0000000000000000000000000000000000000000",
            });
            if s.shared {
                item["shared"] = json!(1);
            }
            if let Some(nodes) = &s.nodes {
                item["nodes"] = json!(nodes);
            }
            item
        })
        .collect();
    data(Value::Array(list))
}

fn storage_visible(token: &TokenSpec, storage: &str) -> bool {
    has_any_privilege(
        token,
        &format!("/storage/{storage}"),
        &["Datastore.Audit", "Datastore.AllocateSpace"],
    )
}

async fn node_network(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path(node): Path<String>,
) -> Response {
    let state = lock(&state);
    let name = match reach(&state, token(request_token).as_ref(), &node, None) {
        Ok(name) => name,
        Err(failure) => return failure.into_response(),
    };
    let list = state
        .nodes
        .get(&name)
        .map(|spec| {
            spec.network
                .iter()
                .map(|entry| toml_to_json(&toml::Value::Table(entry.clone())))
                .collect()
        })
        .unwrap_or_default();
    data(Value::Array(list))
}

async fn node_dns(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path(node): Path<String>,
) -> Response {
    let state = lock(&state);
    let name = match reach_node(&state, token(request_token).as_ref(), &node, "Sys.Audit") {
        Ok(name) => name,
        Err(failure) => return failure.into_response(),
    };
    let dns = state
        .nodes
        .get(&name)
        .map_or(Value::Null, |spec| toml_to_json(&toml::Value::Table(spec.dns.clone())));
    data(dns)
}

/// One node storage as `GET /nodes/{node}/storage` lists it.
fn node_storage_json(state: &ClusterState, node: &NodeName, storage: &str) -> Option<Value> {
    let spec = state.nodes.get(node)?.storages.iter().find(|s| s.storage == storage)?;
    let definition = state.storages.iter().find(|d| d.storage == storage)?;
    let mut item = json!({
        "storage": spec.storage,
        "type": definition.kind,
        "content": definition.content,
        "enabled": u8::from(spec.enabled),
        "active": u8::from(spec.active),
        "shared": u8::from(definition.shared),
    });
    if spec.active {
        item["total"] = json!(spec.total);
        item["used"] = json!(spec.used);
        item["avail"] = json!(spec.avail);
    }
    Some(item)
}

/// `GET /nodes/{node}/storage`: every user; only storages the token may
/// audit or allocate on.
async fn node_storages(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path(node): Path<String>,
) -> Response {
    let state = lock(&state);
    let token = token(request_token);
    let name = match reach(&state, token.as_ref(), &node, None) {
        Ok(name) => name,
        Err(failure) => return failure.into_response(),
    };
    let Some(token) = token else {
        return pve_error(401, "");
    };
    let list: Vec<Value> = state
        .nodes
        .get(&name)
        .map(|spec| {
            spec.storages
                .iter()
                .filter(|s| storage_visible(&token, &s.storage))
                .filter_map(|s| node_storage_json(&state, &name, &s.storage))
                .collect()
        })
        .unwrap_or_default();
    data(Value::Array(list))
}

async fn storage_status(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, storage)): Path<(String, String)>,
) -> Response {
    let state = lock(&state);
    let acl = format!("/storage/{storage}");
    let name = match reach(
        &state,
        token(request_token).as_ref(),
        &node,
        Some((&acl, &["Datastore.Audit", "Datastore.AllocateSpace"])),
    ) {
        Ok(name) => name,
        Err(failure) => return failure.into_response(),
    };
    match node_storage_json(&state, &name, &storage) {
        Some(mut item) => {
            if let Value::Object(map) = &mut item {
                map.remove("storage");
            }
            data(item)
        }
        // Observed on PVE 9.2.
        None => (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "data": null,
                "message": "Parameter verification failed.\n",
                "errors": { "storage": "No such storage." },
            })),
        )
            .into_response(),
    }
}

/// `GET /nodes/{node}/storage/{storage}/content`. Each VM volume is listed
/// only with `Datastore.Allocate` on the storage, or `Datastore.Audit` on it
/// and `VM.Config.Disk` on the owner VM (`PVE::Storage::check_volume_access`
/// on PVE 9.2).
async fn storage_content(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, storage)): Path<(String, String)>,
    Query(query): Query<Vec<(String, String)>>,
) -> Response {
    let state = lock(&state);
    let token = token(request_token);
    let acl = format!("/storage/{storage}");
    let name = match reach(
        &state,
        token.as_ref(),
        &node,
        Some((&acl, &["Datastore.Audit", "Datastore.AllocateSpace"])),
    ) {
        Ok(name) => name,
        Err(failure) => return failure.into_response(),
    };
    let Some(token) = token else {
        return pve_error(401, "");
    };
    let exists = state
        .nodes
        .get(&name)
        .is_some_and(|spec| spec.storages.iter().any(|s| s.storage == storage));
    if !exists {
        // Observed on PVE 9.2.
        return pve_error(500, &format!("storage '{storage}' does not exist\n"));
    }
    if query_value(&query, "content").is_some_and(|content| content != "images") {
        return data(Value::Array(Vec::new()));
    }
    let allocate = has_privilege(&token, &acl, "Datastore.Allocate");
    let audit = has_privilege(&token, &acl, "Datastore.Audit");
    let list: Vec<Value> = state
        .volumes
        .iter()
        .filter(|volume| volume.node == name && volume.storage == storage)
        .filter_map(|volume| {
            let owner = volume_owner(&volume.volid);
            let visible = allocate
                || owner.is_some_and(|vmid| audit && state.allows(&token, &format!("/vms/{vmid}"), "VM.Config.Disk"));
            if !visible {
                return None;
            }
            let mut item = json!({
                "volid": volume.volid,
                "content": "images",
                "format": volume.format,
                "size": volume.size,
                "ctime": 1_790_000_000_u64,
            });
            if let Some(vmid) = owner {
                item["vmid"] = json!(vmid);
            }
            Some(item)
        })
        .collect();
    data(Value::Array(list))
}

/// The node and VM of a `/nodes/{node}/qemu/{vmid}/...` route after the ACL
/// check on `/vms/{vmid}`.
fn reach_vm<'a>(
    state: &'a ClusterState,
    token: Option<&TokenSpec>,
    node: &str,
    vmid: &str,
    privileges: &[&str],
) -> Result<&'a VmSpec, Failure> {
    let acl = format!("/vms/{vmid}");
    let name = reach(state, token, node, Some((&acl, privileges)))?;
    vmid.parse::<u32>()
        .ok()
        .and_then(|id| state.vms.get(&id))
        .filter(|vm| vm.node == name)
        .ok_or_else(|| Failure {
            // Observed on PVE 9.2.
            status: 500,
            message: format!("Configuration file 'nodes/{node}/qemu-server/{vmid}.conf' does not exist\n"),
        })
}

async fn vm_config(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, vmid)): Path<(String, String)>,
) -> Response {
    let state = lock(&state);
    match reach_vm(&state, token(request_token).as_ref(), &node, &vmid, &["VM.Audit"]) {
        Ok(vm) => {
            let mut config = toml_to_json(&toml::Value::Table(vm.config.clone()));
            config["digest"] = json!(config_digest(&vm.config));
            data(config)
        }
        Err(failure) => failure.into_response(),
    }
}

async fn vm_status(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, vmid)): Path<(String, String)>,
) -> Response {
    let state = lock(&state);
    match reach_vm(&state, token(request_token).as_ref(), &node, &vmid, &["VM.Audit"]) {
        Ok(vm) => {
            let mut body = json!({
                "vmid": vm.vmid,
                "status": vm.status.as_str(),
                "qmpstatus": vm.status.as_str(),
                "agent": 1,
            });
            if let Some(name) = vm.config.get("name") {
                body["name"] = toml_to_json(name);
            }
            data(body)
        }
        Err(failure) => failure.into_response(),
    }
}

/// Keys `PUT config` accepts in the simulator: those guarded by
/// `VM.Config.Options` in PVE's `check_vm_modify_config_perm`.
const OPTION_KEYS: [&str; 4] = ["description", "name", "onboot", "tags"];

/// `PUT /nodes/{node}/qemu/{vmid}/config` for option keys, with `delete` and
/// the optimistic lock on `digest` (PVE's `assert_if_modified`). Other keys
/// are not modelled yet.
async fn vm_config_update(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, vmid)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let mut state = lock(&state);
    let form: Vec<(String, String)> = form_urlencoded::parse(&body).into_owned().collect();
    let keys: Vec<&str> = form
        .iter()
        .filter(|(key, _)| key != "digest" && key != "delete")
        .map(|(key, _)| key.as_str())
        .chain(
            form.iter()
                .filter(|(key, _)| key == "delete")
                .flat_map(|(_, value)| value.split(',')),
        )
        .collect();
    if let Some(key) = keys.iter().find(|key| !OPTION_KEYS.contains(key)) {
        return pve_error(501, &format!("pve-sim: config key '{key}' not modelled"));
    }
    let vmid_number = match reach_vm(
        &state,
        token(request_token).as_ref(),
        &node,
        &vmid,
        &["VM.Config.Options"],
    ) {
        Ok(vm) => vm.vmid,
        Err(failure) => return failure.into_response(),
    };
    let Some(vm) = state.vms.get_mut(&vmid_number) else {
        return pve_error(500, "VM vanished");
    };
    if let Some((_, digest)) = form.iter().find(|(key, _)| key == "digest")
        && *digest != config_digest(&vm.config)
    {
        return pve_error(
            500,
            "detected modified configuration - file changed by other user? Try again.\n",
        );
    }
    for (key, value) in &form {
        match key.as_str() {
            "digest" => {}
            "delete" => {
                for key in value.split(',') {
                    vm.config.remove(key);
                }
            }
            key => {
                vm.config.insert(key.to_owned(), toml::Value::String(value.clone()));
            }
        }
    }
    data(Value::Null)
}

/// `POST .../status/{start,shutdown,stop}`: a task that ends after two
/// status polls. The VM's state changes when the task is created. Not
/// observed live: the messages of a failing start or shutdown.
async fn vm_power(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, vmid, action)): Path<(String, String, String)>,
) -> Response {
    let mut state = lock(&state);
    let Some(token) = token(request_token) else {
        return pve_error(401, "");
    };
    let kind = match action.as_str() {
        "start" => "qmstart",
        "shutdown" => "qmshutdown",
        "stop" => "qmstop",
        other => {
            return pve_error(
                501,
                &format!("Method 'POST /nodes/{node}/qemu/{vmid}/status/{other}' not implemented"),
            );
        }
    };
    let (vmid_number, status, acpi, node_name) = match reach_vm(&state, Some(&token), &node, &vmid, &["VM.PowerMgmt"]) {
        Ok(vm) => (vm.vmid, vm.status, vm.acpi, vm.node.clone()),
        Err(failure) => return failure.into_response(),
    };
    let (next, error) = match (kind, status) {
        ("qmstart", VmState::Running) => (status, Some(format!("VM {vmid_number} already running"))),
        ("qmstart", VmState::Stopped) => (VmState::Running, None),
        ("qmshutdown" | "qmstop", VmState::Stopped) => (status, Some(format!("VM {vmid_number} not running"))),
        ("qmshutdown", VmState::Running) if !acpi => (status, Some("VM quit/powerdown failed".to_owned())),
        _ => (VmState::Stopped, None),
    };
    if let Some(vm) = state.vms.get_mut(&vmid_number) {
        vm.status = next;
    }
    if kind == "qmstart" && error.is_none() {
        boot_clone(&mut state, vmid_number);
    }
    let (log, exitstatus) = match error {
        Some(message) => (vec![message.clone(), format!("TASK ERROR: {message}")], message),
        None => (vec!["TASK OK".to_owned()], "OK".to_owned()),
    };
    let upid = state.add_task(TaskSpec {
        node: node_name,
        kind: kind.to_owned(),
        id: vmid_number.to_string(),
        user: token.id.clone(),
        log,
        polls_until_done: 2,
        exitstatus,
    });
    data(Value::String(upid))
}

#[derive(Deserialize)]
struct FileReadQuery {
    file: String,
    #[serde(default)]
    offset: usize,
    count: Option<usize>,
    decode: Option<u8>,
}

/// `GET .../agent/file-read`: PVE 9.2 opens the file, seeks to `offset`,
/// reads up to `count` bytes (16 MiB by default) and returns them decoded,
/// or base64 with `decode=0`; `truncated` is set when the end was not
/// reached. Errors are qemu-ga's, as observed.
async fn agent_file_read(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, vmid)): Path<(String, String)>,
    Query(query): Query<FileReadQuery>,
) -> Response {
    const MAX_READ: usize = 16 * 1024 * 1024;
    let state = lock(&state);
    let (_, guest) = match reach_guest(
        &state,
        token(request_token).as_ref(),
        &node,
        &vmid,
        &["VM.GuestAgent.FileRead", "VM.GuestAgent.Unrestricted"],
    ) {
        Ok(found) => found,
        Err(failure) => return failure.into_response(),
    };
    let bytes = match guest::file(guest, &query.file) {
        guest::Node::File(bytes) => bytes,
        guest::Node::Directory => return pve_error(500, "Agent error: failed to read file: Is a directory\n"),
        guest::Node::Missing => {
            return pve_error(
                500,
                &format!(
                    "Agent error: failed to open file '{}' (mode: 'r'): No such file or directory\n",
                    query.file
                ),
            );
        }
    };
    let count = query.count.unwrap_or(MAX_READ).min(MAX_READ);
    let start = query.offset.min(bytes.len());
    let end = start.saturating_add(count).min(bytes.len());
    let chunk = &bytes[start..end];
    let content = if query.decode == Some(0) {
        base64::engine::general_purpose::STANDARD.encode(chunk)
    } else {
        String::from_utf8_lossy(chunk).into_owned()
    };
    let mut body = json!({ "content": content, "bytes-read": chunk.len() });
    if end < bytes.len() {
        body["truncated"] = json!(1);
    }
    data(body)
}

const AGENT_READ: [&str; 2] = ["VM.GuestAgent.Audit", "VM.GuestAgent.Unrestricted"];

/// The guest of a running VM with a responding agent; errors observed on
/// PVE 9.2.
fn reach_guest<'a>(
    state: &'a ClusterState,
    token: Option<&TokenSpec>,
    node: &str,
    vmid: &str,
    privileges: &[&str],
) -> Result<(&'a VmSpec, &'a crate::scenario::GuestSpec), Failure> {
    let vm = reach_vm(state, token, node, vmid, privileges)?;
    if vm.status != VmState::Running {
        return Err(Failure {
            status: 500,
            message: format!("VM {vmid} is not running\n"),
        });
    }
    let guest = vm.guest.as_ref().ok_or_else(|| Failure {
        status: 500,
        message: "QEMU guest agent is not running\n".to_owned(),
    })?;
    Ok((vm, guest))
}

async fn agent_ping(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, vmid)): Path<(String, String)>,
) -> Response {
    let state = lock(&state);
    match reach_guest(&state, token(request_token).as_ref(), &node, &vmid, &AGENT_READ) {
        Ok(_) => data(json!({ "result": {} })),
        Err(failure) => failure.into_response(),
    }
}

async fn agent_read(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, vmid, command)): Path<(String, String, String)>,
) -> Response {
    let state = lock(&state);
    let (_, guest) = match reach_guest(&state, token(request_token).as_ref(), &node, &vmid, &AGENT_READ) {
        Ok(found) => found,
        Err(failure) => return failure.into_response(),
    };
    let result = match command.as_str() {
        "get-host-name" => guest::host_name(guest),
        "network-get-interfaces" => guest::interfaces(guest),
        "get-fsinfo" => guest::fsinfo(guest),
        other => {
            return pve_error(
                501,
                &format!("Method 'GET /nodes/{node}/qemu/{vmid}/agent/{other}' not implemented"),
            );
        }
    };
    data(json!({ "result": result }))
}

async fn agent_exec(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, vmid)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let mut state = lock(&state);
    let result = match reach_guest(
        &state,
        token(request_token).as_ref(),
        &node,
        &vmid,
        &["VM.GuestAgent.Unrestricted"],
    ) {
        Ok((vm, guest)) => (vm.vmid, guest::exec(guest, &exec_shell(&body))),
        Err(failure) => return failure.into_response(),
    };
    let pid = state.add_exec(result.0, result.1);
    data(json!({ "pid": pid }))
}

async fn agent_exec_status(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, vmid)): Path<(String, String)>,
    Query(query): Query<Vec<(String, String)>>,
) -> Response {
    let state = lock(&state);
    let vm = match reach_guest(
        &state,
        token(request_token).as_ref(),
        &node,
        &vmid,
        &["VM.GuestAgent.Unrestricted"],
    ) {
        Ok((vm, _)) => vm.vmid,
        Err(failure) => return failure.into_response(),
    };
    let pid = query_value(&query, "pid").and_then(|pid| pid.parse::<u32>().ok());
    match pid.and_then(|pid| state.exec.get(&(vm, pid))) {
        Some(result) => data(result.status_json()),
        None => pve_error(500, "Agent error: Invalid parameter 'pid'\n"),
    }
}

/// A scenario value as Proxmox returns it in JSON.
fn toml_to_json(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(text) => Value::String(text.clone()),
        toml::Value::Integer(number) => json!(number),
        toml::Value::Float(number) => json!(number),
        toml::Value::Boolean(flag) => json!(u8::from(*flag)),
        toml::Value::Datetime(datetime) => Value::String(datetime.to_string()),
        toml::Value::Array(items) => Value::Array(items.iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => Value::Object(table.iter().map(|(k, v)| (k.clone(), toml_to_json(v))).collect()),
    }
}

// --- lifecycle (phase 5) -------------------------------------------------

fn form(body: &Bytes) -> Vec<(String, String)> {
    form_urlencoded::parse(body).into_owned().collect()
}

fn form_value<'a>(form: &'a [(String, String)], key: &str) -> Option<&'a str> {
    form.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn forbidden(path: &str, privileges: &str) -> Response {
    pve_error(403, &format!("Permission check failed ({path}, {privileges})\n"))
}

/// A task that ends after two status polls with `log` and `exitstatus`.
fn task(state: &mut ClusterState, node: &NodeName, kind: &str, id: u32, user: &str) -> Response {
    let upid = state.add_task(TaskSpec {
        node: node.clone(),
        kind: kind.to_owned(),
        id: id.to_string(),
        user: user.to_owned(),
        log: vec!["TASK OK".to_owned()],
        polls_until_done: 2,
        exitstatus: "OK".to_owned(),
    });
    data(Value::String(upid))
}

/// `20G`, `512M`, `1T`, or bytes.
fn size_bytes(text: &str) -> Option<u64> {
    let text = text.trim();
    let (number, unit) = text
        .find(|c: char| !c.is_ascii_digit())
        .map_or((text, ""), |at| text.split_at(at));
    let value: u64 = number.parse().ok()?;
    let factor: u64 = match unit {
        "" => 1,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        "T" => 1 << 40,
        _ => return None,
    };
    value.checked_mul(factor)
}

fn is_disk_key(key: &str) -> bool {
    ["scsi", "virtio", "sata", "ide", "efidisk", "tpmstate"]
        .iter()
        .any(|prefix| {
            key.strip_prefix(prefix)
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        || key
            .strip_prefix("unused")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

const CLOUDINIT_KEYS: [&str; 8] = [
    "ipconfig0",
    "nameserver",
    "searchdomain",
    "ciuser",
    "cipassword",
    "ciupgrade",
    "sshkeys",
    "cicustom",
];

/// `POST /nodes/{node}/storage/{storage}/content`: allocate a volume for a
/// VM (`vmid`, `filename`, `size`, `format`); answers the volid. Checked
/// as the API schema says: `Datastore.AllocateSpace` on the storage. Not
/// observed live: the message for an existing name.
async fn volume_alloc(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, storage)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let mut state = lock(&state);
    let form = form(&body);
    let acl = format!("/storage/{storage}");
    let name = match reach(
        &state,
        token(request_token).as_ref(),
        &node,
        Some((&acl, &["Datastore.AllocateSpace"])),
    ) {
        Ok(name) => name,
        Err(failure) => return failure.into_response(),
    };
    let (Some(vmid), Some(filename), Some(size)) = (
        form_value(&form, "vmid"),
        form_value(&form, "filename"),
        form_value(&form, "size").and_then(size_bytes),
    ) else {
        return pve_error(400, "Parameter verification failed.\n");
    };
    if !filename.starts_with(&format!("vm-{vmid}-")) {
        return pve_error(500, &format!("illegal name '{filename}' - should be 'vm-{vmid}-*'\n"));
    }
    let volid = format!("{storage}:{filename}");
    if state.volumes.iter().any(|v| v.node == name && v.volid == volid) {
        return pve_error(500, &format!("volume '{volid}' already exists\n"));
    }
    state.volumes.push(VolumeSpec {
        node: name,
        storage,
        volid: volid.clone(),
        size,
        format: form_value(&form, "format").unwrap_or("raw").to_owned(),
        label: None,
    });
    data(Value::String(volid))
}

/// `DELETE /nodes/{node}/storage/{storage}/content/{volume}`: a task.
/// Needs `Datastore.Allocate` on the storage or `VM.Config.Disk` on the
/// volume's VM, as for listing it.
async fn volume_free(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, storage, volume)): Path<(String, String, String)>,
) -> Response {
    let mut state = lock(&state);
    let Some(token) = token(request_token) else {
        return pve_error(401, "");
    };
    let name = match reach(&state, Some(&token), &node, None) {
        Ok(name) => name,
        Err(failure) => return failure.into_response(),
    };
    let volid = if volume.contains(':') {
        volume
    } else {
        format!("{storage}:{volume}")
    };
    let acl = format!("/storage/{storage}");
    let allowed = has_privilege(&token, &acl, "Datastore.Allocate")
        || volume_owner(&volid).is_some_and(|vmid| state.allows(&token, &format!("/vms/{vmid}"), "VM.Config.Disk"));
    if !allowed {
        return forbidden(&acl, "Datastore.Allocate");
    }
    let before = state.volumes.len();
    state.volumes.retain(|v| !(v.node == name && v.volid == volid));
    if state.volumes.len() == before {
        return pve_error(500, &format!("volume '{volid}' does not exist\n"));
    }
    let owner = volume_owner(&volid).unwrap_or(0);
    task(&mut state, &name, "imgdel", owner, &token.id)
}

/// `POST /nodes/{node}/qemu/{vmid}/config`: like `PUT`, but any modelled
/// key, and answered with a task. Each key needs its own privilege, as PVE
/// checks them: disks `VM.Config.Disk` (and the volume must exist on the
/// node), Cloud-Init keys `VM.Config.Cloudinit` (`cicustom` also
/// `Datastore.AllocateSpace` on each snippet storage), options
/// `VM.Config.Options`.
async fn vm_config_post(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, vmid)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let mut state = lock(&state);
    let Some(token) = token(request_token) else {
        return pve_error(401, "");
    };
    let form = form(&body);
    let (vmid_number, node_name) = match reach_vm(&state, Some(&token), &node, &vmid, &["VM.Audit"]) {
        Ok(vm) => (vm.vmid, vm.node.clone()),
        Err(failure) => return failure.into_response(),
    };
    let acl = format!("/vms/{vmid_number}");
    for (key, value) in &form {
        let keys: Vec<&str> = if key == "delete" {
            value.split(',').collect()
        } else {
            vec![key.as_str()]
        };
        for key in keys {
            let privilege = if key == "digest" {
                continue;
            } else if is_disk_key(key) {
                "VM.Config.Disk"
            } else if CLOUDINIT_KEYS.contains(&key) {
                "VM.Config.Cloudinit"
            } else if OPTION_KEYS.contains(&key) {
                "VM.Config.Options"
            } else {
                return pve_error(501, &format!("pve-sim: config key '{key}' not modelled"));
            };
            if !state.allows(&token, &acl, privilege) {
                return forbidden(&acl, privilege);
            }
        }
        if key == "cicustom" {
            for part in value.split(',') {
                let storage = part
                    .split_once('=')
                    .and_then(|(_, v)| v.split_once(':'))
                    .map(|(s, _)| s);
                if let Some(storage) = storage {
                    let path = format!("/storage/{storage}");
                    if !has_privilege(&token, &path, "Datastore.AllocateSpace") {
                        return forbidden(&path, "Datastore.AllocateSpace");
                    }
                }
            }
        }
        if is_disk_key(key) {
            let volume = value.split(',').next().unwrap_or_default();
            if !state.volumes.iter().any(|v| v.node == node_name && v.volid == volume) {
                return pve_error(500, &format!("{key}: volume '{volume}' does not exist\n"));
            }
        }
    }
    let Some(vm) = state.vms.get_mut(&vmid_number) else {
        return pve_error(500, "VM vanished");
    };
    if let Some(digest) = form_value(&form, "digest")
        && digest != config_digest(&vm.config)
    {
        return pve_error(
            500,
            "detected modified configuration - file changed by other user? Try again.\n",
        );
    }
    for (key, value) in &form {
        match key.as_str() {
            "digest" => {}
            "delete" => {
                for key in value.split(',') {
                    vm.config.remove(key);
                }
            }
            key => {
                vm.config.insert(key.to_owned(), toml::Value::String(value.clone()));
            }
        }
    }
    task(&mut state, &node_name, "qmconfig", vmid_number, &token.id)
}

/// The volume of a disk config value, with the VMID renamed.
fn cloned_volume(value: &str, source: u32, target: u32, storage: &str) -> Option<(String, String)> {
    let (volume, options) = value.split_once(',').map_or((value, ""), |(v, o)| (v, o));
    let (_, name) = volume.split_once(':')?;
    let name = name.replacen(&format!("vm-{source}-"), &format!("vm-{target}-"), 1);
    if !name.starts_with(&format!("vm-{target}-")) {
        return None;
    }
    let volid = format!("{storage}:{name}");
    let value = if options.is_empty() {
        volid.clone()
    } else {
        format!("{volid},{options}")
    };
    Some((volid, value))
}

/// `POST /nodes/{node}/qemu/{vmid}/clone` with `newid`, `name`, `full`,
/// `storage`, `pool`, `description`: a full clone, made at once and
/// reported by a `qmclone` task. Needs `VM.Clone` on the source,
/// `VM.Allocate` on the pool (or `/vms/<newid>` without one) and
/// `Datastore.AllocateSpace` on the storage. Not observed live: the
/// messages of the refusals.
async fn vm_clone(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, vmid)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let mut state = lock(&state);
    let Some(token) = token(request_token) else {
        return pve_error(401, "");
    };
    let form = form(&body);
    let (source, node_name, source_config) = match reach_vm(&state, Some(&token), &node, &vmid, &["VM.Clone"]) {
        Ok(vm) => (vm.vmid, vm.node.clone(), vm.config.clone()),
        Err(failure) => return failure.into_response(),
    };
    let Some(newid) = form_value(&form, "newid").and_then(|v| v.parse::<u32>().ok()) else {
        return pve_error(400, "Parameter verification failed.\n");
    };
    let pool = form_value(&form, "pool").map(str::to_owned);
    let target_acl = pool
        .as_ref()
        .map_or_else(|| format!("/vms/{newid}"), |p| format!("/pool/{p}"));
    if !has_privilege(&token, &target_acl, "VM.Allocate") {
        return forbidden(&target_acl, "VM.Allocate");
    }
    if let Some(pool) = &pool
        && !state.pools.iter().any(|p| &p.poolid == pool)
    {
        return pve_error(500, &format!("pool '{pool}' does not exist\n"));
    }
    if state.vms.contains_key(&newid) {
        return pve_error(
            500,
            &format!("unable to create VM {newid}: config file already exists\n"),
        );
    }
    let storage = form_value(&form, "storage").map(str::to_owned);
    if let Some(storage) = &storage {
        let path = format!("/storage/{storage}");
        if !has_privilege(&token, &path, "Datastore.AllocateSpace") {
            return forbidden(&path, "Datastore.AllocateSpace");
        }
    }
    let mut config = toml::Table::new();
    let mut volumes = Vec::new();
    for (key, value) in &source_config {
        let text = value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string());
        if is_disk_key(key) && !key.starts_with("unused") {
            let disk_storage = text.split(':').next().unwrap_or_default().to_owned();
            let target_storage = storage.clone().unwrap_or(disk_storage);
            if let Some((volid, value)) = cloned_volume(&text, source, newid, &target_storage) {
                let size = text
                    .split(',')
                    .find_map(|o| o.strip_prefix("size="))
                    .and_then(size_bytes)
                    .unwrap_or(0);
                let label = state
                    .volumes
                    .iter()
                    .find(|v| v.volid == text.split(',').next().unwrap_or_default())
                    .and_then(|v| v.label.clone());
                volumes.push(VolumeSpec {
                    node: node_name.clone(),
                    storage: target_storage,
                    volid,
                    size,
                    format: "raw".to_owned(),
                    label,
                });
                config.insert(key.clone(), toml::Value::String(value));
                continue;
            }
        }
        config.insert(key.clone(), value.clone());
    }
    if let Some(name) = form_value(&form, "name") {
        config.insert("name".to_owned(), toml::Value::String(name.to_owned()));
    }
    if let Some(description) = form_value(&form, "description") {
        config.insert("description".to_owned(), toml::Value::String(description.to_owned()));
    }
    state.volumes.extend(volumes);
    state.vms.insert(
        newid,
        VmSpec {
            vmid: newid,
            node: node_name.clone(),
            status: VmState::Stopped,
            config,
            guest: None,
            acpi: true,
            clone_of: Some(source),
        },
    );
    if let Some(pool) = pool
        && let Some(spec) = state.pools.iter_mut().find(|p| p.poolid == pool)
    {
        spec.vms.push(newid);
    }
    task(&mut state, &node_name, "qmclone", source, &token.id)
}

#[derive(Deserialize)]
struct DestroyQuery {
    purge: Option<u8>,
    #[serde(rename = "destroy-unreferenced-disks")]
    unreferenced: Option<u8>,
}

/// `DELETE /nodes/{node}/qemu/{vmid}`: needs `VM.Allocate`; a running VM is
/// refused. Its disks (attached and `unusedN`) go with it, with
/// `destroy-unreferenced-disks=1` also every other volume named for it;
/// `purge=1` takes it out of its pool.
async fn vm_destroy(
    State(state): State<Shared>,
    request_token: Option<axum::Extension<TokenSpec>>,
    Path((node, vmid)): Path<(String, String)>,
    Query(query): Query<DestroyQuery>,
) -> Response {
    let mut state = lock(&state);
    let Some(token) = token(request_token) else {
        return pve_error(401, "");
    };
    let (vmid_number, node_name, status, config) = match reach_vm(&state, Some(&token), &node, &vmid, &["VM.Allocate"])
    {
        Ok(vm) => (vm.vmid, vm.node.clone(), vm.status, vm.config.clone()),
        Err(failure) => return failure.into_response(),
    };
    if status == VmState::Running {
        return pve_error(500, &format!("VM {vmid_number} is running - destroy failed\n"));
    }
    let disks: Vec<String> = config
        .iter()
        .filter(|(key, _)| is_disk_key(key))
        .filter_map(|(_, value)| value.as_str())
        .map(|value| value.split(',').next().unwrap_or_default().to_owned())
        .collect();
    let unreferenced = query.unreferenced == Some(1);
    state.volumes.retain(|v| {
        let attached = v.node == node_name && disks.contains(&v.volid);
        let owned = unreferenced && volume_owner(&v.volid) == Some(vmid_number);
        !(attached || owned)
    });
    state.vms.remove(&vmid_number);
    if query.purge == Some(1) {
        for pool in &mut state.pools {
            pool.vms.retain(|id| *id != vmid_number);
        }
    }
    task(&mut state, &node_name, "qmdestroy", vmid_number, &token.id)
}

/// Percent-decoding of Proxmox's `sshkeys`.
fn decode_sshkeys(value: &str) -> String {
    percent_decode_str(value).decode_utf8_lossy().into_owned()
}

/// The first start of a clone with a HomeStack vendor snippet: what the
/// snippet and Proxmox's Cloud-Init would make of the source's guest
/// (decision 1 = A). The hostname is the VM name; `eth0` gets the MAC of
/// `net0` and the address of `ipconfig0`; root gets `sshkeys`. The home is
/// the disk whose serial is `HS_HOME_<vmid>`: a blank one is formatted with
/// that label by the `create` snippet (and gets the keys), a labelled one is
/// mounted; `keep` leaves a blank one unmounted. The account is the
/// source's only regular one.
fn boot_clone(state: &mut ClusterState, vmid: u32) {
    let Some(vm) = state.vms.get(&vmid) else { return };
    let (Some(source), None) = (vm.clone_of, &vm.guest) else {
        return;
    };
    let text = |key: &str| vm.config.get(key).and_then(toml::Value::as_str).map(str::to_owned);
    let Some(cicustom) = text("cicustom") else { return };
    let create = cicustom.contains("homestack-vendor-create.yaml");
    if !create && !cicustom.contains("homestack-vendor-keep.yaml") {
        return;
    }
    let Some(mut guest) = state.vms.get(&source).and_then(|gold| gold.guest.clone()) else {
        return;
    };
    let name = text("name").unwrap_or_default();
    let mac = text("net0")
        .and_then(|net| {
            net.split(',')
                .next()
                .and_then(|m| m.split_once('='))
                .map(|(_, m)| m.to_ascii_lowercase())
        })
        .unwrap_or_default();
    let address = text("ipconfig0")
        .and_then(|ip| {
            ip.split(',')
                .find_map(|part| part.strip_prefix("ip=").map(str::to_owned))
        })
        .unwrap_or_default();
    let keys = text("sshkeys").map(|k| decode_sshkeys(&k)).unwrap_or_default();
    let label = format!("HS_HOME_{vmid}");
    let home = vm
        .config
        .iter()
        .filter(|(key, _)| is_disk_key(key))
        .find_map(|(key, value)| {
            let value = value.as_str()?;
            value
                .split(',')
                .any(|o| o == format!("serial={label}"))
                .then(|| (key.clone(), value.split(',').next().unwrap_or_default().to_owned()))
        });
    let node = vm.node.clone();
    let user = guest
        .accounts()
        .into_iter()
        .find(|a| (1000..65534).contains(&a.uid))
        .map(|a| a.name);

    guest.hostname = name;
    guest.interfaces = vec![InterfaceSpec {
        name: "eth0".to_owned(),
        mac,
        ipv4: if address.is_empty() { Vec::new() } else { vec![address] },
    }];
    guest.filesystems.retain(|fs| fs.mountpoint == "/");
    if !keys.is_empty() {
        guest
            .files
            .insert("/root/.ssh/authorized_keys".to_owned(), keys.clone());
    }
    if let (Some((slot, volid)), Some(user)) = (home, user) {
        let volume = state.volumes.iter_mut().find(|v| v.node == node && v.volid == volid);
        if let Some(volume) = volume {
            if volume.label.is_none() && create {
                volume.label = Some(label.clone());
                guest.files.insert(format!("/home/{user}/.ssh/authorized_keys"), keys);
            }
            if volume.label.as_deref() == Some(label.as_str()) {
                guest.filesystems.push(FilesystemSpec {
                    mountpoint: format!("/home/{user}"),
                    fstype: "ext4".to_owned(),
                    device: "sdb".to_owned(),
                    slot,
                    label: Some(label),
                    size: volume.size,
                    used: 0,
                    avail: volume.size,
                });
            }
        }
    }
    if guest.users.is_empty() {
        guest.users.push(UserEntry::Name("root".to_owned()));
    }
    guest.cloud_id = Some("nocloud".to_owned());
    if let Some(vm) = state.vms.get_mut(&vmid) {
        vm.guest = Some(guest);
    }
}
