//! Stateless, generic read-only Prometheus MCP service. No UI or authentication.
mod prometheus;
mod tools;

use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use hs_model::health::{HeartbeatObservation, Liveness, LivenessStatus};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

const PROBE_PATH: &str = "/health/heartbeat";
const MAX_MCP_REQUEST_BYTES: usize = 128 * 1024;
#[derive(Clone)]
struct AppState {
    heartbeat: Arc<Heartbeat>,
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let upstream = prometheus::Prometheus::new(
        &std::env::var("HOMESTACK_PROMETHEUS_URL").map_err(|_| "HOMESTACK_PROMETHEUS_URL is required")?,
    )?;
    let listen: SocketAddr = std::env::var("HOMESTACK_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:80".into())
        .parse()?;
    let seconds: u64 = std::env::var("HOMESTACK_HEARTBEAT_SECONDS")
        .unwrap_or_else(|_| "60".into())
        .parse()?;
    if !(1..=3600).contains(&seconds) {
        return Err("HOMESTACK_HEARTBEAT_SECONDS must be 1..3600".into());
    }
    let heartbeat = Arc::new(Heartbeat::new(Duration::from_secs(seconds)));
    let handler = tools::Metrics::new(upstream);
    // Hosts are operator-selected (LAN and Pangolin). Browser Origins are denied;
    // Pangolin owns external authentication. No CORS or browser session support.
    let mut config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .disable_allowed_hosts()
        .enforce_origin_validation();
    config.max_request_body_bytes = MAX_MCP_REQUEST_BYTES;
    let mcp = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let app = Router::new()
        .nest_service("/mcp", mcp)
        .route("/health/live", get(health))
        .route("/health/ready", get(health))
        .route(PROBE_PATH, get(|| async { StatusCode::NO_CONTENT }))
        .route(hs_service_status::PATH, get(service_status))
        .with_state(AppState {
            heartbeat: heartbeat.clone(),
        });
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let task = tokio::spawn(heartbeat.run(listener.local_addr()?));
    eprintln!("Metrics listening on {}", listener.local_addr()?);
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    task.abort();
    result?;
    Ok(())
}

/// Liveness and readiness: the serving-path heartbeat is fresh. Prometheus
/// availability is reported by MCP tool errors, not as process health.
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

    /// A successful probe passes through the same listener and workers as MCP requests.
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
