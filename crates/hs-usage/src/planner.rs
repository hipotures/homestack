//! Stable UTC buckets. Completed buckets are cached independently of live work.
use crate::{
    prometheus::{Prometheus, Source},
    registry::{Instrument, Kind},
};
use futures_util::{StreamExt, stream};
use std::{collections::BTreeMap, sync::Arc};
const RETENTION: &str = "600d";
const GROUPS: &str = "instrument,host_name,project,model,effort,type,token_type,tool,success";
#[derive(Clone, Debug)]
pub struct Record {
    pub instrument: String,
    pub labels: BTreeMap<String, String>,
    pub time: u64,
    pub value: f64,
}
pub struct Block {
    pub records: Vec<Record>,
    pub sources: Vec<Arc<Source>>,
    pub fetched: u64,
}
fn named(expression: &str) -> String {
    format!("label_replace({expression}, \"instrument\", \"$1\", \"__name__\", \"(.*)\")")
}
fn selector(metrics: &[&Instrument]) -> String {
    format!(
        "{{__name__=~\"{}\"}}",
        metrics.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join("|")
    )
}
/// Delta gauge sums per bucket. `sum_over_time` drops `__name__`, so each
/// instrument's name is restored as the `instrument` label.
fn response_sums(metrics: &[&Instrument], seconds: u64) -> String {
    let parts: Vec<String> = metrics
        .iter()
        .map(|m| {
            format!(
                "label_replace(sum_over_time({}[{seconds}s]), \"instrument\", \"{}\", \"\", \"\")",
                selector(&[m]),
                m.name
            )
        })
        .collect();
    format!("sum by ({GROUPS}) ({})", parts.join(" or "))
}
fn samples(source: &Source) -> Vec<(BTreeMap<String, String>, u64, f64)> {
    let mut out = Vec::new();
    for row in source.data["result"].as_array().into_iter().flatten() {
        let labels: BTreeMap<String, String> = row["metric"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.into())))
            .collect();
        let pairs = row["values"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![row["value"].clone()]);
        for pair in pairs {
            if let (Some(t), Some(v)) = (pair[0].as_f64(), pair[1].as_str().and_then(|s| s.parse::<f64>().ok()))
                && v.is_finite()
            {
                out.push((labels.clone(), t as u64, v));
            }
        }
    }
    out
}
fn record(mut labels: BTreeMap<String, String>, time: u64, value: f64) -> Record {
    let instrument = labels
        .remove("instrument")
        .or_else(|| labels.remove("__name__"))
        .unwrap_or_default();
    labels.remove("__name__");
    Record {
        instrument,
        labels,
        time,
        value,
    }
}
pub struct Window {
    pub start: u64,
    pub boundary: u64,
    pub step: u64,
    pub now: u64,
    pub history_end: f64,
    pub budget: Arc<std::sync::atomic::AtomicUsize>,
}
pub async fn block(
    prom: &Prometheus,
    registry: &[Instrument],
    window: Window,
    tokens_only: bool,
) -> Result<Block, String> {
    let Window {
        start,
        boundary,
        step,
        now,
        history_end,
        budget,
    } = window;
    let mut jobs = Vec::new();
    // Closed history is refreshed at most every six hours for late ingestion.
    for kind in [Kind::Counter, Kind::Response, Kind::History] {
        if kind == Kind::History && start as f64 >= history_end {
            continue;
        }
        let metrics: Vec<_> = registry
            .iter()
            .filter(|m| m.kind == kind && (!tokens_only || m.measurement == "tokens"))
            .collect();
        if metrics.is_empty() {
            continue;
        }
        let sel = selector(&metrics);
        if boundary > start {
            let query = match kind {
                // Histograms are read only by per-selection quantile queries.
                Kind::Histogram => continue,
                Kind::Response => response_sums(&metrics, step),
                Kind::Counter => {
                    let end = named(&format!("last_over_time({sel}[{step}s])"));
                    let before = named(&format!("last_over_time({sel}[{RETENTION}] offset {step}s)"));
                    format!("sum by ({GROUPS}) (clamp_min({end} - ({before} or {end} * 0), 0))")
                }
                Kind::History => {
                    let current = named(&format!("last_over_time({sel}[{RETENTION}])"));
                    let frozen = named(&format!("last_over_time({sel}[{RETENTION}] @ {})", history_end - 0.001));
                    let end = format!(
                        "(({current} and on() (vector(time()) < {history_end})) or ({frozen} and on() (vector(time()) >= {history_end})))"
                    );
                    let before = named(&format!("last_over_time({sel}[{RETENTION}] offset {step}s)"));
                    format!(
                        "sum by ({GROUPS}) (clamp_min({end} - ({before} or {end} * 0), 0)) and on() (vector(time() - {step}) < {history_end})"
                    )
                }
            };
            let params = vec![
                ("query".into(), query),
                ("start".into(), (start + step).to_string()),
                ("end".into(), boundary.to_string()),
                ("step".into(), step.to_string()),
                ("timeout".into(), "20s".into()),
            ];
            jobs.push(async move { prom.read("/api/v1/query_range", params, 21600).await }.boxed());
        }
        if now > boundary {
            match kind {
                Kind::Histogram => continue,
                Kind::Response => {
                    let query = response_sums(&metrics, now - boundary);
                    jobs.push(async move { prom.instant(query, now as f64, 30).await }.boxed());
                }
                Kind::Counter | Kind::History => {
                    // Auxiliary projections reuse the main view's per-process counter endpoints.
                    let sel = if kind == Kind::Counter {
                        selector(&registry.iter().filter(|m| m.kind == Kind::Counter).collect::<Vec<_>>())
                    } else {
                        sel
                    };
                    if kind == Kind::History && boundary as f64 >= history_end {
                        continue;
                    }
                    let end = if kind == Kind::History {
                        (now as f64).min(history_end - 0.001)
                    } else {
                        now as f64
                    };
                    let current = named(&format!(
                        "last_over_time({sel}[{}ms])",
                        ((end - boundary as f64) * 1000.).round().max(1.) as u64
                    ));
                    let ttl = if kind == Kind::History && end < now as f64 {
                        21600
                    } else {
                        30
                    };
                    jobs.push(async move { prom.instant(current, end, ttl).await }.boxed());
                    let query = named(&format!("last_over_time({sel}[{RETENTION}])"));
                    jobs.push(async move { prom.instant(query, boundary as f64, 21600).await }.boxed());
                }
            }
        }
    }
    let mut results = stream::iter(jobs).buffered(4);
    let mut sources = Vec::new();
    while let Some(result) = results.next().await {
        let source = result?;
        if budget.fetch_add(source.bytes, std::sync::atomic::Ordering::Relaxed) + source.bytes > 8 * 1024 * 1024 {
            return Err("This calculation exceeds the bounded history budget; historical coverage is unavailable at this cardinality".into());
        }
        sources.push(source);
    }
    let mut records = Vec::new();
    // Baselines retain process identity until after subtraction; aggregating first loses resets/attribution.
    let mut baselines: BTreeMap<String, f64> = BTreeMap::new();
    for source in &sources {
        if source
            .params
            .iter()
            .any(|(k, v)| k == "time" && v == &boundary.to_string())
        {
            for (labels, _, v) in samples(source) {
                baselines.insert(serde_json::to_string(&labels).unwrap_or_default(), v);
            }
        }
    }
    for source in &sources {
        let is_range = source.data["resultType"] == "matrix";
        let baseline = source
            .params
            .iter()
            .any(|(k, v)| k == "time" && v == &boundary.to_string());
        if baseline {
            continue;
        }
        for (labels, t, v) in samples(source) {
            let name = labels.get("instrument").map(String::as_str).unwrap_or("");
            let Some(instrument) = registry.iter().find(|m| m.name == name) else {
                continue;
            };
            if tokens_only && instrument.measurement != "tokens" {
                continue;
            }
            let value = if !is_range && instrument.kind != Kind::Response {
                (v - baselines
                    .get(&serde_json::to_string(&labels).unwrap_or_default())
                    .copied()
                    .unwrap_or(0.))
                .max(0.)
            } else {
                v
            };
            records.push(record(
                labels,
                if is_range { t.saturating_sub(step) } else { boundary },
                value,
            ));
        }
    }
    let fetched = sources.iter().map(|s| s.fetched).min().unwrap_or(now);
    Ok(Block {
        records,
        sources,
        fetched,
    })
}
/// Bucket-approximated p50/p95 of a duration histogram for one selection, in seconds.
/// Shared blocks are selection-independent and filtered in Rust, but quantiles cannot be
/// combined after the fact, so these small queries carry the host/project matchers.
pub async fn quantiles(
    prom: &Prometheus,
    histogram: &Instrument,
    matchers: &str,
    range: (u64, u64, u64),
    now: u64,
) -> Result<serde_json::Value, String> {
    let (start, boundary, step) = range;
    let name = &histogram.name;
    let quantile = |q: f64, window: String| {
        format!("histogram_quantile({q}, sum by (le) (increase({name}{{{matchers}}}[{window}])))")
    };
    // The bucket bounds come along so that quantiles in the +Inf bucket can be marked as capped.
    let total = format!(
        "label_replace({}, \"q\", \"p50\", \"\", \"\") or label_replace({}, \"q\", \"p95\", \"\", \"\") or label_replace(count by (le) (last_over_time({name}{{{matchers}}}[{}s])), \"q\", \"bound\", \"\", \"\")",
        quantile(0.5, format!("{}s", now - start)),
        quantile(0.95, format!("{}s", now - start)),
        now - start
    );
    let timeline = vec![
        ("query".into(), quantile(0.5, format!("{step}s"))),
        ("start".into(), (start + step).to_string()),
        ("end".into(), boundary.to_string()),
        ("step".into(), step.to_string()),
        ("timeout".into(), "20s".into()),
    ];
    let (total, timeline) = futures_util::try_join!(
        prom.instant(total, now as f64, 30),
        prom.read("/api/v1/query_range", timeline, 21600)
    )?;
    let mut value = serde_json::Map::new();
    let mut top = 0f64;
    for (labels, _, v) in samples(&total) {
        match labels.get("q").map(String::as_str) {
            Some("bound") => {
                let le = labels.get("le").and_then(|le| le.parse::<f64>().ok()).unwrap_or(0.);
                if le.is_finite() {
                    top = top.max(le);
                }
            }
            Some(q) => {
                value.insert(q.into(), (v / 1000.).into());
            }
            None => {}
        }
    }
    // histogram_quantile reports the largest finite bound for quantiles above it.
    for q in ["p50", "p95"] {
        let capped = value
            .get(q)
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|v| top > 0. && v * 1000. >= top);
        value.insert(format!("{q}Capped"), capped.into());
    }
    // Closed buckets only; each point is stamped with its bucket start like the shared blocks.
    let points: Vec<_> = samples(&timeline)
        .into_iter()
        .map(|(_, t, v)| (t.saturating_sub(step), v / 1000.))
        .collect();
    value.insert("p50Timeline".into(), serde_json::json!(points));
    Ok(serde_json::Value::Object(value))
}
use futures_util::FutureExt;
