//! Bounded OTLP/HTTP JSON export to the existing HomeStack Collector.
use crate::{app::Context, model::*};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

type Labels = Vec<(&'static str, String)>;
fn attributes(labels: &Labels) -> Vec<Value> {
    labels
        .iter()
        .map(|(key, value)| json!({"key":key,"value":{"stringValue":value}}))
        .collect()
}
#[derive(Default)]
struct Metrics(BTreeMap<&'static str, Value>);
impl Metrics {
    fn gauge(&mut self, name: &'static str, unit: &str, value: Option<f64>, labels: &Labels, observed: i64) {
        let Some(value) = value.filter(|v| v.is_finite()) else {
            return;
        };
        let metric = self
            .0
            .entry(name)
            .or_insert_with(|| json!({"name":name,"unit":unit,"gauge":{"dataPoints":[]}}));
        if let Some(points) = metric["gauge"]["dataPoints"].as_array_mut() {
            points.push(json!({"attributes":attributes(labels),"timeUnixNano":format!("{}000000000",observed),"asDouble":value}));
        }
    }
    fn counter(&mut self, name: &'static str, value: u64, labels: &Labels, start: i64, observed: i64) {
        let metric = self.0.entry(name).or_insert_with(
            || json!({"name":name,"unit":"1","sum":{"aggregationTemporality":2,"isMonotonic":true,"dataPoints":[]}}),
        );
        if let Some(points) = metric["sum"]["dataPoints"].as_array_mut() {
            points.push(json!({"attributes":attributes(labels),"startTimeUnixNano":format!("{}000000000",start),"timeUnixNano":format!("{}000000000",observed),"asInt":value.to_string()}));
        }
    }
}
pub fn conditions(ctx: &Context, p: &Profile) -> Vec<&'static str> {
    if !p.enabled || p.deleting {
        return Vec::new();
    }
    let mut flags = Vec::new();
    match p.auth_status.as_str() {
        "authentication_expired" => flags.push("authentication_expired"),
        "unauthenticated" | "reauthentication_required" => flags.push("reauthentication_required"),
        _ => {}
    }
    if p.failures >= 3 {
        flags.push("repeated_probe_failures");
    }
    if p.error.as_ref().is_some_and(|e| e.code == "network_failure") {
        flags.push("provider_unreachable");
    }
    if ctx.stale(p) {
        flags.push("usage_stale");
    }
    flags
}
fn payload(ctx: &Context) -> Result<Value> {
    let mut metrics = Metrics::default();
    let time = now();
    for p in ctx.db.profiles()? {
        if p.deleting {
            continue;
        }
        let labels = vec![
            ("provider", p.provider.clone()),
            ("profile_id", p.id.clone()),
            ("scope", "account".into()),
        ];
        metrics.gauge(
            "agent_account_enabled",
            "1",
            Some(f64::from(u8::from(p.enabled))),
            &labels,
            time,
        );
        metrics.gauge(
            "agent_account_authenticated",
            "1",
            Some(f64::from(u8::from(p.auth_status == "authenticated"))),
            &labels,
            time,
        );
        metrics.gauge(
            "agent_account_last_success_timestamp_seconds",
            "s",
            p.last_success.map(|t| t as f64),
            &labels,
            time,
        );
        metrics.gauge(
            "agent_account_measurement_stale",
            "1",
            Some(f64::from(u8::from(ctx.stale(&p)))),
            &labels,
            time,
        );
        metrics.counter(
            "agent_account_probe_failures_total",
            p.total_failures,
            &labels,
            p.created_at,
            time,
        );
        metrics.gauge(
            "agent_account_probe_duration_seconds",
            "s",
            p.probe_duration_seconds,
            &labels,
            p.last_attempt.unwrap_or(time),
        );
        let active = conditions(ctx, &p);
        for condition in [
            "authentication_expired",
            "reauthentication_required",
            "repeated_probe_failures",
            "provider_unreachable",
            "usage_stale",
        ] {
            let mut ls = labels.clone();
            ls.push(("condition", condition.into()));
            metrics.gauge(
                "agent_account_alert",
                "1",
                Some(f64::from(u8::from(active.contains(&condition)))),
                &ls,
                time,
            );
        }
        if let Some(snapshot) = &p.snapshot {
            for window in &snapshot.limits {
                let mut ls = labels.clone();
                ls.push(("window", window.id.clone()));
                metrics.gauge(
                    "agent_account_limit_used_percent",
                    "%",
                    window.used_percent,
                    &ls,
                    snapshot.observed_at,
                );
                metrics.gauge(
                    "agent_account_limit_reset_timestamp_seconds",
                    "s",
                    window.resets_at.map(|t| t as f64),
                    &ls,
                    snapshot.observed_at,
                );
            }
        }
    }
    for provider in &ctx.providers {
        if let Some(runtime) = ctx.db.runtime(provider.id())? {
            let labels = vec![("provider", runtime.provider), ("version", runtime.version)];
            metrics.gauge("agent_account_runtime_version_info", "1", Some(1.0), &labels, time);
            metrics.gauge(
                "agent_account_runtime_verified_timestamp_seconds",
                "s",
                runtime.verified_at.map(|t| t as f64),
                &labels,
                time,
            );
            metrics.gauge(
                "agent_account_runtime_verification_failed",
                "1",
                Some(f64::from(u8::from(runtime.last_error.as_ref().is_some_and(|e| {
                    e.code == "runtime_verification_failure" || e.code == "verification_blocked"
                })))),
                &labels,
                time,
            );
            metrics.gauge(
                "agent_account_runtime_upgrade_failed",
                "1",
                Some(f64::from(u8::from(runtime.last_error.is_some()))),
                &labels,
                time,
            );
        }
    }
    let instance = std::fs::read_to_string("/etc/hostname")
        .unwrap_or_else(|_| "local".into())
        .trim()
        .to_owned();
    Ok(
        json!({"resourceMetrics":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"agent-accounts"}},{"key":"service.namespace","value":{"stringValue":"homestack"}},{"key":"service.instance.id","value":{"stringValue":instance}}]},"scopeMetrics":[{"scope":{"name":"homestack.agent_accounts","version":env!("CARGO_PKG_VERSION")},"metrics":metrics.0.into_values().collect::<Vec<_>>()}]}]}),
    )
}
pub async fn run(ctx: Arc<Context>, endpoint: String) {
    let Ok(client) = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .build()
    else {
        return;
    };
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {_=ctx.shutdown.cancelled()=>break,_=interval.tick()=>{}};
        let result = async {
            let value = payload(&ctx)?;
            let response = client
                .post(&endpoint)
                .json(&value)
                .send()
                .await
                .map_err(|_| Failure::new("telemetry_failure", "OTLP endpoint is unreachable"))?;
            if !response.status().is_success() {
                return Err(Failure::new("telemetry_failure", "OTLP endpoint rejected metrics"));
            }
            use futures_util::StreamExt;
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| Failure::new("telemetry_failure", "OTLP response failed"))?;
                if bytes.len() + chunk.len() > 65536 {
                    return Err(Failure::new("telemetry_failure", "OTLP response is too large"));
                }
                bytes.extend_from_slice(&chunk);
            }
            let body: Value = serde_json::from_slice(&bytes)
                .map_err(|_| Failure::new("telemetry_failure", "Invalid OTLP response"))?;
            let rejected = &body["partialSuccess"]["rejectedDataPoints"];
            if rejected.as_u64().is_some_and(|n| n > 0) || rejected.as_str().is_some_and(|n| n != "0") {
                return Err(Failure::new(
                    "telemetry_failure",
                    "Collector partially rejected metrics",
                ));
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            eprintln!("Metrics export cycle failed: {}", e.code);
        }
    }
}
pub fn endpoint() -> Result<Option<String>> {
    let Some(endpoint) = std::env::var("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    let url = reqwest::Url::parse(&endpoint).map_err(|_| Failure::new("invalid_input", "Invalid OTLP metrics URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/v1/metrics"
    {
        return Err(Failure::new(
            "invalid_input",
            "OTLP metrics URL must be an HTTP(S) /v1/metrics endpoint without credentials, query or fragment",
        ));
    }
    Ok(Some(endpoint))
}
