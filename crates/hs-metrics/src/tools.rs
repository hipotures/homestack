use crate::prometheus::{self, Endpoint, Prometheus};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

type Params = Vec<(String, String)>;
type Prepared = Result<(Params, u32), String>;

/// Unix seconds (finite numeric value) or an RFC3339 timestamp string.
#[derive(Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Timestamp {
    Seconds(f64),
    Text(String),
}
impl Timestamp {
    fn seconds(&self) -> Result<f64, String> {
        let value = match self {
            Self::Seconds(n) => *n,
            Self::Text(s) => s
                .parse::<f64>()
                .or_else(|_| {
                    chrono::DateTime::parse_from_rfc3339(s)
                        .map(|d| d.timestamp() as f64 + f64::from(d.timestamp_subsec_nanos()) / 1e9)
                })
                .map_err(|_| "Timestamp must be Unix seconds or RFC3339")?,
        };
        if !value.is_finite() {
            return Err("Timestamp must be finite".into());
        }
        Ok(value)
    }
    fn text(&self) -> String {
        match self {
            Self::Seconds(n) => n.to_string(),
            Self::Text(s) => s.clone(),
        }
    }
}

/// Positive numeric seconds, or a Prometheus duration such as 30s, 5m, 1h30m.
#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Step {
    Seconds(f64),
    Text(String),
}
impl Step {
    fn seconds(&self) -> Result<f64, String> {
        let n = match self {
            Self::Seconds(n) => *n,
            Self::Text(s) => match s.parse::<f64>() {
                Ok(n) => n,
                Err(_) => duration_seconds(s)?,
            },
        };
        if !n.is_finite() || n <= 0.0 {
            return Err("step must be finite and greater than zero".into());
        }
        Ok(n)
    }
    fn text(&self) -> String {
        match self {
            Self::Seconds(n) => n.to_string(),
            Self::Text(s) => s.clone(),
        }
    }
}

fn duration_seconds(s: &str) -> Result<f64, String> {
    if s.is_empty() || s.len() > 128 {
        return Err("Invalid Prometheus duration".into());
    }
    let mut rest = s;
    let mut total = 0.0;
    let mut previous = f64::INFINITY;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err("Invalid Prometheus duration".into());
        }
        let number: f64 = rest[..digits].parse().map_err(|_| "Invalid duration number")?;
        rest = &rest[digits..];
        let (unit, scale) = [
            ("ms", 0.001),
            ("y", 31_536_000.0),
            ("w", 604_800.0),
            ("d", 86_400.0),
            ("h", 3600.0),
            ("m", 60.0),
            ("s", 1.0),
        ]
        .into_iter()
        .find(|(unit, _)| rest.starts_with(unit))
        .ok_or("Invalid Prometheus duration unit")?;
        if scale >= previous {
            return Err("Duration units must be unique and in descending order".into());
        }
        previous = scale;
        total += number * scale;
        rest = &rest[unit.len()..];
    }
    Ok(total)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Query {
    /// Arbitrary PromQL expression, at most 32 KiB.
    query: String,
    time: Option<Timestamp>,
    /// Maximum returned series, 1..1000; default 100.
    #[schemars(range(min = 1, max = 1000))]
    limit: Option<u32>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Range {
    /// Arbitrary PromQL expression, at most 32 KiB.
    query: String,
    start: Timestamp,
    end: Timestamp,
    step: Step,
    /// Maximum returned series, 1..1000; default 100.
    #[schemars(range(min = 1, max = 1000))]
    limit: Option<u32>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Series {
    /// Nonempty array of series selectors, at most 64 and 32 KiB total.
    #[schemars(length(min = 1, max = 64))]
    matchers: Vec<String>,
    start: Option<Timestamp>,
    end: Option<Timestamp>,
    /// Maximum results, 1..1000; default 100.
    #[schemars(range(min = 1, max = 1000))]
    limit: Option<u32>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Labels {
    /// Optional series selectors, at most 64 and 32 KiB total.
    matchers: Option<Vec<String>>,
    start: Option<Timestamp>,
    end: Option<Timestamp>,
    /// Maximum results, 1..1000; default 100.
    #[schemars(range(min = 1, max = 1000))]
    limit: Option<u32>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LabelValues {
    /// Label name matching [a-zA-Z_][a-zA-Z0-9_]*. Use __name__ for metric names.
    label: String,
    matchers: Option<Vec<String>>,
    start: Option<Timestamp>,
    end: Option<Timestamp>,
    /// Maximum results, 1..1000; default 100.
    #[schemars(range(min = 1, max = 1000))]
    limit: Option<u32>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    /// Optional metric name to filter metadata.
    metric: Option<String>,
    /// Maximum metrics, 1..1000; default 100.
    #[schemars(range(min = 1, max = 1000))]
    limit: Option<u32>,
    /// Maximum metadata entries per metric, 1..1000; default 100.
    #[schemars(range(min = 1, max = 1000))]
    limit_per_metric: Option<u32>,
}

#[derive(Clone)]
pub struct Metrics {
    upstream: Prometheus,
    tool_router: ToolRouter<Self>,
}
impl Metrics {
    pub fn new(upstream: Prometheus) -> Self {
        Self {
            upstream,
            tool_router: Self::tool_router(),
        }
    }
    async fn execute(&self, endpoint: Endpoint, prepared: Prepared) -> CallToolResult {
        match prepared {
            Ok((params, limit)) => self.upstream.request(endpoint, params, limit).await,
            Err(e) => prometheus::error(e),
        }
    }
}
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Metrics {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("metrics", option_env!("HOMESTACK_METRICS_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))))
            .with_instructions("Generic read-only Prometheus exploration. Arbitrary PromQL: YES. Arbitrary Prometheus HTTP access: NO. Writes/admin: NO. Discover metrics using prometheus_label_values with label=\"__name__\". Results preserve original Prometheus JSON. Limits: 20s timeout, 16 concurrent upstream requests, 32 KiB expressions, 8 MiB responses, 11000 range points per series, result limit 100 default/1000 maximum.")
    }
}
#[tool_router]
impl Metrics {
    #[tool(
        description = "Execute arbitrary PromQL as an instant query through the fixed read-only /api/v1/query endpoint. Preserve the original Prometheus JSON envelope, values, labels, warnings and infos.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn prometheus_query(&self, Parameters(p): Parameters<Query>) -> CallToolResult {
        let prepared = (|| {
            prometheus::bounded_text(&p.query)?;
            let limit = prometheus::bounded_limit(p.limit)?;
            let mut params = vec![("query".into(), p.query), ("limit".into(), limit.to_string())];
            if let Some(t) = p.time {
                t.seconds()?;
                params.push(("time".into(), t.text()));
            }
            Ok((params, limit))
        })();
        self.execute(Endpoint::Query, prepared).await
    }
    #[tool(
        description = "Execute arbitrary PromQL over a range through fixed read-only /api/v1/query_range. At most 11000 evaluation points per series; long history is allowed with a sufficiently large step. Preserve original Prometheus JSON.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn prometheus_query_range(&self, Parameters(p): Parameters<Range>) -> CallToolResult {
        let prepared = (|| {
            prometheus::bounded_text(&p.query)?;
            let start = p.start.seconds()?;
            let end = p.end.seconds()?;
            if end < start {
                return Err("end must be greater than or equal to start".into());
            }
            let points = ((end - start) / p.step.seconds()?).floor() + 1.0;
            if !points.is_finite() || points > prometheus::MAX_RANGE_POINTS {
                return Err("Range exceeds 11000 points per series; increase step or narrow the range".into());
            }
            let limit = prometheus::bounded_limit(p.limit)?;
            Ok((
                vec![
                    ("query".into(), p.query),
                    ("start".into(), p.start.text()),
                    ("end".into(), p.end.text()),
                    ("step".into(), p.step.text()),
                    ("limit".into(), limit.to_string()),
                ],
                limit,
            ))
        })();
        self.execute(Endpoint::QueryRange, prepared).await
    }
    #[tool(
        description = "Discover series and their complete label sets through fixed read-only /api/v1/series. Selectors are sent as repeated match[] parameters. Requires at least one selector.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn prometheus_series(&self, Parameters(p): Parameters<Series>) -> CallToolResult {
        let prepared = if p.matchers.is_empty() {
            Err("matchers must be nonempty".into())
        } else {
            discovery(Some(p.matchers), p.start, p.end, p.limit)
        };
        self.execute(Endpoint::Series, prepared).await
    }
    #[tool(
        description = "Discover available label names through fixed read-only /api/v1/labels, optionally filtered by series selectors and timestamps.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn prometheus_label_names(&self, Parameters(p): Parameters<Labels>) -> CallToolResult {
        self.execute(Endpoint::LabelNames, discovery(p.matchers, p.start, p.end, p.limit))
            .await
    }
    #[tool(
        description = "Discover values for one label through fixed read-only /api/v1/label/{label}/values. Use label = \"__name__\" to discover available metric names. Optional selectors and timestamps filter discovery.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn prometheus_label_values(&self, Parameters(p): Parameters<LabelValues>) -> CallToolResult {
        self.execute(
            Endpoint::LabelValues(p.label),
            discovery(p.matchers, p.start, p.end, p.limit),
        )
        .await
    }
    #[tool(
        description = "Retrieve metric metadata through fixed read-only /api/v1/metadata. Optionally filter metric and bound metrics/entries per metric using limit/limit_per_metric. Preserve type, unit, help and all other upstream metadata fields.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn prometheus_metadata(&self, Parameters(p): Parameters<Metadata>) -> CallToolResult {
        let prepared = (|| {
            let limit = prometheus::bounded_limit(p.limit)?;
            let per_metric = prometheus::bounded_limit(p.limit_per_metric)?;
            let mut params = vec![
                ("limit".into(), limit.to_string()),
                ("limit_per_metric".into(), per_metric.to_string()),
            ];
            if let Some(metric) = p.metric {
                prometheus::bounded_text(&metric)?;
                params.push(("metric".into(), metric));
            }
            Ok((params, limit))
        })();
        self.execute(Endpoint::Metadata, prepared).await
    }
}

fn discovery(
    matchers: Option<Vec<String>>,
    start: Option<Timestamp>,
    end: Option<Timestamp>,
    limit: Option<u32>,
) -> Prepared {
    let limit = prometheus::bounded_limit(limit)?;
    let mut params = vec![("limit".into(), limit.to_string())];
    if let Some(matchers) = matchers {
        if matchers.len() > prometheus::MAX_MATCHERS
            || matchers.iter().map(String::len).sum::<usize>() > prometheus::MAX_QUERY_BYTES
        {
            return Err("At most 64 selectors and 32 KiB of selector text are allowed".into());
        }
        for matcher in matchers {
            prometheus::bounded_text(&matcher)?;
            params.push(("match[]".into(), matcher));
        }
    }
    if let (Some(start), Some(end)) = (&start, &end)
        && end.seconds()? < start.seconds()?
    {
        return Err("end must be greater than or equal to start".into());
    }
    for (name, t) in [("start", start), ("end", end)] {
        if let Some(t) = t {
            t.seconds()?;
            params.push((name.into(), t.text()));
        }
    }
    Ok((params, limit))
}
