//! `homestack-usage`: charts of Claude Code and Codex usage from Prometheus.
//!
//! | Variable | Meaning |
//! | --- | --- |
//! | `HOMESTACK_PROMETHEUS_URL` | Prometheus URL, `http` only, e.g. `http://prom:9090`; any path is ignored |
//! | `HOMESTACK_HEARTBEAT_SECONDS` | Serving-path heartbeat interval, 1 to 3600 seconds; default 60 |
//!
//! The page queries Prometheus through this service, which forwards only the
//! read-only instant, range and metric-name endpoints. LiteLLM OpenAI prices
//! are downloaded once at startup; restart the container to refresh or retry.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use hs_model::health::{HeartbeatObservation, Liveness, LivenessStatus};
use tracing_subscriber::EnvFilter;

const PROBE_PATH: &str = "/health/heartbeat";
const PAGE: &str = include_str!("index.html");
const PRICING_URL: &str = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
/// Prometheus endpoints the page may read; nothing else is forwarded.
const FORWARDED: [&str; 3] = ["/api/v1/query", "/api/v1/query_range", "/api/v1/label/__name__/values"];

#[derive(Clone)]
struct AppState {
    prometheus: url::Url,
    client: reqwest::Client,
    heartbeat: Arc<Heartbeat>,
    page: Arc<str>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let filter = EnvFilter::try_from_env("HOMESTACK_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    let (prometheus, interval, listen) = match settings(|name| std::env::var(name).ok()) {
        Ok(settings) => settings,
        Err(error) => {
            tracing::error!(%error, "invalid configuration");
            return ExitCode::from(2);
        }
    };
    let client = match reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::error!(%error, "cannot create the Prometheus client");
            return ExitCode::from(1);
        }
    };
    let heartbeat = Arc::new(Heartbeat::new(interval));
    // Fetch once per process. Restart the container to refresh prices.
    let pricing = match fetch_pricing(&client).await {
        Ok(prices) => Some(prices),
        Err(error) => {
            tracing::warn!(%error, "Codex cost estimates unavailable; restart to retry pricing download");
            None
        }
    };
    // Embed the immutable startup rates in the page; the browser never
    // downloads a separate catalog on reload or on filter changes.
    let prices = pricing.map_or_else(
        || "null".to_owned(),
        |prices| prices.to_string().replace('<', "\\u003c"),
    );
    let page: Arc<str> = Arc::from(PAGE.replace("__HOMESTACK_PRICING__", &prices).replace(
        "__HOMESTACK_USAGE_VERSION__",
        option_env!("HOMESTACK_USAGE_VERSION").unwrap_or("development"),
    ));
    let mut app = Router::new()
        .route("/", get(index))
        .route("/health/live", get(health))
        .route("/health/ready", get(health))
        .route(PROBE_PATH, get(|| async { StatusCode::NO_CONTENT }))
        .route(hs_service_status::PATH, get(service_status));
    for path in FORWARDED {
        app = app.route(path, get(move |state, query| forward(state, path, query)));
    }
    let app = app.with_state(AppState {
        prometheus: prometheus.clone(),
        client,
        heartbeat: Arc::clone(&heartbeat),
        page,
    });
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%error, listen = %listen, "cannot listen");
            return ExitCode::from(1);
        }
    };
    let heartbeat_task = tokio::spawn(heartbeat.run(listen));
    tracing::info!(listen = %listen, prometheus = %prometheus, "homestack-usage started");
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    heartbeat_task.abort();
    if let Err(error) = served {
        tracing::error!(%error, "server failed");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

async fn fetch_pricing(client: &reqwest::Client) -> Result<serde_json::Value, String> {
    let catalog = client
        .get(PRICING_URL)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| error.to_string())?
        .json::<serde_json::Value>()
        .await
        .map_err(|error| error.to_string())?;
    let entries = catalog.as_object().ok_or("Invalid LiteLLM pricing catalog")?;
    let fields = [
        "input_cost_per_token",
        "output_cost_per_token",
        "cache_read_input_token_cost",
        "cache_creation_input_token_cost",
    ];
    let prices: serde_json::Map<String, serde_json::Value> = entries
        .iter()
        .filter(|(_, value)| value["litellm_provider"] == "openai")
        .map(|(name, value)| {
            let rates: serde_json::Map<String, serde_json::Value> = fields
                .iter()
                .filter_map(|field| value.get(*field).map(|rate| ((*field).to_owned(), rate.clone())))
                .collect();
            (name.clone(), serde_json::Value::Object(rates))
        })
        .collect();
    if prices.is_empty() {
        return Err("No OpenAI prices in LiteLLM catalog".to_owned());
    }
    tracing::info!(models = prices.len(), "downloaded startup pricing catalog");
    Ok(serde_json::Value::Object(prices))
}

async fn index(State(state): State<AppState>) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Html(state.page.to_string())).into_response()
}

fn settings(env: impl Fn(&str) -> Option<String>) -> Result<(url::Url, Duration, SocketAddr), String> {
    let raw = env("HOMESTACK_PROMETHEUS_URL").ok_or("HOMESTACK_PROMETHEUS_URL is required")?;
    let mut prometheus = url::Url::parse(&raw).map_err(|error| format!("HOMESTACK_PROMETHEUS_URL: {error}"))?;
    prometheus.set_path("");
    prometheus.set_query(None);
    prometheus.set_fragment(None);
    if prometheus.scheme() != "http" || prometheus.host().is_none() {
        return Err("HOMESTACK_PROMETHEUS_URL must be an http URL with a host".to_owned());
    }
    let seconds = match env("HOMESTACK_HEARTBEAT_SECONDS") {
        None => 60,
        Some(value) => value
            .parse()
            .ok()
            .filter(|seconds| (1..=3600).contains(seconds))
            .ok_or("HOMESTACK_HEARTBEAT_SECONDS must be an integer from 1 to 3600")?,
    };
    let listen = env("HOMESTACK_LISTEN")
        .ok_or("HOMESTACK_LISTEN is required")?
        .parse()
        .map_err(|error| format!("HOMESTACK_LISTEN: {error}"))?;
    Ok((prometheus, Duration::from_secs(seconds), listen))
}

/// Forward one read-only Prometheus request, keeping its status and body.
async fn forward(State(state): State<AppState>, path: &'static str, RawQuery(query): RawQuery) -> Response {
    let mut target = state.prometheus.clone();
    target.set_path(path);
    target.set_query(query.as_deref());
    let response = match state.client.get(target).send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%error, path, "Prometheus is unreachable");
            return (StatusCode::BAD_GATEWAY, "Prometheus is unreachable").into_response();
        }
    };
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    match response.bytes().await {
        Ok(body) => (status, [(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(error) => {
            tracing::warn!(%error, path, "Prometheus response failed");
            (StatusCode::BAD_GATEWAY, "Prometheus response failed").into_response()
        }
    }
}

/// Liveness and readiness: the serving-path heartbeat is fresh. Prometheus
/// availability is reported by the page, not as this service's health.
async fn health(State(state): State<AppState>) -> Response {
    let health = state.heartbeat.snapshot();
    let status = match health.status {
        LivenessStatus::Pass => StatusCode::OK,
        LivenessStatus::Fail => StatusCode::SERVICE_UNAVAILABLE,
    };
    (
        status,
        [(header::CONTENT_TYPE, "application/health+json")],
        axum::Json(health),
    )
        .into_response()
}

async fn service_status(State(state): State<AppState>) -> impl IntoResponse {
    let status = match state.heartbeat.snapshot().status {
        LivenessStatus::Pass => hs_service_status::Status::Pass,
        LivenessStatus::Fail => hs_service_status::Status::Fail,
    };
    (
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(hs_service_status::report(status)),
    )
}

struct Heartbeat {
    interval: Duration,
    progress: Mutex<(Instant, u64)>,
}

impl Heartbeat {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            progress: Mutex::new((Instant::now(), 0)),
        }
    }

    fn snapshot(&self) -> Liveness {
        let (last, count) = *self.progress.lock().unwrap_or_else(PoisonError::into_inner);
        let age = last.elapsed();
        let status = if age > self.interval.saturating_mul(3) {
            LivenessStatus::Fail
        } else {
            LivenessStatus::Pass
        };
        let check = |value, unit: Option<&str>| {
            vec![HeartbeatObservation {
                status,
                observed_value: value,
                observed_unit: unit.map(str::to_owned),
            }]
        };
        Liveness {
            status,
            checks: [
                ("heartbeat:age".to_owned(), check(age.as_secs(), Some("s"))),
                ("heartbeat:count".to_owned(), check(count, None)),
            ]
            .into(),
        }
    }

    /// A successful probe passes through the same listener and workers as the page.
    async fn run(self: Arc<Self>, mut address: SocketAddr) {
        if address.ip().is_unspecified() {
            address.set_ip(match address.ip() {
                IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
            });
        }
        let Ok(client) = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(self.interval.min(Duration::from_secs(2)))
            .build()
        else {
            return;
        };
        let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + self.interval, self.interval);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let url = format!("http://{address}{PROBE_PATH}");
        loop {
            timer.tick().await;
            if client
                .get(&url)
                .send()
                .await
                .is_ok_and(|response| response.status() == reqwest::StatusCode::NO_CONTENT)
            {
                let mut progress = self.progress.lock().unwrap_or_else(PoisonError::into_inner);
                progress.0 = Instant::now();
                progress.1 = progress.1.saturating_add(1);
            }
        }
    }
}

async fn shutdown_signal() {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminate => {}
    }
}
