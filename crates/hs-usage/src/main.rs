//! `homestack-usage`: charts of Claude Code and Codex usage from Prometheus.
//!
//! | Variable | Meaning |
//! | --- | --- |
//! | `HOMESTACK_PROMETHEUS_URL` | Prometheus URL, `http` only, e.g. `http://prom:9090`; any path is ignored |
//! | `HOMESTACK_CODEX_HISTORY_END` | Exclusive end of historical turn usage, Unix seconds; default 0 disables historical reads |
//! | `HOMESTACK_HEARTBEAT_SECONDS` | Serving-path heartbeat interval, 1 to 3600 seconds; default 60 |
//!
//! Purpose-built, shared dashboard datasets with local Datastar and ECharts assets.

mod dashboard;
mod datasets;
mod live;
mod planner;
mod prometheus;
mod registry;

use std::hash::{Hash, Hasher};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use hs_model::health::{HeartbeatObservation, Liveness, LivenessStatus};
use tracing_subscriber::EnvFilter;

const PROBE_PATH: &str = "/health/heartbeat";
const PAGE: &str = include_str!("index.html");
const PRICING_URL: &str = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
#[derive(Clone)]
struct AppState {
    dashboard: Arc<dashboard::Dashboard>,
    heartbeat: Arc<Heartbeat>,
    page: Bytes,
    shutdown: tokio::sync::watch::Receiver<bool>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let filter = EnvFilter::try_from_env("HOMESTACK_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    let (prometheus, interval, listen, history_end) = match settings(|name| std::env::var(name).ok()) {
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
    let page = Bytes::from(PAGE.replace(
        "__HOMESTACK_USAGE_VERSION__",
        option_env!("HOMESTACK_USAGE_VERSION").unwrap_or("development"),
    ));
    let dashboard = Arc::new(dashboard::Dashboard::new(
        prometheus::Prometheus::new(client, prometheus.clone()),
        pricing,
        history_end,
    ));
    let (shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
    let app = Router::new()
        .route("/", get(index))
        .route("/assets/{name}", get(asset))
        .route("/api/dashboard/events", get(live::events))
        .route("/api/dashboard/snapshot", get(live::snapshot))
        .route("/api/dashboard/export", get(live::export))
        .route("/health/live", get(health))
        .route("/health/ready", get(health))
        .route(PROBE_PATH, get(|| async { StatusCode::NO_CONTENT }))
        .route(hs_service_status::PATH, get(service_status))
        .layer(tower_http::compression::CompressionLayer::new())
        .with_state(AppState {
            dashboard,
            heartbeat: Arc::clone(&heartbeat),
            page,
            shutdown,
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
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            let _ = shutdown_tx.send(true);
        })
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
    ([(header::CACHE_CONTROL, "no-store")], Html(state.page)).into_response()
}

fn settings(env: impl Fn(&str) -> Option<String>) -> Result<(url::Url, Duration, SocketAddr, f64), String> {
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
    let history_end = env("HOMESTACK_CODEX_HISTORY_END")
        .map_or(Ok(0.0), |value| value.parse::<f64>())
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or("HOMESTACK_CODEX_HISTORY_END must be a finite, nonnegative Unix timestamp")?;
    Ok((prometheus, Duration::from_secs(seconds), listen, history_end))
}

/// Versioned vendor assets are embedded in the Alpine binary, including gzip representations.
async fn asset(Path(name): Path<String>, headers: HeaderMap) -> Response {
    let gzip = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|s| s.trim() == "gzip" || s.trim() == "gzip;q=1"));
    let (plain, compressed): (&'static [u8], &'static [u8]) = match name.as_str() {
        "datastar-1.0.4.js" => (
            include_bytes!("../assets/datastar-1.0.4.js"),
            include_bytes!("../assets/datastar-1.0.4.js.gz"),
        ),
        "echarts-6.1.0.min.js" => (
            include_bytes!("../assets/echarts-6.1.0.min.js"),
            include_bytes!("../assets/echarts-6.1.0.min.js.gz"),
        ),
        "dashboard.js" => (include_bytes!("../assets/dashboard.js"), &[]),
        "aiusage-echarts-theme.js" => (include_bytes!("../assets/aiusage-echarts-theme.js"), &[]),
        "aiusage-tokens.css" => (include_bytes!("../assets/aiusage-tokens.css"), &[]),
        "datastar-LICENSE.md" => (include_bytes!("../assets/datastar-LICENSE.md"), &[]),
        "echarts-LICENSE.txt" => (include_bytes!("../assets/echarts-LICENSE.txt"), &[]),
        "echarts-NOTICE.txt" => (include_bytes!("../assets/echarts-NOTICE.txt"), &[]),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let vendor = !compressed.is_empty();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    plain.hash(&mut hasher);
    let etag = format!("W/\"{:016x}\"", hasher.finish());
    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag);
    let mut response = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        Bytes::from_static(if gzip && vendor { compressed } else { plain }).into_response()
    };
    let headers = response.headers_mut();
    if let Ok(etag) = header::HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, etag);
    }
    headers.insert(
        header::CONTENT_TYPE,
        (if name.ends_with(".js") {
            "text/javascript; charset=utf-8"
        } else if name.ends_with(".css") {
            "text/css; charset=utf-8"
        } else {
            "text/plain; charset=utf-8"
        })
        .parse()
        .unwrap_or_else(|_| header::HeaderValue::from_static("text/javascript")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static(if vendor {
            "public, max-age=31536000, immutable"
        } else {
            "public, max-age=0, must-revalidate"
        }),
    );
    headers.insert(header::VARY, header::HeaderValue::from_static("Accept-Encoding"));
    if gzip && vendor {
        headers.insert(header::CONTENT_ENCODING, header::HeaderValue::from_static("gzip"));
    }
    response
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
