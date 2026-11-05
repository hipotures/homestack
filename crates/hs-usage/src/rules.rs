//! Prometheus recording rules that pre-aggregate usage into one-minute increments.
//!
//! The rules mirror the planner's accounting: counter increments subtract the last value
//! before the minute (a new series counts in full), delta gauges are summed, and frozen
//! Codex history stops at the configured end. A delayed group (`query_offset`) lets late
//! samples arrive before a minute is aggregated; the dashboard reads newer data raw.
//! `hs_usage:evaluated` marks every aggregated minute with the rules version, so gaps
//! (Prometheus downtime, missed evaluations, a changed rule set) fall back to raw reads.

/// Labels kept by the aggregates; the planner groups raw reads by the same set.
pub const GROUPS: &str = "instrument,host_name,project,model,effort,type,token_type,tool,success";
pub const INCREASE: &str = "hs_usage:increase_1m";
pub const BUCKETS: &str = "hs_usage:bucket_increase_1m";
pub const ACTIVE: &str = "hs_usage:active_1m";
pub const EVALUATED: &str = "hs_usage:evaluated";
const LOOKBACK: &str = "600d";
const COUNTERS: &str = "claude_code_(token_usage_tokens|cost_usage_USD|session_count|active_time_seconds|commit_count|lines_of_code_count)_total|codex_(thread_started|conversation_turn_count).*_(total|sum)|codex_tool_call_total|codex_(turn_(e2e|ttft|ttfm)|tool_call)_duration_ms.*_(sum|count)";
const HISTORY: &str = "codex_turn_token_usage.*_(total|sum)";
const HISTOGRAMS: &str = "codex_(turn_(e2e|ttft|ttfm)|tool_call)_duration_ms.*_bucket";
const RESPONSES: [&str; 9] = [
    "agy_token_usage",
    "agy_invocations",
    "agy_tools",
    "agy_turns",
    "claude_code_api_request_duration_ms",
    "claude_code_api_request_count",
    "claude_code_api_request_ttft_ms",
    "claude_code_api_request_ttft_count",
    "codex_response_token_usage",
];

fn named(expression: &str) -> String {
    format!("label_replace({expression}, \"instrument\", \"$1\", \"__name__\", \"(.*)\")")
}
/// Per-series increase over the last minute; a series new in the lookback counts in full.
fn counter(regex: &str) -> String {
    let sel = format!("{{__name__=~\"{regex}\"}}");
    let end = named(&format!("last_over_time({sel}[1m])"));
    let before = named(&format!("last_over_time({sel}[{LOOKBACK}] offset 1m)"));
    format!("clamp_min({end} - ({before} or {end} * 0), 0)")
}
fn history(history_end: f64) -> String {
    let sel = format!("{{__name__=~\"{HISTORY}\"}}");
    let current = named(&format!("last_over_time({sel}[{LOOKBACK}])"));
    let frozen = named(&format!("last_over_time({sel}[{LOOKBACK}] @ {})", history_end - 0.001));
    let end = format!(
        "(({current} and on() (vector(time()) < {history_end})) or ({frozen} and on() (vector(time()) >= {history_end})))"
    );
    let before = named(&format!("last_over_time({sel}[{LOOKBACK}] offset 1m)"));
    format!("(clamp_min({end} - ({before} or {end} * 0), 0) and on() (vector(time() - 60) < {history_end}))")
}
fn response(name: &str) -> String {
    format!("label_replace(sum_over_time({{__name__=\"{name}\"}}[1m]), \"instrument\", \"{name}\", \"\", \"\")")
}

/// Rule expressions without the group configuration, in evaluation order.
fn expressions(history_end: f64) -> Vec<(&'static str, String)> {
    let mut increase = vec![
        format!("sum by ({GROUPS}) ({}) > 0", counter(COUNTERS)),
        format!(
            "sum by ({GROUPS}) ({}) > 0",
            RESPONSES.iter().map(|n| response(n)).collect::<Vec<_>>().join(" or ")
        ),
    ];
    let mut codex_tokens = vec![format!("{} > 0", response("codex_response_token_usage"))];
    if history_end > 0. {
        increase.push(format!("sum by ({GROUPS}) ({}) > 0", history(history_end)));
        codex_tokens.push(format!("{} > 0", history(history_end)));
    }
    let agent = |name: &str, parts: &[String]| {
        format!(
            "label_replace(count by (host_name, project) ({}), \"agent\", \"{name}\", \"\", \"\")",
            parts.join(" or ")
        )
    };
    let active = [
        agent(
            "claude",
            &[format!("{} > 0", counter("claude_code_token_usage_tokens_total"))],
        ),
        agent("codex", &codex_tokens),
        agent("antigravity", &[format!("{} > 0", response("agy_token_usage"))]),
    ];
    vec![
        (
            INCREASE,
            increase
                .into_iter()
                .map(|e| format!("({e})"))
                .collect::<Vec<_>>()
                .join(" or "),
        ),
        (
            BUCKETS,
            format!(
                "sum by (instrument, le, host_name, project) ({}) > 0",
                counter(HISTOGRAMS)
            ),
        ),
        (ACTIVE, active.join(" or ")),
    ]
}

/// Stable identity of the rule set: a changed rule never counts as coverage.
pub fn version(history_end: f64) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (record, expression) in expressions(history_end) {
        for byte in record.bytes().chain([0]).chain(expression.bytes()).chain([0]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

/// Validates a Prometheus duration such as `1h` or `90m`, between one minute and one day.
pub fn offset(value: &str) -> Result<&str, String> {
    let (number, unit) = value.split_at(value.len().saturating_sub(1));
    let seconds = number
        .parse::<u64>()
        .ok()
        .and_then(|n| match unit {
            "m" => n.checked_mul(60),
            "h" => n.checked_mul(3600),
            _ => None,
        })
        .ok_or("The offset must be minutes or hours, such as 90m or 1h")?;
    if !(60..=86400).contains(&seconds) {
        return Err("The offset must be between 1m and 24h".into());
    }
    Ok(value)
}

/// The rules file for `rule_files`. JSON strings are valid YAML double-quoted scalars.
pub fn render(history_end: f64, query_offset: &str) -> String {
    let mut out = format!(
        "# Generated by homestack-usage {}. Recording rules for the AI Usage dashboard.\ngroups:\n  - name: homestack-ai-usage\n    interval: 1m\n    query_offset: {query_offset}\n    rules:\n",
        crate::VERSION
    );
    for (record, expression) in expressions(history_end) {
        out.push_str(&format!(
            "      - record: {record}\n        expr: {}\n",
            serde_json::Value::from(expression)
        ));
    }
    out.push_str(&format!(
        "      - record: {EVALUATED}\n        expr: vector(1)\n        labels:\n          rules: \"{}\"\n",
        version(history_end)
    ));
    out
}
