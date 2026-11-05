//! Comparable activity estimates from occupied five-minute token-usage windows.
use crate::{
    datasets::{Availability, Dataset, Selection, Series, TimeRange},
    planner::{Coverage, samples},
    prometheus::{Prometheus, Source},
    registry::{Instrument, Kind},
    rules,
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
    coverage: Option<&Coverage>,
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
    let dataset = unavailable(
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
    if let Some(coverage) = coverage
        && let Some(found) = aggregated(prom, coverage, &agents, &matchers, start, end, step, boundary).await?
    {
        return Ok(finish(dataset, found.0, found.1, end));
    }
    // Exclude the left boundary even when a subquery includes its first sample.
    let query = |agent: &str, expression: &str, seconds: u64| {
        format!(
            "label_replace(sum_over_time(({expression})[{}s:300s]) / 12, \"agent\", \"{agent}\", \"\", \"\")",
            seconds - 1
        )
    };
    // Prometheus evaluates `or` operands one after another; separate per-agent
    // queries run concurrently, closed buckets first.
    let mut reads: Vec<futures_util::future::BoxFuture<'_, Result<Arc<Source>, String>>> = Vec::new();
    if boundary > start {
        for (agent, expression) in &agents {
            reads.push(Box::pin(prom.read(
                "/api/v1/query_range",
                vec![
                    ("query".into(), query(agent, expression, step)),
                    ("start".into(), (start + step).to_string()),
                    ("end".into(), boundary.to_string()),
                    ("step".into(), step.to_string()),
                    ("timeout".into(), "20s".into()),
                ],
                21600,
            )));
        }
    }
    if end > boundary {
        for (agent, expression) in &agents {
            reads.push(Box::pin(prom.instant(
                query(agent, expression, end - boundary),
                end as f64,
                30,
            )));
        }
    }
    let sources = futures_util::future::try_join_all(reads).await?;
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
    Ok(finish(dataset, points, sources, end))
}

fn finish(
    mut dataset: Dataset,
    points: BTreeMap<String, BTreeMap<u64, f64>>,
    sources: Vec<Arc<Source>>,
    end: u64,
) -> (Dataset, Vec<Arc<Source>>) {
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
    (dataset, sources)
}

/// Active slots from the one-minute activity aggregates up to the newest aggregated minute,
/// raw slots after it. `None` when some minute of the range was not aggregated.
#[allow(clippy::too_many_arguments)]
async fn aggregated(
    prom: &Prometheus,
    coverage: &Coverage,
    agents: &[(&str, String)],
    matchers: &str,
    start: u64,
    end: u64,
    step: u64,
    boundary: u64,
) -> Result<Option<(BTreeMap<String, BTreeMap<u64, f64>>, Vec<Arc<Source>>)>, String> {
    // 30-day reads keep each response small and closed ones cacheable.
    const CHUNK: u64 = 30 * 86400;
    let through = coverage.through_seconds().min(end as f64);
    if through <= start as f64 {
        return Ok(None);
    }
    let window = |from: f64, to: f64| ((to - from) * 1000.).round().max(1.) as u64;
    let (runs, check) = crate::planner::gaps(prom, coverage, start as f64, through).await?;
    let Some(runs) = runs else {
        return Ok(None);
    };
    let names: Vec<&str> = agents.iter().map(|(a, _)| *a).filter(|a| *a != "all").collect();
    let selector = format!("{}{{agent=~\"{}\"{matchers}}}", rules::ACTIVE, names.join("|"));
    let mut reads = Vec::new();
    let mut chunk = start / CHUNK * CHUNK;
    while (chunk as f64) < through {
        let from = chunk.max(start) as f64;
        let to = ((chunk + CHUNK) as f64).min(through);
        let ttl = if (chunk + CHUNK) as f64 <= through { 21600 } else { 30 };
        reads.push(prom.instant(format!("{selector}[{}ms]", window(from, to)), to, ttl));
        chunk += CHUNK;
    }
    // Slots (t - 300, t] touching an unaggregated run, or from the one holding the newest
    // aggregated minute on, are read raw; overlap with aggregated slots is harmless in a union.
    let mut windows: Vec<(u64, u64)> = runs
        .iter()
        .map(|(a, b)| {
            (
                (*a / SLOT as f64).floor() as u64 * SLOT + SLOT,
                (*b / SLOT as f64).ceil() as u64 * SLOT,
            )
        })
        .collect();
    windows.push(((through / SLOT as f64).ceil() as u64 * SLOT, end));
    let mut raw = Vec::new();
    for (from, to) in windows.into_iter().map(|(a, b)| (a.max(start + SLOT), b.min(end))) {
        if from > to {
            continue;
        }
        for (agent, expression) in agents.iter().filter(|(agent, _)| *agent != "all") {
            raw.push(async move {
                let params = vec![
                    ("query".into(), expression.clone()),
                    ("start".into(), from.to_string()),
                    ("end".into(), to.to_string()),
                    ("step".into(), SLOT.to_string()),
                    ("timeout".into(), "20s".into()),
                ];
                let ttl = if to as f64 <= through { 21600 } else { 30 };
                prom.read("/api/v1/query_range", params, ttl)
                    .await
                    .map(|source| (agent.to_string(), source))
            });
        }
    }
    let (chunks, raw) = futures_util::try_join!(
        futures_util::future::try_join_all(reads),
        futures_util::future::try_join_all(raw)
    )?;
    let mut slots: BTreeMap<String, std::collections::BTreeSet<u64>> = BTreeMap::new();
    for source in &chunks {
        for (labels, minute, _) in samples(source) {
            let Some(agent) = labels.get("agent") else {
                continue;
            };
            // A minute ending at `minute` belongs to the slot holding its midpoint.
            let slot = ((minute - 30.) / SLOT as f64).ceil() as u64 * SLOT;
            if slot > start && slot <= end {
                slots.entry(agent.clone()).or_default().insert(slot);
            }
        }
    }
    for (agent, source) in &raw {
        for (_, slot, v) in samples(source) {
            if v > 0. {
                slots.entry(agent.clone()).or_default().insert(slot as u64);
            }
        }
    }
    // Total is the union of agents, never a sum of overlapping wall-clock time.
    let union = slots.values().flatten().copied().collect();
    slots.insert("all".into(), union);
    let mut points = BTreeMap::new();
    for (agent, _) in agents {
        // Every bucket has a point, like the raw sums with `or vector(0)`.
        let mut counts = BTreeMap::new();
        for i in 0..boundary.saturating_sub(start) / step {
            counts.insert(start + i * step, 0u64);
        }
        if end > boundary {
            counts.insert(boundary, 0);
        }
        for slot in slots.get(*agent).into_iter().flatten() {
            let stamp = if *slot <= boundary {
                start + (slot - start).div_ceil(step) * step - step
            } else {
                boundary
            };
            *counts.entry(stamp).or_insert(0) += 1;
        }
        points.insert(
            agent.to_string(),
            counts.into_iter().map(|(t, n)| (t, n as f64 / 12.)).collect(),
        );
    }
    let sources = chunks
        .into_iter()
        .chain(raw.into_iter().map(|(_, source)| source))
        .chain([check])
        .collect();
    Ok(Some((points, sources)))
}
