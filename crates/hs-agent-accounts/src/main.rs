mod app;
mod db;
mod model;
mod process;
mod providers;
mod runtime;
mod telemetry;

use app::Context;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use model::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    path::PathBuf,
    sync::{Arc, PoisonError},
    time::Duration,
};

const VERSION: &str = match option_env!("AGENT_ACCOUNTS_VERSION") {
    Some(version) => version,
    None => "development",
};

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let status = match self.code.as_str() {
            "not_found" => StatusCode::NOT_FOUND,
            "busy" | "disabled" => StatusCode::CONFLICT,
            "unsupported_capability" => StatusCode::NOT_IMPLEMENTED,
            "invalid_input" | "invalid_id" | "confirmation_required" => StatusCode::BAD_REQUEST,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status, Json(json!({"error":self}))).into_response()
    }
}
fn setting(name: &str, default: u64, min: u64, max: u64) -> std::result::Result<u64, Box<dyn std::error::Error>> {
    let value = std::env::var(name)
        .ok()
        .map(|s| s.parse::<u64>())
        .transpose()?
        .unwrap_or(default);
    if !(min..=max).contains(&value) {
        return Err(format!("{name} must be {min}..{max}").into());
    }
    Ok(value)
}
#[tokio::main(worker_threads = 2)]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    rustix::process::umask(rustix::fs::Mode::from_bits_truncate(0o077));
    let root = PathBuf::from(std::env::var("AGENT_ACCOUNTS_HOME").unwrap_or_else(|_| "/home/app".into()));
    let otlp_endpoint = telemetry::endpoint()?;
    let ctx = Context::open(
        root,
        setting("AGENT_ACCOUNTS_INTERVAL_SECONDS", 300, 60, 3600)?,
        setting("AGENT_ACCOUNTS_PROBE_CONCURRENCY", 1, 1, 2)? as usize,
        setting("HOMESTACK_HEARTBEAT_SECONDS", 60, 1, 3600)?,
    )?;
    for provider in &ctx.providers {
        if let Some(runtime) = ctx.db.runtime(provider.id())? {
            let binary = ctx.binary(provider.id())?;
            process::bounded(
                &ctx.shutdown,
                15,
                runtime::verify_version(&ctx, provider.id(), &binary, &runtime.version),
            )
            .await?;
        }
    }
    let page = include_str!("index.html").replace("__AGENT_ACCOUNTS_VERSION__", VERSION);
    let app = Router::new()
        .route("/", get(move || async move { axum::response::Html(page) }))
        .route(
            "/favicon.ico",
            get(|| async { ([(header::CONTENT_TYPE, "image/x-icon")], include_bytes!("../assets/favicon.ico").as_slice()) }),
        )
        .route(
            "/favicon-32.png",
            get(|| async { ([(header::CONTENT_TYPE, "image/png")], include_bytes!("../assets/favicon-32.png").as_slice()) }),
        )
        .route(
            "/favicon-180.png",
            get(|| async { ([(header::CONTENT_TYPE, "image/png")], include_bytes!("../assets/favicon-180.png").as_slice()) }),
        )
        .route(
            "/favicon-192.png",
            get(|| async { ([(header::CONTENT_TYPE, "image/png")], include_bytes!("../assets/favicon-192.png").as_slice()) }),
        )
        .route(
            "/favicon-512.png",
            get(|| async { ([(header::CONTENT_TYPE, "image/png")], include_bytes!("../assets/favicon-512.png").as_slice()) }),
        )
        .route(
            "/manifest.webmanifest",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/manifest+json")],
                    r##"{"name":"Agent Accounts","icons":[{"src":"/favicon-192.png","sizes":"192x192","type":"image/png"},{"src":"/favicon-512.png","sizes":"512x512","type":"image/png"}]}"##,
                )
            }),
        )
        .route(
            "/tokens.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css")],
                    include_str!("../../hs-usage/assets/aiusage-tokens.css"),
                )
            }),
        )
        .route(
            "/app.js",
            get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], include_str!("app.js")) }),
        )
        .route(
            "/style.css",
            get(|| async { ([(header::CONTENT_TYPE, "text/css")], include_str!("style.css")) }),
        )
        .route("/api/v1/providers", get(providers))
        .route("/api/v1/profiles", get(profiles).post(create))
        .route("/api/v1/profiles/{id}", get(profile).patch(edit).delete(delete_profile))
        .route(
            "/api/v1/profiles/{id}/authentication",
            post(authenticate).get(authentication).delete(cancel_auth),
        )
        .route("/api/v1/profiles/{id}/authentication/code", post(submit_code))
        .route("/api/v1/profiles/{id}/limits", get(limits).post(probe))
        .route("/api/v1/profiles/{id}/history", get(history))
        .route("/api/v1/profiles/{id}/statistics", post(statistics))
        .route("/api/v1/runtimes", get(runtimes))
        .route("/api/v1/runtimes/{provider}/check", post(check_runtime))
        .route("/api/v1/runtimes/{provider}/upgrade", post(upgrade_runtime))
        .route("/api/v1/operations/{id}", get(operation).delete(cancel_operation))
        .route("/api/v1/operations/{id}/events", get(events))
        .route("/api/v1/health", get(api_health))
        .route("/api/v1/version", get(|| async { Json(json!({"version": VERSION})) }))
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route("/health/heartbeat", get(heartbeat))
        .route(hs_service_status::PATH, get(status))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn(browser_boundary))
        .with_state(ctx.clone());
    let listener =
        tokio::net::TcpListener::bind(std::env::var("HOMESTACK_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into()))
            .await?;
    let addr = listener.local_addr()?;
    if let Some(endpoint) = otlp_endpoint {
        tokio::spawn(telemetry::run(ctx.clone(), endpoint));
    }
    tokio::spawn(ctx.clone().scheduler());
    tokio::spawn(ctx.clone().heartbeat(addr));
    eprintln!("Agent Accounts listening on {addr}");
    let stop = ctx.clone();
    axum::serve(listener,app).with_graceful_shutdown(async move {
        let mut sig=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=async {if let Some(s)=sig.as_mut(){s.recv().await;}else{std::future::pending::<()>().await;}}=>{}};
        stop.shutdown.cancel();
    }).await?;
    for _ in 0..50 {
        if ctx
            .jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .all(|j| j.value.terminal())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}
async fn browser_boundary(req: axum::extract::Request, next: Next) -> Response {
    if !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        if req.headers().get("sec-fetch-site").is_some_and(|v| v == "cross-site") {
            return StatusCode::FORBIDDEN.into_response();
        }
        if let Some(origin) = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok());
            if reqwest::Url::parse(origin)
                .ok()
                .and_then(|u| {
                    u.host_str().map(|h| match u.port() {
                        Some(p) => format!("{h}:{p}"),
                        None => h.into(),
                    })
                })
                .as_deref()
                != host
            {
                return StatusCode::FORBIDDEN.into_response();
            }
        }
        if matches!(*req.method(), Method::POST | Method::PATCH | Method::DELETE)
            && !req
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.split(';').next() == Some("application/json"))
        {
            return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
        }
    }
    let api_request = req.uri().path().starts_with("/api/");
    let mut response = next.run(req).await;
    if api_request
        && response.status().is_client_error()
        && !response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|v| v == "application/json")
    {
        let status = response.status();
        response = (status,Json(json!({"error":{"code":"invalid_input","message":"Invalid API request; check the route, JSON body and input types"}}))).into_response();
    }
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    h.insert("x-content-type-options", header::HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", header::HeaderValue::from_static("no-referrer"));
    h.insert(header::CONTENT_SECURITY_POLICY,header::HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"));
    response
}
async fn providers(State(c): State<Arc<Context>>) -> Json<Value> {
    Json(json!(c.providers.iter().map(|p| p.description()).collect::<Vec<_>>()))
}
async fn profiles(State(c): State<Arc<Context>>) -> Result<Json<Vec<Profile>>> {
    Ok(Json(c.profiles()?))
}
async fn profile(State(c): State<Arc<Context>>, Path(id): Path<String>) -> Result<Json<Profile>> {
    let mut p = c.db.profile(&id)?;
    p.stale = c.stale(&p);
    Ok(Json(p))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    provider: String,
    display_name: String,
}
async fn create(State(c): State<Arc<Context>>, Json(v): Json<Create>) -> Result<(StatusCode, Json<Profile>)> {
    Ok((StatusCode::CREATED, Json(c.create(&v.provider, &v.display_name)?)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    enabled: bool,
}
async fn edit(State(c): State<Arc<Context>>, Path(id): Path<String>, Json(v): Json<Edit>) -> Result<Json<Profile>> {
    c.db.profile(&id)?;
    c.cancel_profile(&id);
    let lock = c.lock(&id);
    let _guard = lock.lock().await;
    let mut p = c.db.profile(&id)?;
    if p.deleting {
        return Err(Failure::new("busy", "Profile is being deleted"));
    }
    p.enabled = v.enabled;
    p.updated_at = now();
    c.db.save(&p, None)?;
    drop(_guard);
    if p.enabled {
        let _ = c.begin(&p, "probe")?;
    }
    Ok(Json(p))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delete {
    confirm: bool,
}
async fn delete_profile(
    State(c): State<Arc<Context>>,
    Path(id): Path<String>,
    Json(v): Json<Delete>,
) -> Result<StatusCode> {
    if !v.confirm {
        return Err(Failure::new(
            "confirmation_required",
            "Confirm permanent deletion of this profile and all its history",
        ));
    }
    c.db.profile(&id)?;
    c.cancel_profile(&id);
    let lock = c.lock(&id);
    let _guard = lock.lock().await;
    let mut p = c.db.profile(&id)?;
    p.deleting = true;
    c.db.save(&p, None)?;
    c.delete_files(&id)?;
    c.db.delete(&id)?;
    c.locks.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
    c.jobs
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|_, j| j.value.profile_id.as_deref() != Some(&id));
    Ok(StatusCode::NO_CONTENT)
}
fn accepted(o: Operation) -> (StatusCode, Json<Operation>) {
    (StatusCode::ACCEPTED, Json(o))
}
async fn authenticate(State(c): State<Arc<Context>>, Path(id): Path<String>) -> Result<(StatusCode, Json<Operation>)> {
    let p = c.db.profile(&id)?;
    Ok(accepted(c.begin(&p, "authentication")?))
}
async fn authentication(State(c): State<Arc<Context>>, Path(id): Path<String>) -> Result<Json<Value>> {
    let p = c.db.profile(&id)?;
    let op = c
        .jobs
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .values()
        .filter(|j| j.value.profile_id.as_deref() == Some(&id) && j.value.kind == "authentication")
        .max_by_key(|j| j.value.started_at)
        .map(|j| j.value.clone());
    Ok(Json(json!({"auth_status":p.auth_status,"operation":op})))
}
async fn cancel_auth(State(c): State<Arc<Context>>, Path(id): Path<String>) -> Result<StatusCode> {
    c.db.profile(&id)?;
    c.cancel_profile(&id);
    Ok(StatusCode::ACCEPTED)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizationCode {
    code: String,
}
async fn submit_code(
    State(c): State<Arc<Context>>,
    Path(id): Path<String>,
    Json(v): Json<AuthorizationCode>,
) -> Result<StatusCode> {
    c.db.profile(&id)?;
    c.submit_code(&id, v.code)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn probe(State(c): State<Arc<Context>>, Path(id): Path<String>) -> Result<(StatusCode, Json<Operation>)> {
    let p = c.db.profile(&id)?;
    if !p.enabled {
        return Err(Failure::new("disabled", "Enable the profile before checking usage"));
    }
    Ok(accepted(c.begin(&p, "probe")?))
}
async fn limits(State(c): State<Arc<Context>>, Path(id): Path<String>) -> Result<Json<Value>> {
    let p = c.db.profile(&id)?;
    Ok(Json(json!({"snapshot":p.snapshot,"stale":c.stale(&p),"error":p.error})))
}
async fn history(State(c): State<Arc<Context>>, Path(id): Path<String>) -> Result<Json<Vec<Snapshot>>> {
    c.db.profile(&id)?;
    Ok(Json(c.db.history(&id)?))
}
async fn statistics(State(c): State<Arc<Context>>, Path(id): Path<String>) -> Result<(StatusCode, Json<Operation>)> {
    let p = c.db.profile(&id)?;
    Ok(accepted(c.begin(&p, "statistics")?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Upgrade {
    version: String,
}
async fn check_runtime(
    State(c): State<Arc<Context>>,
    Path(provider): Path<String>,
) -> Result<(StatusCode, Json<Operation>)> {
    Ok(accepted(c.runtime_operation(&provider, None)?))
}
async fn upgrade_runtime(
    State(c): State<Arc<Context>>,
    Path(provider): Path<String>,
    Json(v): Json<Upgrade>,
) -> Result<(StatusCode, Json<Operation>)> {
    Ok(accepted(c.runtime_operation(&provider, Some(v.version))?))
}
async fn runtimes(State(c): State<Arc<Context>>) -> Result<Json<Vec<Runtime>>> {
    let mut rs = Vec::new();
    for provider in &c.providers {
        if let Some(runtime) = c.db.runtime(provider.id())? {
            rs.push(runtime);
        }
    }
    Ok(Json(rs))
}
async fn operation(State(c): State<Arc<Context>>, Path(id): Path<String>) -> Result<Json<Operation>> {
    Ok(Json(c.operation(&id)?))
}
async fn cancel_operation(State(c): State<Arc<Context>>, Path(id): Path<String>) -> Result<StatusCode> {
    let jobs = c.jobs.lock().unwrap_or_else(PoisonError::into_inner);
    let j = jobs
        .get(&id)
        .ok_or_else(|| Failure::new("not_found", "Operation not found"))?;
    j.cancel.cancel();
    Ok(StatusCode::ACCEPTED)
}
async fn events(
    State(c): State<Arc<Context>>,
    Path(id): Path<String>,
) -> Result<Sse<impl futures_util::Stream<Item = std::result::Result<Event, Infallible>>>> {
    c.operation(&id)?;
    let stream = futures_util::stream::unfold((c, id, false), |(c, id, done)| async move {
        if done {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        match c.operation(&id) {
            Ok(o) => {
                let done = o.terminal();
                let event = Event::default()
                    .event("operation")
                    .data(serde_json::to_string(&o).unwrap_or_default());
                Some((Ok(event), (c, id, done)))
            }
            Err(_) => None,
        }
    });
    Ok(Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}
async fn api_health(State(c): State<Arc<Context>>) -> Result<Json<Value>> {
    Ok(Json(
        json!({"database_ready":c.db.ready(),"profiles":c.profiles()?,"alerts":c.profiles()?.iter().map(|p|json!({"profile_id":p.id,"conditions":telemetry::conditions(&c,p)})).collect::<Vec<_>>()}),
    ))
}
fn health(c: &Context, ready: bool) -> Response {
    let (live, age, count) = c.liveness();
    let local_runtimes = c
        .providers
        .iter()
        .all(|p| p.bundled_version().is_none() || c.binary(p.id()).is_ok());
    let pass = live && (!ready || (c.db.ready() && local_runtimes));
    let status = if pass { "pass" } else { "fail" };
    (if pass {StatusCode::OK}else{StatusCode::SERVICE_UNAVAILABLE},[(header::CONTENT_TYPE,"application/health+json")],Json(json!({"status":status,"checks":{"heartbeat:age":[{"status":if live{"pass"}else{"fail"},"observedValue":age,"observedUnit":"s"}],"heartbeat:count":[{"status":if live{"pass"}else{"fail"},"observedValue":count}],"database:connection":[{"status":if c.db.ready(){"pass"}else{"fail"}}]}}))).into_response()
}
async fn live(State(c): State<Arc<Context>>) -> Response {
    health(&c, false)
}
async fn ready(State(c): State<Arc<Context>>) -> Response {
    health(&c, true)
}
async fn heartbeat(State(c): State<Arc<Context>>) -> StatusCode {
    if c.db.ready() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}
async fn status(State(c): State<Arc<Context>>) -> Json<hs_service_status::Report> {
    Json(hs_service_status::report(if c.liveness().0 {
        hs_service_status::Status::Pass
    } else {
        hs_service_status::Status::Fail
    }))
}
