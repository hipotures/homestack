//! Comparable activity estimates from occupied five-minute token-usage windows.
use crate::{
    datasets::{Availability, Dataset, Selection, Series, TimeRange},
    prometheus::{Prometheus, Source},
    registry::{Instrument, Kind},
};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

const SLOT: u64 = 300;

pub fn unavailable(selection: &Selection, range: &TimeRange, error: String) -> Dataset {
    let mut dataset = Dataset {
        id: "all.activeHours".into(),
        measurement: "activeHours".into(),
        agent: "all".into(),
        unit: "hours".into(),
        freshness_key: "activeHours".into(),
        scope: selection.clone(),
        range: range.clone(),
        state: Availability::Unavailable,
        series: Vec::new(),
        total: None,
        details: json!({"estimated":true,"window_seconds":SLOT}),
        warnings: vec![error],
        version: String::new(),
    };
    dataset.seal();
    dataset
}

pub async fn read(
    prom: &Prometheus,
    registry: &[Instrument],
    selection: &Selection,
    range: &TimeRange,
    timestamp: u64,
    history_end: f64,
) -> Result<(Dataset, Vec<Arc<Source>>), String> {
    let span = range.partial_start - range.start;
    let step = match span {
        0..=3600 => SLOT,
        3601..=172800 => 3600,
        172801..=3888000 => 86400,
        3888001..=15552000 => 7 * 86400,
        _ => 30 * 86400,
    };
    // Only completed slots contribute; the newest observation can lag by five minutes.
    let start = range.start.div_ceil(SLOT) * SLOT;
    let end = timestamp / SLOT * SLOT;
    let boundary = start + (end.saturating_sub(start) / step) * step;
    let activity_range = TimeRange {
        start,
        end,
        step,
        partial_start: boundary,
    };
    let mut matchers = String::new();
    for (label, value) in [("host_name", &selection.host), ("project", &selection.project)] {
        if value != "*" {
            matchers.push_str(&format!(
                ",{label}=\"{}\"",
                value.replace('\\', "\\\\").replace('"', "\\\"")
            ));
        }
    }
    let mut agents = Vec::new();
    for agent in ["claude", "codex", "antigravity"] {
        if selection.agent != "all" && selection.agent != agent {
            continue;
        }
        let mut parts = Vec::new();
        for metric in registry
            .iter()
            .filter(|m| m.agent == agent && m.measurement == "tokens")
        {
            let sel = format!("{}{{{}}}", metric.name, matchers.trim_start_matches(','));
            let expression = match metric.kind {
                Kind::Response => format!("sum_over_time({sel}[300s])"),
                Kind::Counter | Kind::History => {
                    let current = format!("last_over_time({sel}[300s])");
                    let before = format!("last_over_time({sel}[600d] offset 300s)");
                    let current = if metric.kind == Kind::History {
                        let frozen = format!("last_over_time({sel}[600d] @ {})", history_end - 0.001);
                        format!(
                            "(({current} and on() (vector(time()) < {history_end})) or ({frozen} and on() (vector(time()) >= {history_end})))"
                        )
                    } else {
                        current
                    };
                    let delta = format!("clamp_min({current} - ({before} or {current} * 0), 0)");
                    if metric.kind == Kind::History {
                        format!("({delta} and on() (vector(time() - 300) < {history_end}))")
                    } else {
                        delta
                    }
                }
                Kind::Histogram => continue,
            };
            parts.push(format!("(sum({expression}) or vector(0))"));
        }
        if !parts.is_empty() {
            agents.push((agent, format!("({} > bool 0)", parts.join(" + "))));
        }
    }
    let mut dataset = unavailable(
        selection,
        &activity_range,
        "No token telemetry is available for this selection".into(),
    );
    if agents.is_empty() {
        return Ok((dataset, Vec::new()));
    }
    // Total is the union of agents, never a sum of overlapping wall-clock time.
    let union = format!(
        "({} > bool 0)",
        agents
            .iter()
            .map(|(_, expr)| expr.as_str())
            .collect::<Vec<_>>()
            .join(" + ")
    );
    agents.push(("all", union));
    let query = |seconds: u64| {
        agents
            .iter()
            .map(|(agent, expression)| {
                // Exclude the left boundary even when a subquery includes its first sample.
                format!(
                    "label_replace(sum_over_time(({expression})[{}s:300s]) / 12, \"agent\", \"{agent}\", \"\", \"\")",
                    seconds - 1
                )
            })
            .collect::<Vec<_>>()
            .join(" or ")
    };
    let mut sources = Vec::new();
    if boundary > start {
        sources.push(
            prom.read(
                "/api/v1/query_range",
                vec![
                    ("query".into(), query(step)),
                    ("start".into(), (start + step).to_string()),
                    ("end".into(), boundary.to_string()),
                    ("step".into(), step.to_string()),
                    ("timeout".into(), "20s".into()),
                ],
                21600,
            )
            .await?,
        );
    }
    if end > boundary {
        sources.push(prom.instant(query(end - boundary), end as f64, 30).await?);
    }
    let mut points = BTreeMap::<String, BTreeMap<u64, f64>>::new();
    for source in &sources {
        let matrix = source.data["resultType"] == "matrix";
        for row in source.data["result"].as_array().into_iter().flatten() {
            let Some(agent) = row["metric"]["agent"].as_str() else {
                continue;
            };
            let pairs = row["values"]
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![row["value"].clone()]);
            for pair in pairs {
                if let (Some(t), Some(v)) = (pair[0].as_f64(), pair[1].as_str().and_then(|v| v.parse::<f64>().ok()))
                    && v.is_finite()
                    && v >= 0.
                {
                    let t = if matrix {
                        (t as u64).saturating_sub(step)
                    } else {
                        boundary
                    };
                    points.entry(agent.into()).or_default().insert(t, v);
                }
            }
        }
    }
    dataset.series = points
        .into_iter()
        .map(|(agent, points)| Series {
            id: agent.clone(),
            label: agent.clone(),
            dimensions: [("agent".into(), agent)].into(),
            points: points.into_iter().collect(),
        })
        .collect();
    dataset.total = dataset
        .series
        .iter()
        .find(|s| s.id == "all")
        .map(|s| s.points.iter().map(|(_, v)| v).sum());
    dataset.state = if dataset.series.is_empty() {
        Availability::Missing
    } else {
        Availability::Available
    };
    dataset.warnings.clear();
    dataset.details = json!({"estimated":true,"window_seconds":SLOT,"through":end});
    dataset.seal();
    Ok((dataset, sources))
}
