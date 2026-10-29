//! Fixed read-only API allowlist. Arbitrary PromQL is allowed; arbitrary HTTP is not.
use std::{sync::Arc, time::Duration};

use futures_util::StreamExt;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

pub const UPSTREAM_TIMEOUT_SECONDS: u64 = 20;
pub const MAX_CONCURRENT_REQUESTS: usize = 16;
pub const MAX_QUERY_BYTES: usize = 32 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RANGE_POINTS: f64 = 11_000.0;
pub const DEFAULT_LIMIT: u32 = 100;
pub const MAX_LIMIT: u32 = 1000;
pub const MAX_MATCHERS: usize = 64;

/// There is deliberately no caller-supplied method or URL variant.
pub enum Endpoint {
    Query,
    QueryRange,
    Series,
    LabelNames,
    LabelValues(String),
    Metadata,
}

#[derive(Clone)]
pub struct Prometheus {
    base: url::Url,
    client: reqwest::Client,
    permits: Arc<Semaphore>,
}

pub fn error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::structured_error(json!({"status":"error", "errorType":"metrics", "error":message.into()}))
}

impl Prometheus {
    pub fn new(raw: &str) -> Result<Self, String> {
        let mut base = url::Url::parse(raw).map_err(|_| "Invalid HOMESTACK_PROMETHEUS_URL")?;
        if base.scheme() != "http" || base.host().is_none() || !base.username().is_empty() || base.password().is_some()
        {
            return Err("HOMESTACK_PROMETHEUS_URL must be an HTTP URL with a host and without credentials".into());
        }
        base.set_path("/");
        base.set_query(None);
        base.set_fragment(None);
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(UPSTREAM_TIMEOUT_SECONDS))
            .build()
            .map_err(|_| "Cannot build HTTP client")?;
        Ok(Self {
            base,
            client,
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
        })
    }

    pub async fn request(&self, endpoint: Endpoint, params: Vec<(String, String)>, limit: u32) -> CallToolResult {
        let Ok(_permit) = self.permits.try_acquire() else {
            return error("All 16 upstream request slots are busy; retry later");
        };
        let mut url = self.base.clone();
        let path = match &endpoint {
            Endpoint::Query => "/api/v1/query".to_owned(),
            Endpoint::QueryRange => "/api/v1/query_range".to_owned(),
            Endpoint::Series => "/api/v1/series".to_owned(),
            Endpoint::LabelNames => "/api/v1/labels".to_owned(),
            Endpoint::LabelValues(label) => {
                if !valid_label(label) {
                    return error("Invalid label name: expected [a-zA-Z_][a-zA-Z0-9_]*");
                }
                format!("/api/v1/label/{label}/values")
            }
            Endpoint::Metadata => "/api/v1/metadata".to_owned(),
        };
        let metadata_limit = params
            .iter()
            .find(|(key, _)| key == "limit_per_metric")
            .and_then(|(_, value)| value.parse::<usize>().ok());
        url.set_path(&path);
        url.query_pairs_mut().extend_pairs(params);
        let response = match self.client.get(url).send().await {
            Ok(response) => response,
            Err(_) => return error("Prometheus request failed or exceeded the 20-second timeout"),
        };
        let status = response.status();
        if response.content_length().is_some_and(|n| n > MAX_RESPONSE_BYTES as u64) {
            return error("Prometheus response exceeds 8 MiB; narrow the query or reduce its limit");
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => return error("Prometheus body read failed or timed out"),
            };
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return error("Prometheus response exceeds 8 MiB; narrow the query or reduce its limit");
            }
            body.extend_from_slice(&chunk);
        }
        let value: Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => return error(format!("Prometheus returned invalid JSON (HTTP {status})")),
        };
        if !status.is_success() || value["status"] == "error" {
            // Preserve the original error envelope, including upstream errorType/error/warnings/infos.
            return CallToolResult::structured_error(value);
        }
        if value["status"] != "success" || value.get("data").is_none() {
            return error("Prometheus returned an invalid API envelope");
        }
        let count = match endpoint {
            Endpoint::Query | Endpoint::QueryRange => match value["data"]["resultType"].as_str() {
                Some("vector" | "matrix") => value["data"]["result"].as_array().map(Vec::len),
                // Scalar/string results are one sample tuple, not an array of series.
                _ => None,
            },
            Endpoint::Metadata => value["data"].as_object().map(serde_json::Map::len),
            _ => value["data"].as_array().map(Vec::len),
        };
        if count.is_some_and(|n| n > limit as usize) {
            return error("Prometheus ignored the requested result limit; narrow the query");
        }
        if let Some(per_metric) = metadata_limit
            && value["data"].as_object().is_some_and(|metrics| {
                metrics
                    .values()
                    .any(|entries| entries.as_array().is_some_and(|entries| entries.len() > per_metric))
            })
        {
            return error("Prometheus ignored limit_per_metric; narrow the metadata query");
        }
        CallToolResult::structured(value)
    }
}

fn valid_label(label: &str) -> bool {
    let mut chars = label.bytes();
    label.len() <= 1024
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

pub fn bounded_limit(limit: Option<u32>) -> Result<u32, String> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err("limit must be an integer from 1 to 1000 (default 100)".into());
    }
    Ok(limit)
}

pub fn bounded_text(value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > MAX_QUERY_BYTES {
        return Err("Query/selector text must be nonempty and at most 32 KiB".into());
    }
    Ok(())
}
