//! Stable UTC buckets. Completed buckets are cached independently of live work.
//!
//! With recording rules installed ([`crate::rules`]), closed minutes come from one-minute
//! aggregates up to the newest aggregated minute; later data, buckets whose minutes are not
//! all aggregated, and views without rules are read from the raw series.
use crate::{
    prometheus::{Prometheus, Source},
    registry::{Instrument, Kind},
    rules,
};
use futures_util::{FutureExt, StreamExt, future::BoxFuture, stream};
use std::{collections::BTreeMap, sync::Arc};
const RETENTION: &str = "600d";
const GROUPS: &str = rules::GROUPS;
/// More unaggregated buckets than this are read as one raw range.
const MAX_GAP_READS: usize = 8;
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
/// Minutes aggregated by the installed rules, in Unix milliseconds.
#[derive(Clone, Debug, PartialEq)]
pub struct Coverage {
    pub version: String,
    /// End of the newest aggregated minute.
    pub through: u64,
    /// End of the oldest aggregated minute, at hourly resolution.
    pub oldest: u64,
    /// Whether aggregation starts with the first retained data, so earlier time holds nothing.
    pub from_first: bool,
}
impl Coverage {
    pub fn through_seconds(&self) -> f64 {
        self.through as f64 / 1000.
    }
    pub fn heartbeat(&self) -> String {
        format!("{}{{rules=\"{}\"}}", rules::EVALUATED, self.version)
    }
    /// Whether `count` aggregated minutes cover (start, end]; one minute of slack absorbs the
    /// evaluation phase, which differs between backfilled and live rule groups.
    pub fn complete(&self, start: f64, end: f64, count: f64) -> bool {
        let oldest = self.oldest as f64 / 1000.;
        if start + 60. < oldest && !self.from_first {
            return false;
        }
        count + 1. >= ((end - start.max(oldest)) / 60.).floor()
    }
}
#[derive(Clone, Copy)]
enum Part {
    /// Range matrix of bucket values stamped one step before each point.
    Range(u64),
    /// Like `Range`, read from the one-minute aggregates.
    Aggregated(u64),
    /// Instant vector of increments for the bucket starting at the stamp.
    Sum(u64),
    /// Counter values to subtract the paired baseline from.
    Current(usize, u64),
    Baseline(usize),
}
type Job<'a> = (Part, BoxFuture<'a, Result<Arc<Source>, String>>);
fn named(expression: &str) -> String {
    format!("label_replace({expression}, \"instrument\", \"$1\", \"__name__\", \"(.*)\")")
}
fn selector(metrics: &[&Instrument]) -> String {
    format!(
        "{{__name__=~\"{}\"}}",
        metrics.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join("|")
    )
}
/// Delta gauge sums over `window`. `sum_over_time` drops `__name__`, so each
/// instrument's name is restored as the `instrument` label.
fn response_sums(metrics: &[&Instrument], window: &str) -> String {
    let parts: Vec<String> = metrics
        .iter()
        .map(|m| {
            format!(
                "label_replace(sum_over_time({}[{window}]), \"instrument\", \"{}\", \"\", \"\")",
                selector(&[m]),
                m.name
            )
        })
        .collect();
    format!("sum by ({GROUPS}) ({})", parts.join(" or "))
}
pub fn samples(source: &Source) -> Vec<(BTreeMap<String, String>, f64, f64)> {
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
                out.push((labels.clone(), t, v));
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
fn range_params(query: String, start: u64, end: u64, step: u64) -> Vec<(String, String)> {
    vec![
        ("query".into(), query),
        ("start".into(), start.to_string()),
        ("end".into(), end.to_string()),
        ("step".into(), step.to_string()),
        ("timeout".into(), "20s".into()),
    ]
}
fn milliseconds(from: f64, to: f64) -> u64 {
    ((to - from) * 1000.).round().max(1.) as u64
}
pub struct Window {
    pub start: u64,
    pub boundary: u64,
    pub step: u64,
    pub now: u64,
    pub history_end: f64,
    pub budget: Arc<std::sync::atomic::AtomicUsize>,
}
struct Plan<'a> {
    prom: &'a Prometheus,
    registry: &'a [Instrument],
    step: u64,
    history_end: f64,
    tokens_only: bool,
    pairs: usize,
    jobs: Vec<Job<'a>>,
}
impl<'a> Plan<'a> {
    fn metrics(&self, kind: Kind) -> Vec<&'a Instrument> {
        let registry = self.registry;
        let tokens_only = self.tokens_only;
        registry
            .iter()
            .filter(|m| m.kind == kind && (!tokens_only || m.measurement == "tokens"))
            .collect()
    }
    fn read(&mut self, part: Part, params: Vec<(String, String)>, ttl: u64) {
        let prom = self.prom;
        self.jobs.push((
            part,
            async move { prom.read("/api/v1/query_range", params, ttl).await }.boxed(),
        ));
    }
    fn instant(&mut self, part: Part, query: String, time: f64, ttl: u64) {
        let prom = self.prom;
        self.jobs
            .push((part, async move { prom.instant(query, time, ttl).await }.boxed()));
    }
    /// Raw closed buckets ending in (start, boundary] and the partial bucket up to `now`.
    fn raw(&mut self, start: u64, boundary: u64, now: u64) {
        let step = self.step;
        // Closed history is refreshed at most every six hours for late ingestion.
        for kind in [Kind::Counter, Kind::Response, Kind::History] {
            if kind == Kind::History && start as f64 >= self.history_end {
                continue;
            }
            let metrics = self.metrics(kind);
            if metrics.is_empty() {
                continue;
            }
            let sel = selector(&metrics);
            if boundary > start {
                let query = match kind {
                    // Histograms are read only by per-selection quantile queries.
                    Kind::Histogram => continue,
                    Kind::Response => response_sums(&metrics, &format!("{step}s")),
                    Kind::Counter => {
                        let end = named(&format!("last_over_time({sel}[{step}s])"));
                        let before = named(&format!("last_over_time({sel}[{RETENTION}] offset {step}s)"));
                        format!("sum by ({GROUPS}) (clamp_min({end} - ({before} or {end} * 0), 0))")
                    }
                    Kind::History => {
                        let history_end = self.history_end;
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
                self.read(
                    Part::Range(step),
                    range_params(query, start + step, boundary, step),
                    21600,
                );
            }
            if now > boundary {
                self.partial(kind, boundary as f64, now as f64, boundary, now);
            }
        }
    }
    /// Raw increments of one kind over (from, to], stamped with the bucket start.
    fn partial(&mut self, kind: Kind, from: f64, to: f64, stamp: u64, now: u64) {
        let metrics = self.metrics(kind);
        if metrics.is_empty() {
            return;
        }
        match kind {
            Kind::Histogram => {}
            Kind::Response => {
                let query = response_sums(&metrics, &format!("{}ms", milliseconds(from, to)));
                self.instant(Part::Sum(stamp), query, to, 30);
            }
            Kind::Counter | Kind::History => {
                // Auxiliary projections reuse the main view's per-process counter endpoints.
                let sel = if kind == Kind::Counter {
                    selector(
                        &self
                            .registry
                            .iter()
                            .filter(|m| m.kind == Kind::Counter)
                            .collect::<Vec<_>>(),
                    )
                } else {
                    selector(&metrics)
                };
                if kind == Kind::History && from >= self.history_end {
                    return;
                }
                let end = if kind == Kind::History {
                    to.min(self.history_end - 0.001)
                } else {
                    to
                };
                let window = format!("{sel}[{}ms]", milliseconds(from, end));
                let current = named(&format!("last_over_time({window})"));
                let ttl = if kind == Kind::History && end < now as f64 {
                    21600
                } else {
                    30
                };
                // Baselines are needed only for series active in the window; retained
                // per-session history would otherwise grow without bound.
                let query = format!(
                    "{} and {}",
                    named(&format!("last_over_time({sel}[{RETENTION}])")),
                    named(&format!("last_over_time({window} @ {end})"))
                );
                let pair = self.pairs;
                self.pairs += 1;
                self.instant(Part::Current(pair, stamp), current, end, ttl);
                self.instant(Part::Baseline(pair), query, from, ttl);
            }
        }
    }
    /// Raw increments of every kind over (from, to], stamped with the bucket start.
    fn partials(&mut self, from: f64, to: f64, stamp: u64, now: u64) {
        for kind in [Kind::Counter, Kind::Response, Kind::History] {
            self.partial(kind, from, to, stamp, now);
        }
    }
    fn aggregated(&self) -> String {
        let names: Vec<_> = self
            .registry
            .iter()
            .filter(|m| m.kind != Kind::Histogram && (!self.tokens_only || m.measurement == "tokens"))
            .map(|m| m.name.as_str())
            .collect();
        format!("{}{{instrument=~\"{}\"}}", rules::INCREASE, names.join("|"))
    }
}
/// Aggregated minute counts per closed bucket ending in (start, end], and over (from, to].
async fn heartbeats(
    prom: &Prometheus,
    coverage: &Coverage,
    closed: Option<(u64, u64, u64)>,
    open: Option<(f64, f64)>,
) -> Result<(BTreeMap<u64, f64>, f64, Vec<Arc<Source>>), String> {
    let beat = coverage.heartbeat();
    let mut counts = BTreeMap::new();
    let mut sources = Vec::new();
    if let Some((start, end, step)) = closed {
        let source = prom
            .read(
                "/api/v1/query_range",
                range_params(format!("count_over_time({beat}[{step}s])"), start + step, end, step),
                21600,
            )
            .await?;
        for (_, t, v) in samples(&source) {
            counts.insert(t as u64, v);
        }
        sources.push(source);
    }
    let mut open_count = 0.;
    if let Some((from, to)) = open {
        let source = prom
            .instant(format!("count_over_time({beat}[{}ms])", milliseconds(from, to)), to, 30)
            .await?;
        open_count = samples(&source).first().map_or(0., |s| s.2);
        sources.push(source);
    }
    Ok((counts, open_count, sources))
}
/// Unaggregated time in (start, through] at hourly resolution, merged into runs. `None` when
/// there are more runs than [`MAX_GAP_READS`], so the caller reads the whole range raw.
pub async fn gaps(
    prom: &Prometheus,
    coverage: &Coverage,
    start: f64,
    through: f64,
) -> Result<(Option<Vec<(f64, f64)>>, Arc<Source>), String> {
    const HOUR: f64 = 3600.;
    let hours = ((through - start) / HOUR).ceil().max(1.) as u64;
    let first = through - (hours - 1) as f64 * HOUR;
    let source = prom
        .read(
            "/api/v1/query_range",
            vec![
                ("query".into(), format!("count_over_time({}[1h])", coverage.heartbeat())),
                ("start".into(), format!("{first:.3}")),
                ("end".into(), format!("{through:.3}")),
                ("step".into(), "3600".into()),
                ("timeout".into(), "20s".into()),
            ],
            30,
        )
        .await?;
    let key = |t: f64| (t * 1000.).round() as u64;
    let counts: BTreeMap<u64, f64> = samples(&source).into_iter().map(|(_, t, v)| (key(t), v)).collect();
    let mut runs: Vec<(f64, f64)> = Vec::new();
    for i in 0..hours {
        let end = first + i as f64 * HOUR;
        let count = counts.get(&key(end)).copied().unwrap_or(0.);
        if coverage.complete(end - HOUR, end, count) {
            continue;
        }
        let from = (end - HOUR).max(start);
        match runs.last_mut() {
            Some(run) if run.1 >= from => run.1 = end,
            _ => runs.push((from, end)),
        }
    }
    Ok(((runs.len() <= MAX_GAP_READS).then_some(runs), source))
}
pub async fn block(
    prom: &Prometheus,
    registry: &[Instrument],
    window: Window,
    tokens_only: bool,
    coverage: Option<&Coverage>,
) -> Result<Block, String> {
    let Window {
        start,
        boundary,
        step,
        now,
        history_end,
        budget,
    } = window;
    let mut plan = Plan {
        prom,
        registry,
        step,
        history_end,
        tokens_only,
        pairs: 0,
        jobs: Vec::new(),
    };
    let mut sources = Vec::new();
    let mut gaps = std::collections::BTreeSet::new();
    match coverage.filter(|c| c.through_seconds() > start as f64) {
        None => plan.raw(start, boundary, now),
        Some(coverage) => {
            let through = coverage.through_seconds().min(now as f64);
            // Closed buckets ending at or before the newest aggregated minute come from aggregates.
            let closed_end = (start + ((through as u64 - start) / step) * step).min(boundary);
            // The bucket holding `through`: closed when it ends by `boundary`, otherwise partial.
            let straddle = closed_end;
            let straddle_end = if straddle < boundary { straddle + step } else { now };
            let open = (through > straddle as f64).then_some((straddle as f64, through));
            let (counts, open_count, beats) = heartbeats(
                prom,
                coverage,
                (closed_end > start).then_some((start, closed_end, step)),
                open,
            )
            .await?;
            sources.extend(beats);
            let aggregated = plan.aggregated();
            if closed_end > start {
                let missing: Vec<u64> = (1..=(closed_end - start) / step)
                    .map(|i| start + i * step)
                    .filter(|end| {
                        let count = counts.get(end).copied().unwrap_or(0.);
                        !coverage.complete((end - step) as f64, *end as f64, count)
                    })
                    .collect();
                // Many unaggregated buckets are read raw as one range from the first of them.
                let aggregated_end = if missing.len() > MAX_GAP_READS {
                    missing[0] - step
                } else {
                    closed_end
                };
                if aggregated_end > start {
                    plan.read(
                        Part::Aggregated(step),
                        range_params(
                            format!("sum by ({GROUPS}) (sum_over_time({aggregated}[{step}s]))"),
                            start + step,
                            aggregated_end,
                            step,
                        ),
                        21600,
                    );
                }
                if aggregated_end < closed_end {
                    plan.raw(aggregated_end, closed_end, closed_end);
                } else {
                    for end in missing {
                        gaps.insert(end - step);
                        plan.raw(end - step, end, end);
                    }
                }
            }
            match open {
                Some((from, to)) if coverage.complete(from, to, open_count) => {
                    let query = format!(
                        "sum by ({GROUPS}) (sum_over_time({aggregated}[{}ms]))",
                        milliseconds(from, to)
                    );
                    plan.instant(Part::Sum(straddle), query, to, 30);
                    if straddle_end as f64 > through {
                        plan.partials(through, straddle_end as f64, straddle, now);
                    }
                }
                _ if straddle < boundary => plan.raw(straddle, straddle_end, straddle_end),
                _ if now > boundary => plan.partials(boundary as f64, now as f64, boundary, now),
                _ => {}
            }
            if straddle < boundary {
                // Raw buckets after the straddling one, and the partial bucket.
                plan.raw(straddle_end, boundary, now);
            }
        }
    }
    finish(plan, sources, budget, now, gaps).await
}
/// Runs the planned reads and converts them to bucket records.
async fn finish(
    plan: Plan<'_>,
    mut sources: Vec<Arc<Source>>,
    budget: Arc<std::sync::atomic::AtomicUsize>,
    now: u64,
    gaps: std::collections::BTreeSet<u64>,
) -> Result<Block, String> {
    let Plan {
        registry,
        tokens_only,
        jobs,
        ..
    } = plan;
    let (parts, futures): (Vec<_>, Vec<_>) = jobs.into_iter().unzip();
    let mut results = stream::iter(futures).buffered(4);
    let mut fetched = Vec::new();
    while let Some(result) = results.next().await {
        let source = result?;
        if budget.fetch_add(source.bytes, std::sync::atomic::Ordering::Relaxed) + source.bytes > 8 * 1024 * 1024 {
            return Err("This calculation exceeds the bounded history budget; historical coverage is unavailable at this cardinality".into());
        }
        fetched.push(source);
    }
    // Baselines retain process identity until after subtraction; aggregating first loses resets/attribution.
    let mut baselines: BTreeMap<(usize, String), f64> = BTreeMap::new();
    for (part, source) in parts.iter().zip(&fetched) {
        if let Part::Baseline(pair) = part {
            for (labels, _, v) in samples(source) {
                baselines.insert((*pair, serde_json::to_string(&labels).unwrap_or_default()), v);
            }
        }
    }
    let mut records = Vec::new();
    for (part, source) in parts.iter().zip(&fetched) {
        for (labels, t, v) in samples(source) {
            let name = labels.get("instrument").map(String::as_str).unwrap_or("");
            let Some(instrument) = registry.iter().find(|m| m.name == name) else {
                continue;
            };
            if tokens_only && instrument.measurement != "tokens" {
                continue;
            }
            let (time, value) = match *part {
                Part::Range(step) => ((t as u64).saturating_sub(step), v),
                // Raw reads replace the aggregated values of unaggregated buckets.
                Part::Aggregated(step) if gaps.contains(&(t as u64).saturating_sub(step)) => continue,
                Part::Aggregated(step) => ((t as u64).saturating_sub(step), v),
                Part::Sum(stamp) => (stamp, v),
                Part::Current(pair, stamp) => (
                    stamp,
                    (v - baselines
                        .get(&(pair, serde_json::to_string(&labels).unwrap_or_default()))
                        .copied()
                        .unwrap_or(0.))
                    .max(0.),
                ),
                Part::Baseline(_) => continue,
            };
            records.push(record(labels, time, value));
        }
    }
    let fetched_at = fetched.iter().map(|s| s.fetched).min().unwrap_or(now);
    sources.extend(fetched);
    Ok(Block {
        records,
        sources,
        fetched: fetched_at,
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
    coverage: Option<&Coverage>,
) -> Result<serde_json::Value, String> {
    let (start, boundary, step) = range;
    let name = &histogram.name;
    let raw = |q: f64, window: String| {
        format!("histogram_quantile({q}, sum by (le) (increase({name}{{{matchers}}}[{window}])))")
    };
    let comma = if matchers.is_empty() { "" } else { "," };
    let aggregated = format!("{}{{instrument=\"{name}\"{comma}{matchers}}}", rules::BUCKETS);
    // Aggregated minutes up to the newest one, raw reads for a few unaggregated runs.
    let covered = match coverage.filter(|c| c.through_seconds() > start as f64) {
        Some(coverage) => {
            let through = coverage.through_seconds().min(now as f64);
            gaps(prom, coverage, start as f64, through)
                .await?
                .0
                .map(|runs| (through, runs))
        }
        None => None,
    };
    let (total, timeline) = match covered {
        None => {
            // The bucket bounds come along so that quantiles in the +Inf bucket can be marked as capped.
            let total = format!(
                "label_replace({}, \"q\", \"p50\", \"\", \"\") or label_replace({}, \"q\", \"p95\", \"\", \"\") or label_replace(count by (le) (last_over_time({name}{{{matchers}}}[{}s])), \"q\", \"bound\", \"\", \"\")",
                raw(0.5, format!("{}s", now - start)),
                raw(0.95, format!("{}s", now - start)),
                now - start
            );
            let timeline = range_params(raw(0.5, format!("{step}s")), start + step, boundary, step);
            let (total, timeline) = futures_util::try_join!(
                prom.instant(total, now as f64, 30),
                prom.read("/api/v1/query_range", timeline, 21600)
            )?;
            (total, vec![timeline])
        }
        Some((through, runs)) => {
            let sel = format!("{name}{{{matchers}}}");
            // Aggregated segments between the runs; raw increments over the runs and after
            // `through`, where short windows need no retention-wide baselines.
            let mut terms = Vec::new();
            let mut from = start as f64;
            for (a, b) in runs.iter().copied().chain([(through, now as f64)]) {
                if a > from {
                    terms.push(format!(
                        "sum by (le) (sum_over_time({aggregated}[{}ms] @ {a:.3}))",
                        milliseconds(from, a)
                    ));
                }
                if b > a {
                    terms.push(format!(
                        "sum by (le) (increase({sel}[{}ms] @ {b:.3}))",
                        milliseconds(a, b)
                    ));
                }
                from = b;
            }
            let buckets = format!(
                "sum by (le) ({})",
                terms
                    .iter()
                    .enumerate()
                    .map(|(i, t)| format!("label_replace({t}, \"part\", \"{i}\", \"\", \"\")"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            );
            let total = format!(
                "label_replace(histogram_quantile(0.5, {buckets}), \"q\", \"p50\", \"\", \"\") or label_replace(histogram_quantile(0.95, {buckets}), \"q\", \"p95\", \"\", \"\")"
            );
            let closed_end = (start + ((through as u64 - start) / step) * step).min(boundary);
            let mut reads = vec![
                prom.instant(total, now as f64, 30).boxed(),
                prom.read(
                    "/api/v1/label/le/values",
                    vec![
                        ("match[]".into(), sel.clone()),
                        ("start".into(), start.to_string()),
                        ("end".into(), now.to_string()),
                    ],
                    3600,
                )
                .boxed(),
            ];
            if closed_end > start {
                reads.push(
                    prom.read(
                        "/api/v1/query_range",
                        range_params(
                            format!("histogram_quantile(0.5, sum by (le) (sum_over_time({aggregated}[{step}s])))"),
                            start + step,
                            closed_end,
                            step,
                        ),
                        21600,
                    )
                    .boxed(),
                );
            }
            // Closed buckets touching a run are read raw, as are those after `closed_end`.
            let raw_ends: Vec<u64> = (1..=(boundary - start) / step)
                .map(|i| start + i * step)
                .filter(|end| {
                    *end > closed_end
                        || runs
                            .iter()
                            .any(|(a, b)| ((end - step) as f64) < *b && (*end as f64) > *a)
                })
                .collect();
            if let (Some(first), Some(last)) = (raw_ends.first(), raw_ends.last()) {
                reads.push(
                    prom.read(
                        "/api/v1/query_range",
                        range_params(raw(0.5, format!("{step}s")), *first, *last, step),
                        21600,
                    )
                    .boxed(),
                );
            }
            let mut sources = futures_util::future::try_join_all(reads).await?.into_iter();
            let total = sources.next().ok_or("missing quantile read")?;
            let bounds = sources.next().ok_or("missing bucket bounds")?;
            // Aggregated values of buckets read raw are partial and give way to the raw ones.
            let mut points = BTreeMap::new();
            for (i, source) in sources.enumerate() {
                let aggregated = i == 0 && closed_end > start;
                for (_, t, v) in samples(&source) {
                    if aggregated != raw_ends.contains(&(t as u64)) {
                        points.insert((t as u64).saturating_sub(step), v / 1000.);
                    }
                }
            }
            let top = bounds
                .data
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|le| le.as_str()?.parse::<f64>().ok())
                .filter(|le| le.is_finite())
                .fold(0f64, f64::max);
            let mut value = quantile_values(&total, top);
            let points: Vec<_> = points.into_iter().collect();
            value.insert("p50Timeline".into(), serde_json::json!(points));
            return Ok(serde_json::Value::Object(value));
        }
    };
    let mut top = 0f64;
    for (labels, _, _) in samples(&total) {
        if labels.get("q").map(String::as_str) == Some("bound") {
            let le = labels.get("le").and_then(|le| le.parse::<f64>().ok()).unwrap_or(0.);
            if le.is_finite() {
                top = top.max(le);
            }
        }
    }
    let mut value = quantile_values(&total, top);
    // Closed buckets only; each point is stamped with its bucket start like the shared blocks.
    let points: Vec<_> = timeline
        .iter()
        .flat_map(|s| samples(s))
        .map(|(_, t, v)| ((t as u64).saturating_sub(step), v / 1000.))
        .collect();
    value.insert("p50Timeline".into(), serde_json::json!(points));
    Ok(serde_json::Value::Object(value))
}
fn quantile_values(total: &Source, top: f64) -> serde_json::Map<String, serde_json::Value> {
    let mut value = serde_json::Map::new();
    for (labels, _, v) in samples(total) {
        if let Some(q) = labels.get("q").map(String::as_str)
            && q != "bound"
        {
            value.insert(q.into(), (v / 1000.).into());
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
    value
}
