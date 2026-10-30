//! Agent adapters: all metric and accounting semantics live on the server.
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Instrument {
    pub name: String,
    pub agent: &'static str,
    pub measurement: &'static str,
    pub unit: &'static str,
    pub kind: Kind,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Counter,
    Response,
    History,
    /// Cumulative `_bucket` series read only by per-selection quantile queries.
    Histogram,
}

pub const DISCOVERY: &str = "{__name__=~\"claude_code_(token_usage_tokens|cost_usage_USD|session_count|active_time_seconds|commit_count|lines_of_code_count)_total|claude_code_api_request_(duration_ms|count|ttft_ms|ttft_count)|codex_(response_token_usage|(turn_(token_usage|e2e_duration_ms|ttft_duration_ms|ttfm_duration_ms)|thread_started|conversation_turn_count|tool_call).*_(sum|count|total)|(turn_(e2e|ttft|ttfm)|tool_call)_duration_ms.*_bucket)\"}";

pub fn instruments(names: &[String], history_end: f64) -> Vec<Instrument> {
    let mut out = Vec::new();
    for (name, measurement, unit) in [
        ("claude_code_token_usage_tokens_total", "tokens", "tokens"),
        ("claude_code_cost_usage_USD_total", "cost", "USD"),
        ("claude_code_session_count_total", "sessions", "count"),
        ("claude_code_active_time_seconds_total", "active", "seconds"),
        ("claude_code_commit_count_total", "commits", "count"),
        ("claude_code_lines_of_code_count_total", "lines", "count"),
    ] {
        if names.iter().any(|n| n == name) {
            out.push(Instrument {
                name: name.into(),
                agent: "claude",
                measurement,
                unit,
                kind: Kind::Counter,
            });
        }
    }
    // Delta gauges the Collector derives from Claude Code api_request events.
    for (name, measurement, unit) in [
        ("claude_code_api_request_duration_ms", "requestDuration", "milliseconds"),
        ("claude_code_api_request_count", "requestDuration", "observations"),
        ("claude_code_api_request_ttft_ms", "requestFirstToken", "milliseconds"),
        (
            "claude_code_api_request_ttft_count",
            "requestFirstToken",
            "observations",
        ),
    ] {
        if names.iter().any(|n| n == name) {
            out.push(Instrument {
                name: name.into(),
                agent: "claude",
                measurement,
                unit,
                kind: Kind::Response,
            });
        }
    }
    let mut add = |name: Option<&String>, measurement, unit, kind| {
        if let Some(name) = name {
            out.push(Instrument {
                name: name.clone(),
                agent: "codex",
                measurement,
                unit,
                kind,
            });
        }
    };
    let pick = |prefix: &str| {
        names
            .iter()
            .find(|n| n.starts_with(prefix) && n.ends_with("_total"))
            .or_else(|| names.iter().find(|n| n.starts_with(prefix) && n.ends_with("_sum")))
    };
    add(
        names.iter().find(|n| n.as_str() == "codex_response_token_usage"),
        "tokens",
        "tokens",
        Kind::Response,
    );
    if history_end > 0. {
        add(pick("codex_turn_token_usage"), "tokens", "tokens", Kind::History);
    }
    add(pick("codex_thread_started"), "sessions", "count", Kind::Counter);
    add(pick("codex_conversation_turn_count"), "turns", "count", Kind::Counter);
    add(
        names.iter().find(|n| n.as_str() == "codex_tool_call_total"),
        "tools",
        "count",
        Kind::Counter,
    );
    for (prefix, measurement) in [
        ("codex_turn_e2e_duration_ms", "turnDuration"),
        ("codex_turn_ttft_duration_ms", "firstToken"),
        ("codex_turn_ttfm_duration_ms", "firstOutput"),
        ("codex_tool_call_duration_ms", "toolDuration"),
    ] {
        add(
            names.iter().find(|n| n.starts_with(prefix) && n.ends_with("_sum")),
            measurement,
            "milliseconds",
            Kind::Counter,
        );
        add(
            names.iter().find(|n| n.starts_with(prefix) && n.ends_with("_count")),
            measurement,
            "observations",
            Kind::Counter,
        );
        add(
            names.iter().find(|n| n.starts_with(prefix) && n.ends_with("_bucket")),
            measurement,
            "buckets",
            Kind::Histogram,
        );
    }
    out
}
