//! Datastar SSE: full resync per connection, then coherent versioned changes.
use crate::{AppState, datasets::Selection};
use axum::{
    extract::{Query, State},
    http::{StatusCode, header},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
};
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use serde_json::json;
use std::{convert::Infallible, sync::Arc, time::Duration};
#[derive(Deserialize)]
pub struct Signals {
    datastar: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    agent: String,
    host: String,
    project: String,
    range: String,
    epoch: u64,
}
struct Connection {
    state: AppState,
    selection: Selection,
    epoch: u64,
    previous: Option<String>,
    first: bool,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
pub async fn events(State(state): State<AppState>, Query(signals): Query<Signals>) -> Response {
    if signals.datastar.len() > 2048 {
        return (StatusCode::BAD_REQUEST, "Request too large").into_response();
    }
    let request: Request = match serde_json::from_str(&signals.datastar) {
        Ok(r) => r,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid selection").into_response(),
    };
    let selection = Selection {
        agent: request.agent,
        host: request.host,
        project: request.project,
        range: request.range,
    };
    if let Err(error) = selection.validate() {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    let permit = match Arc::clone(&state.dashboard.connections).try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return (StatusCode::TOO_MANY_REQUESTS, "Dashboard connection limit reached").into_response(),
    };
    let connection = Connection {
        state,
        selection,
        epoch: request.epoch,
        previous: None,
        first: true,
        _permit: permit,
    };
    let stream = stream::unfold(connection, |mut c| async move {
        if *c.state.shutdown.borrow() {
            return None;
        }
        if !c.first {
            tokio::select! { _=c.state.shutdown.changed()=>return None, ()=tokio::time::sleep(Duration::from_secs(30))=>{} }
        }
        c.first = false;
        let result = tokio::select! {_=c.state.shutdown.changed()=>return None,result=c.state.dashboard.snapshot(c.selection.clone())=>result};
        let packet = match result {
            Ok(snapshot) => {
                let changes = if let Some(before) = &c.previous {
                    c.state.dashboard.changes(before, &snapshot).await
                } else {
                    None
                };
                let packet = if let Some(changes) = changes {
                    json!({"kind":"delta","epoch":c.epoch,"snapshot":snapshot.id,"base":c.previous,"selection":snapshot.selection,"changes":changes,"freshness":snapshot.freshness(),"hosts":snapshot.hosts,"projects":snapshot.projects})
                } else {
                    json!({"kind":"full","epoch":c.epoch,"data":snapshot.full()})
                };
                c.previous = Some(snapshot.id.clone());
                packet
            }
            Err(error) => {
                tracing::warn!(%error,"dashboard refresh failed");
                json!({"kind":"error","epoch":c.epoch,"message":error,"checked_at":crate::prometheus::now()})
            }
        };
        // A string is intentional: one atomic Datastar signal, with no nested merge-patch ambiguity.
        let event = Event::default()
            .event("datastar-patch-signals")
            .data(format!("signals {}", json!({"_packet":packet.to_string()})));
        Some((Ok::<_, Infallible>(event), c))
    });
    // Announce the running build before querying dependencies, including after reconnects.
    let version = Event::default().event("datastar-patch-signals").data(format!(
        "signals {}",
        json!({"_packet":json!({"kind":"version","version":crate::VERSION}).to_string()})
    ));
    let stream = stream::once(async move { Ok::<_, Infallible>(version) }).chain(stream);
    (
        [
            (header::CACHE_CONTROL, "no-cache, no-transform"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(10)).text("heartbeat")),
    )
        .into_response()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Export {
    snapshot: String,
    dataset: Option<String>,
}
pub async fn export(State(state): State<AppState>, Query(query): Query<Export>) -> Response {
    if query.snapshot.len() != 16 || query.dataset.as_ref().is_some_and(|d| d.len() > 64) {
        return (StatusCode::BAD_REQUEST, "Invalid export identifier").into_response();
    }
    match state.dashboard.export(&query.snapshot, query.dataset.as_deref()).await {
        Ok(value) => ([(header::CACHE_CONTROL, "no-store")], axum::Json(value)).into_response(),
        Err(error) => (StatusCode::CONFLICT, axum::Json(json!({"error":error}))).into_response(),
    }
}
pub async fn snapshot(State(state): State<AppState>, Query(selection): Query<Selection>) -> Response {
    if let Err(error) = selection.validate() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error":error}))).into_response();
    }
    match state.dashboard.snapshot(selection).await {
        Ok(value) => ([(header::CACHE_CONTROL, "no-store")], axum::Json(value.full())).into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, axum::Json(json!({"error":error}))).into_response(),
    }
}
