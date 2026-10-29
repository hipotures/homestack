//! Shared snapshot construction, normalization, pricing, and bounded export retention.
use crate::{
    datasets::{Availability, Dataset, Selection, Series, TimeRange, hash},
    planner::{self, Block, Record},
    prometheus::{Prometheus, Source, now},
    registry::{self, Instrument},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

pub struct Snapshot {
    pub id: String,
    pub datasets: Arc<BTreeMap<String, Dataset>>,
    pub selection: Selection,
    pub fetched: u64,
    pub checked: u64,
    pub generated_ms: u128,
    pub sources: Vec<Arc<Source>>,
    pub hosts: Vec<String>,
    pub projects: Vec<String>,
    pub warnings: Vec<String>,
    pub source_freshness: Value,
}
impl Snapshot {
    pub fn full(&self) -> Value {
        json!({"snapshot":self.id,"selection":self.selection,"datasets":self.datasets,"freshness":self.freshness(),"hosts":self.hosts,"projects":self.projects,"warnings":self.warnings})
    }
    pub fn freshness(&self) -> Value {
        json!({"history_checked_at":self.fetched,"source_at":self.sources.iter().map(|s|s.fetched).max().unwrap_or(self.checked),"checked_at":self.checked,"generated_ms":self.generated_ms,"blocks":self.source_freshness})
    }
    pub fn export(&self, dataset: Option<&str>) -> Result<Value, String> {
        let chosen: Vec<_> = self
            .datasets
            .values()
            .filter(|d| dataset.is_none_or(|id| id == d.id))
            .collect();
        if chosen.is_empty() {
            return Err("Unknown dataset".into());
        }
        Ok(
            json!({"snapshot":self.id,"selection":self.selection,"freshness":self.freshness(),"datasets":chosen,"queries":self.sources.iter().map(|s|json!({"query":s.query,"parameters":s.params,"data":s.data,"fetched_at":s.fetched})).collect::<Vec<_>>(),"warnings":self.warnings}),
        )
    }
}
struct Global {
    registry: Vec<Instrument>,
    main: Block,
    hourly: Block,
    daily: Block,
    range: TimeRange,
}
#[derive(Default)]
struct Cache {
    global: VecDeque<(String, Instant, Arc<Global>)>,
    snapshots: VecDeque<Arc<Snapshot>>,
}
pub struct Dashboard {
    pub prom: Prometheus,
    prices: Option<Value>,
    history_end: f64,
    cache: Mutex<Cache>,
    pub connections: Arc<tokio::sync::Semaphore>,
}
impl Dashboard {
    pub fn new(prom: Prometheus, prices: Option<Value>, history_end: f64) -> Self {
        Self {
            prom,
            prices,
            history_end,
            cache: Mutex::new(Cache::default()),
            connections: Arc::new(tokio::sync::Semaphore::new(32)),
        }
    }
    pub async fn snapshot(&self, selection: Selection) -> Result<Arc<Snapshot>, String> {
        selection.validate()?;
        let started = Instant::now();
        let timestamp = now();
        let (seconds, step) = match selection.timing()? {
            Some(timing) => timing,
            None => crate::datasets::all_timing(timestamp.saturating_sub(self.first_sample(timestamp).await?)),
        };
        let boundary = timestamp / step * step;
        // This short, bounded shared cache also singleflights global misses; builds never overlap.
        let mut cache = self.cache.lock().await;
        if let Some(s) = cache
            .snapshots
            .iter()
            .rev()
            .find(|s| s.selection == selection && timestamp.saturating_sub(s.checked) < 30)
        {
            return Ok(Arc::clone(s));
        }
        let key = format!("{}:{boundary}", selection.range);
        let global = if let Some((_, _, global)) = cache
            .global
            .iter()
            .find(|(k, at, _)| k == &key && at.elapsed() < Duration::from_secs(30))
        {
            Arc::clone(global)
        } else {
            let names = self
                .prom
                .read(
                    "/api/v1/label/__name__/values",
                    vec![
                        ("match[]".into(), registry::DISCOVERY.into()),
                        (
                            "start".into(),
                            ((timestamp / 3600 * 3600) as i64 - 600 * 86400).to_string(),
                        ),
                        ("end".into(), (timestamp / 3600 * 3600 + 3600).to_string()),
                    ],
                    3600,
                )
                .await?;
            let names: Vec<String> = names
                .data
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|n| n.as_str().map(str::to_owned))
                .collect();
            let registry = registry::instruments(&names, self.history_end);
            let days = (seconds / 86400).max(7);
            let day = timestamp / 86400 * 86400;
            let hour = timestamp / 3600 * 3600;
            let budget = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let window = |start, boundary, step| planner::Window {
                start,
                boundary,
                step,
                now: timestamp,
                history_end: self.history_end,
                budget: Arc::clone(&budget),
            };
            let (main, hourly, daily) = tokio::try_join!(
                planner::block(&self.prom, &registry, window(boundary - seconds, boundary, step), false),
                planner::block(
                    &self.prom,
                    &registry,
                    window(hour - days.min(30) * 86400, hour, 3600),
                    true
                ),
                planner::block(
                    &self.prom,
                    &registry,
                    window(day - (days + 7) * 86400, day, 86400),
                    true
                )
            )?;
            let source_bytes: usize = main
                .sources
                .iter()
                .chain(&hourly.sources)
                .chain(&daily.sources)
                .map(|s| s.bytes)
                .sum();
            if source_bytes > 8 * 1024 * 1024 {
                return Err("This view exceeds the bounded history budget; historical coverage is unavailable at this cardinality".into());
            }
            let global = Arc::new(Global {
                registry,
                main,
                hourly,
                daily,
                range: TimeRange {
                    start: boundary - seconds,
                    end: boundary + step,
                    step,
                    partial_start: boundary,
                },
            });
            cache.global.retain(|(k, _, _)| k != &key);
            cache.global.push_back((key, Instant::now(), Arc::clone(&global)));
            while cache.global.len() > 2
                || cache.global.iter().map(|(_, _, g)| global_bytes(g)).sum::<usize>() > 8 * 1024 * 1024
            {
                cache.global.pop_front();
            }
            global
        };
        let mut snapshot = normalize(&global, selection, &self.prices, timestamp);
        snapshot.generated_ms = started.elapsed().as_millis();
        let snapshot = Arc::new(snapshot);
        if snapshot.full().to_string().len() > 512 * 1024 {
            return Err("Normalized view exceeds the 512 KiB dataset budget".into());
        }
        // Exact exports are bounded shared snapshots, never per-browser histories.
        cache.snapshots.push_back(Arc::clone(&snapshot));
        while cache.snapshots.len() > 12
            || cache.snapshots.iter().map(|s| snapshot_bytes(s)).sum::<usize>() > 8 * 1024 * 1024
        {
            cache.snapshots.pop_front();
        }
        Ok(snapshot)
    }
    /// Start of the day holding the first retained usage sample, or today without samples.
    async fn first_sample(&self, timestamp: u64) -> Result<u64, String> {
        let day = timestamp / 86400 * 86400;
        let source = self
            .prom
            .read(
                "/api/v1/query_range",
                vec![
                    ("query".into(), format!("count(last_over_time({}[1d]))", registry::DISCOVERY)),
                    ("start".into(), (day - 599 * 86400).to_string()),
                    ("end".into(), (day + 86400).to_string()),
                    ("step".into(), "86400".into()),
                    ("timeout".into(), "20s".into()),
                ],
                21600,
            )
            .await?;
        // Each point counts samples in the preceding day.
        Ok(source.data["result"][0]["values"]
            .as_array()
            .and_then(|values| values.first())
            .and_then(|pair| pair[0].as_f64())
            .map_or(day, |t| t as u64 - 86400))
    }
    pub async fn changes(&self, previous: &str, current: &Snapshot) -> Option<Vec<Value>> {
        let cache = self.cache.lock().await;
        cache
            .snapshots
            .iter()
            .find(|s| s.id == previous)
            .map(|before| crate::datasets::changes(&before.datasets, &current.datasets))
    }
    pub async fn export(&self, id: &str, dataset: Option<&str>) -> Result<Value, String> {
        let cache = self.cache.lock().await;
        cache
            .snapshots
            .iter()
            .find(|s| s.id == id)
            .ok_or("This snapshot has expired; wait for the next update before exporting")?
            .export(dataset)
    }
}
fn global_bytes(g: &Global) -> usize {
    g.main
        .sources
        .iter()
        .chain(&g.hourly.sources)
        .chain(&g.daily.sources)
        .map(|s| s.bytes)
        .sum::<usize>()
        + g.main.records.len() * 512
        + g.hourly.records.len() * 512
        + g.daily.records.len() * 512
}
fn snapshot_bytes(snapshot: &Snapshot) -> usize {
    snapshot.full().to_string().len() * 3
        + snapshot
            .sources
            .iter()
            .map(|s| s.data.to_string().len() * 4)
            .sum::<usize>()
}
fn selected(record: &Record, selection: &Selection) -> bool {
    (selection.host == "*" || record.labels.get("host_name") == Some(&selection.host))
        && (selection.project == "*" || record.labels.get("project") == Some(&selection.project))
}
type Groups = BTreeMap<(String, String), BTreeMap<u64, f64>>;
fn series(groups: Groups, dimension: &str) -> Vec<Series> {
    groups
        .into_iter()
        .map(|((model, category), points)| {
            let dimensions: BTreeMap<_, _> =
                [("model".into(), model.clone()), (dimension.into(), category.clone())].into();
            Series {
                id: hash(&dimensions),
                label: format!("{} · {}", if model.is_empty() { "?" } else { &model }, category),
                dimensions,
                points: points.into_iter().collect(),
            }
        })
        .collect()
}
fn make(
    agent: &str,
    measurement: &str,
    unit: &str,
    selection: &Selection,
    range: &TimeRange,
    series: Vec<Series>,
    supported: bool,
) -> Dataset {
    let state = if !supported {
        Availability::Unavailable
    } else if series.is_empty() {
        Availability::Missing
    } else {
        Availability::Available
    };
    let total = (!series.is_empty()).then(|| series.iter().flat_map(|s| &s.points).map(|(_, v)| v).sum());
    Dataset {
        id: format!("{agent}.{measurement}"),
        measurement: measurement.into(),
        agent: agent.into(),
        unit: unit.into(),
        freshness_key: if measurement == "activity" {
            "hourly"
        } else if measurement == "spikes" {
            "daily"
        } else {
            "usage"
        }
        .into(),
        scope: selection.clone(),
        range: range.clone(),
        state,
        series,
        total,
        details: Value::Null,
        warnings: Vec::new(),
        version: String::new(),
    }
}
fn agent_data(global: &Global, selection: &Selection, agent: &str, prices: &Option<Value>) -> Vec<Dataset> {
    let registry: Vec<_> = global.registry.iter().filter(|m| m.agent == agent).collect();
    let mut raw: BTreeMap<String, Groups> = BTreeMap::new();
    for r in global.main.records.iter().filter(|r| selected(r, selection)) {
        let Some(m) = registry.iter().find(|m| m.name == r.instrument) else {
            continue;
        };
        let category = if m.measurement == "tokens" {
            r.labels
                .get(if agent == "codex" { "token_type" } else { "type" })
                .cloned()
                .unwrap_or_default()
        } else if m.measurement == "tools" {
            format!(
                "{}:{}",
                r.labels.get("tool").map(String::as_str).unwrap_or("?"),
                r.labels.get("success").map(String::as_str).unwrap_or("?")
            )
        } else if m.unit == "observations" {
            "observations".into()
        } else if m.unit == "milliseconds" {
            "milliseconds".into()
        } else {
            r.labels.get("type").cloned().unwrap_or_else(|| "value".into())
        };
        let value = raw
            .entry(m.measurement.into())
            .or_default()
            .entry((r.labels.get("model").cloned().unwrap_or_default(), category))
            .or_default()
            .entry(r.time)
            .or_default();
        *value += r.value;
    }
    let mut normalized: BTreeMap<(String, String), BTreeMap<u64, f64>> = BTreeMap::new();
    if let Some(tokens) = raw.remove("tokens") {
        let mut models: BTreeMap<(String, u64), BTreeMap<String, f64>> = BTreeMap::new();
        for ((model, category), points) in tokens {
            for (t, v) in points {
                *models
                    .entry((model.clone(), t))
                    .or_default()
                    .entry(category.clone())
                    .or_default() += v;
            }
        }
        for ((model, t), values) in models {
            let get = |id: &str| values.get(id).copied().unwrap_or(0.);
            let (input, cached, written, output, total, reasoning) = if agent == "codex" {
                (
                    (get("input") - get("cached_input") - get("cache_write_input")).max(0.),
                    get("cached_input"),
                    get("cache_write_input"),
                    get("output"),
                    get("total"),
                    Some(get("reasoning_output")),
                )
            } else {
                (
                    get("input"),
                    get("cacheRead"),
                    get("cacheCreation"),
                    get("output"),
                    get("input") + get("cacheRead") + get("cacheCreation") + get("output"),
                    None,
                )
            };
            for (category, v) in [
                ("input", input),
                ("cached", cached),
                ("written", written),
                ("output", output),
                ("total", total),
            ] {
                normalized
                    .entry((model.clone(), category.into()))
                    .or_default()
                    .insert(t, v);
            }
            if let Some(v) = reasoning {
                normalized.entry((model, "reasoning".into())).or_default().insert(t, v);
            }
        }
    }
    let mut tokens = make(
        agent,
        "tokens",
        "tokens",
        selection,
        &global.range,
        series(normalized, "category"),
        registry.iter().any(|m| m.measurement == "tokens"),
    );
    tokens.total = sum_category(&tokens, "total");
    let mut output = Vec::new();
    let mut cost = if agent == "codex" {
        let mut models: BTreeMap<String, BTreeMap<u64, f64>> = BTreeMap::new();
        let mut missing = BTreeSet::new();
        for s in &tokens.series {
            let model = &s.dimensions["model"];
            let category = s.dimensions["category"].as_str();
            let field = match category {
                "input" => "input_cost_per_token",
                "cached" => "cache_read_input_token_cost",
                "written" => "cache_creation_input_token_cost",
                "output" => "output_cost_per_token",
                _ => continue,
            };
            let price = prices
                .as_ref()
                .and_then(|p| p.get(model.strip_prefix("openai/").unwrap_or(model)))
                .and_then(|p| p[field].as_f64())
                .filter(|v| v.is_finite() && *v >= 0.);
            if price.is_none() && s.points.iter().any(|(_, v)| *v > 0.) {
                missing.insert(model.clone());
                continue;
            }
            for (t, v) in &s.points {
                *models.entry(model.clone()).or_default().entry(*t).or_default() += v * price.unwrap_or(0.);
            }
        }
        models.retain(|name, _| !missing.contains(name));
        let groups = models
            .into_iter()
            .map(|(name, points)| ((name, "estimated".into()), points))
            .collect();
        let mut d = make(
            agent,
            "cost",
            "USD",
            selection,
            &global.range,
            series(groups, "kind"),
            true,
        );
        if prices.is_none() || !missing.is_empty() {
            d.state = Availability::Partial;
            d.total = None;
            d.warnings.push(if prices.is_none() {
                "Pricing unavailable; restart the service to retry the startup catalog".into()
            } else {
                format!(
                    "Incomplete estimate; missing prices for {}",
                    missing.into_iter().collect::<Vec<_>>().join(", ")
                )
            });
        }
        d.details = json!({"estimated":true,"basis":"Standard API rates from the startup LiteLLM catalog; not subscription charges"});
        d
    } else {
        make(
            agent,
            "cost",
            "USD",
            selection,
            &global.range,
            series(raw.remove("cost").unwrap_or_default(), "kind"),
            registry.iter().any(|m| m.measurement == "cost"),
        )
    };
    if agent == "codex" && !registry.iter().any(|m| m.kind == registry::Kind::Response) {
        tokens
            .warnings
            .push("Live response token metrics unavailable; showing historical measurements only".into());
        cost.warnings.extend(tokens.warnings.clone());
    }
    if tokens.series.is_empty() && (selection.host != "*" || selection.project != "*") {
        let unattributed = global.main.records.iter().any(|r| {
            registry
                .iter()
                .any(|m| m.name == r.instrument && m.measurement == "tokens")
                && ((selection.host != "*" && !r.labels.contains_key("host_name"))
                    || (selection.project != "*" && !r.labels.contains_key("project")))
        });
        tokens.warnings.push(if unattributed {"Usage samples have missing host_name/project labels. Unattributed usage cannot be assigned to this selection; choose All servers / All projects.".into()}else{"No usage matches the selected server/project. Missing data is not zero.".into()});
    }
    output.push(tokens);
    output.push(cost);
    for (measurement, unit) in [
        ("sessions", "count"),
        ("active", "seconds"),
        ("commits", "count"),
        ("lines", "count"),
        ("tools", "count"),
        ("turns", "count"),
        ("turnDuration", "milliseconds"),
        ("firstToken", "milliseconds"),
        ("firstOutput", "milliseconds"),
        ("toolDuration", "milliseconds"),
    ] {
        let groups = raw.remove(measurement).unwrap_or_default();
        let mut d = make(
            agent,
            measurement,
            unit,
            selection,
            &global.range,
            series(groups, "kind"),
            registry.iter().any(|m| m.measurement == measurement),
        );
        if ["turnDuration", "firstToken", "firstOutput", "toolDuration"].contains(&measurement) {
            let mut sums = BTreeMap::new();
            let mut counts = BTreeMap::new();
            for s in &d.series {
                for (t, v) in &s.points {
                    *(if s.dimensions["kind"] == "milliseconds" {
                        &mut sums
                    } else {
                        &mut counts
                    })
                    .entry(*t)
                    .or_insert(0.) += v;
                }
            }
            let points: Vec<_> = sums
                .iter()
                .filter_map(|(t, v)| counts.get(t).filter(|n| **n > 0.).map(|n| (*t, v / n / 1000.)))
                .collect();
            d.details = json!({"total_seconds":sums.values().sum::<f64>()/1000.,"observations":counts.values().sum::<f64>(),"totalTimeline":sums.iter().map(|(t,v)|(*t,*v/1000.)).collect::<Vec<_>>()});
            d.total = (counts.values().sum::<f64>() > 0.)
                .then(|| sums.values().sum::<f64>() / counts.values().sum::<f64>() / 1000.);
            d.unit = "seconds".into();
            d.series = if points.is_empty() {
                Vec::new()
            } else {
                vec![Series {
                    id: "mean".into(),
                    label: "Mean seconds".into(),
                    dimensions: BTreeMap::new(),
                    points,
                }]
            };
        }
        output.push(d);
    }
    output
}
fn sum_category(dataset: &Dataset, category: &str) -> Option<f64> {
    let rows: Vec<_> = dataset
        .series
        .iter()
        .filter(|s| s.dimensions.get("category").is_some_and(|c| c == category))
        .collect();
    (!rows.is_empty()).then(|| rows.iter().flat_map(|s| &s.points).map(|(_, v)| v).sum())
}
fn normalize(global: &Global, selection: Selection, prices: &Option<Value>, timestamp: u64) -> Snapshot {
    // Comparison and total tiles use these same datasets, never independent accounting queries.
    let mut all = Vec::new();
    for agent in ["claude", "codex"] {
        if selection.agent == "all" || selection.agent == agent {
            all.extend(agent_data(global, &selection, agent, prices));
        }
    }
    let mut values = BTreeMap::new();
    for agent in ["claude", "codex"] {
        let mut v = BTreeMap::new();
        for d in all.iter().filter(|d| d.agent == agent) {
            v.insert(
                d.measurement.clone(),
                json!({"value":d.total,"state":d.state,"warnings":d.warnings}),
            );
            if d.measurement == "tokens" {
                for category in ["total", "input", "cached", "written", "output", "reasoning"] {
                    v.insert(
                        category.into(),
                        json!({"value":sum_category(d,category),"state":if agent=="claude" && category=="reasoning"{Availability::Unavailable}else{d.state.clone()}}),
                    );
                }
                let input = sum_category(d, "input").unwrap_or(0.)
                    + sum_category(d, "cached").unwrap_or(0.)
                    + sum_category(d, "written").unwrap_or(0.);
                v.insert(
                    "cacheHit".into(),
                    json!({"value":if input>0.{sum_category(d,"cached").map(|v|v/input)}else{None}}),
                );
            }
            if d.measurement == "lines" {
                for category in ["added", "removed"] {
                    let value = (!d.series.is_empty()).then(|| {
                        d.series
                            .iter()
                            .filter(|s| s.dimensions.get("kind").is_some_and(|v| v == category))
                            .flat_map(|s| &s.points)
                            .map(|(_, v)| v)
                            .sum::<f64>()
                    });
                    v.insert(category.into(), json!({"value":value,"state":if agent=="claude" && category=="reasoning"{Availability::Unavailable}else{d.state.clone()}}));
                }
            }
        }
        values.insert(agent.to_owned(), v);
    }
    let mut totals = BTreeMap::new();
    for measurement in ["cost", "total"] {
        let available: Vec<f64> = values
            .values()
            .filter_map(|v| v.get(measurement).and_then(|v| v["value"].as_f64()))
            .collect();
        totals.insert(measurement.to_owned(),json!({"value":if available.is_empty(){None}else{Some(available.iter().sum::<f64>())},"contributors":available.len()}));
    }
    values.insert("totals".into(), totals);
    let mut summary = make("all", "summary", "mixed", &selection, &global.range, Vec::new(), true);
    summary.state = Availability::Available;
    summary.details = json!(values);
    all.push(summary);
    // Heatmap timestamps remain UTC; browsers group into their local weekday/hour.
    let mut heat = BTreeMap::<(String, String), BTreeMap<u64, f64>>::new();
    let mut daily = BTreeMap::<u64, f64>::new();
    for (block, is_heat) in [(&global.hourly, true), (&global.daily, false)] {
        for r in block.records.iter().filter(|r| selected(r, &selection)) {
            let Some(m) = global.registry.iter().find(|m| m.name == r.instrument) else {
                continue;
            };
            if selection.agent != "all" && selection.agent != m.agent {
                continue;
            }
            let category = r
                .labels
                .get(if m.agent == "codex" { "token_type" } else { "type" })
                .map(String::as_str)
                .unwrap_or("");
            if m.agent == "codex" && category != "total" {
                continue;
            }
            if is_heat {
                *heat
                    .entry((m.agent.into(), "total".into()))
                    .or_default()
                    .entry(r.time)
                    .or_default() += r.value;
            } else {
                *daily.entry(r.time).or_default() += r.value;
            }
        }
    }
    let mut activity = make(
        "all",
        "activity",
        "tokens",
        &selection,
        &TimeRange {
            start: timestamp / 3600 * 3600
                - ((global.range.end - global.range.step - global.range.start) / 86400).clamp(7, 30) * 86400,
            end: timestamp / 3600 * 3600 + 3600,
            step: 3600,
            partial_start: timestamp / 3600 * 3600,
        },
        series(heat, "category"),
        true,
    );
    activity.total = None;
    all.push(activity);
    let mut spikes = make("all", "spikes", "tokens", &selection, &global.range, Vec::new(), true);
    let days = ((global.range.end - global.range.step - global.range.start) / 86400).max(7);
    let day = timestamp / 86400 * 86400;
    let times: Vec<_> = if daily.is_empty() {
        Vec::new()
    } else {
        (0..=days + 7)
            .map(|i| {
                let t = day - (days + 7 - i) * 86400;
                (t, daily.get(&t).copied().unwrap_or(0.))
            })
            .collect()
    };
    let mut items = Vec::new();
    for i in 7..times.len() {
        let before = times[i - 7..i].iter().map(|(_, v)| v).sum::<f64>() / 7.;
        if before > 0. && times[i].1 > 2.5 * before {
            items.push(json!({"time":times[i].0,"tokens":times[i].1,"factor":times[i].1/before}));
        }
    }
    spikes.details = json!(items);
    spikes.state = Availability::Available;
    all.push(spikes);
    for d in &mut all {
        enrich(d);
    }
    let mut datasets = BTreeMap::new();
    for mut d in all {
        d.seal();
        datasets.insert(d.id.clone(), d);
    }
    let mut hosts = BTreeSet::new();
    let mut projects = BTreeSet::new();
    for r in global
        .main
        .records
        .iter()
        .chain(&global.hourly.records)
        .chain(&global.daily.records)
    {
        if let Some(v) = r.labels.get("host_name") {
            hosts.insert(v.clone());
        }
        if let Some(v) = r.labels.get("project") {
            projects.insert(v.clone());
        }
    }
    // Inactive scopes can exist only in retained boundary baselines. Keep them
    // discoverable without issuing another retention-wide label query.
    for row in global
        .main
        .sources
        .iter()
        .flat_map(|source| source.data["result"].as_array().into_iter().flatten())
    {
        for (label, values) in [("host_name", &mut hosts), ("project", &mut projects)] {
            if let Some(value) = row["metric"][label].as_str()
                && !value.is_empty()
                && value.len() <= 160
                && !value.chars().any(char::is_control)
            {
                values.insert(value.to_owned());
            }
        }
    }
    let mut sources = Vec::new();
    let mut seen = BTreeSet::new();
    for source in global
        .main
        .sources
        .iter()
        .chain(&global.hourly.sources)
        .chain(&global.daily.sources)
    {
        let key = hash(&source.params);
        if seen.insert(key) {
            sources.push(Arc::clone(source));
        }
    }
    let warnings=vec!["History uses retained samples (600 days), not a lifetime ledger. Counter resets between samples, missing exports, pre-retention baselines and unattributed history cannot be reconstructed. Totals describe the selected UTC buckets; the current bucket is partial.".into()];
    let id = hash(&(
        timestamp,
        &selection,
        datasets.values().map(|d| &d.version).collect::<Vec<_>>(),
        sources.iter().map(|s| s.fetched).collect::<Vec<_>>(),
    ));
    Snapshot {
        id,
        datasets: Arc::new(datasets),
        selection,
        fetched: global.main.fetched.min(global.hourly.fetched).min(global.daily.fetched),
        checked: timestamp,
        generated_ms: 0,
        sources,
        hosts: hosts.into_iter().collect(),
        projects: projects.into_iter().collect(),
        warnings,
        source_freshness: json!({"usage":block_freshness(&global.main),"hourly":block_freshness(&global.hourly),"daily":block_freshness(&global.daily)}),
    }
}

// Presentation-ready projections are shared by tiles, charts, tables and exports.
// Exact source values remain in the normalized series above.
fn enrich(d: &mut Dataset) {
    let mut timeline = BTreeMap::<u64, f64>::new();
    if d.measurement == "tokens" {
        let mut categories = BTreeMap::<String, BTreeMap<u64, f64>>::new();
        let mut models = BTreeMap::<String, BTreeMap<u64, f64>>::new();
        for s in &d.series {
            let category = &s.dimensions["category"];
            for (t, v) in &s.points {
                *categories.entry(category.clone()).or_default().entry(*t).or_default() += v;
                if category == "total" {
                    *models
                        .entry(s.dimensions["model"].clone())
                        .or_default()
                        .entry(*t)
                        .or_default() += v;
                }
            }
        }
        let mut hit = Vec::new();
        if let Some(input) = categories.get("input") {
            for (t, v) in input {
                let cached = categories.get("cached").and_then(|p| p.get(t)).copied().unwrap_or(0.);
                let written = categories.get("written").and_then(|p| p.get(t)).copied().unwrap_or(0.);
                let total = v + cached + written;
                if total > 0. {
                    hit.push((*t, cached / total));
                }
            }
        }
        d.details = json!({"categories":categories.into_iter().map(|(id,p)|json!({"id":id,"label":id,"points":p.into_iter().collect::<Vec<_>>()})).collect::<Vec<_>>(),"models":models.into_iter().map(|(id,p)|json!({"id":id,"label":id,"points":p.into_iter().collect::<Vec<_>>()})).collect::<Vec<_>>(),"cacheHit":hit});
    } else if !["summary", "activity", "spikes"].contains(&d.measurement.as_str()) {
        for s in &d.series {
            for (t, v) in &s.points {
                *timeline.entry(*t).or_default() += v;
            }
        }
        let mut details = d.details.as_object().cloned().unwrap_or_default();
        details.insert("timeline".into(), json!(timeline.into_iter().collect::<Vec<_>>()));
        if d.measurement == "tools" {
            let mut failures: BTreeMap<u64, f64> = d
                .series
                .iter()
                .flat_map(|s| s.points.iter().map(|(t, _)| (*t, 0.)))
                .collect();
            let mut tools = BTreeMap::<String, BTreeMap<u64, f64>>::new();
            for s in &d.series {
                let kind = &s.dimensions["kind"];
                let (tool, success) = kind.rsplit_once(':').unwrap_or((kind, "?"));
                for (t, v) in &s.points {
                    *tools.entry(tool.into()).or_default().entry(*t).or_default() += v;
                    if success == "false" {
                        *failures.entry(*t).or_default() += v;
                    }
                }
            }
            details.insert("failures".into(), json!(failures.into_iter().collect::<Vec<_>>()));
            details.insert(
                "tools".into(),
                json!(
                    tools
                        .into_iter()
                        .map(|(id, p)| json!({"id":id,"label":id,"points":p.into_iter().collect::<Vec<_>>()}))
                        .collect::<Vec<_>>()
                ),
            );
        }
        d.details = Value::Object(details);
    }
}

fn block_freshness(block: &Block) -> Value {
    json!({"source_at":block.sources.iter().map(|s|s.fetched).max().unwrap_or(block.fetched),"history_checked_at":block.fetched})
}
