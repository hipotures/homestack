//! Rendering-independent datasets and exact, version-checked point deltas.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    hash::{Hash, Hasher},
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(default, deny_unknown_fields)]
pub struct Selection {
    pub agent: String,
    pub host: String,
    pub project: String,
    pub range: String,
}
impl Default for Selection {
    fn default() -> Self {
        Self {
            agent: "all".into(),
            host: "*".into(),
            project: "*".into(),
            range: "24h".into(),
        }
    }
}
impl Selection {
    pub fn validate(&self) -> Result<(), String> {
        if !["all", "claude", "codex"].contains(&self.agent.as_str()) {
            return Err("Unknown agent".into());
        }
        self.timing()?;
        for value in [&self.host, &self.project] {
            if value.len() > 160 || value.chars().any(char::is_control) {
                return Err("Invalid scope label".into());
            }
        }
        Ok(())
    }
    /// Range length and bucket in seconds, aiming at 30-90 points per chart; must match
    /// `AIUsage.RANGES` in `aiusage-echarts-theme.js`. `None` is "all", sized by [`all_timing`].
    pub fn timing(&self) -> Result<Option<(u64, u64)>, String> {
        const DAY: u64 = 86400;
        match self.range.as_str() {
            "1h" => Ok(Some((3600, 120))),
            "24h" => Ok(Some((DAY, 1800))),
            "7d" => Ok(Some((7 * DAY, 10800))),
            "30d" => Ok(Some((30 * DAY, 43200))),
            "90d" => Ok(Some((90 * DAY, DAY))),
            "180d" => Ok(Some((180 * DAY, 3 * DAY))),
            "1y" => Ok(Some((365 * DAY, 7 * DAY))),
            "all" => Ok(None),
            _ => Err("Unknown time range".into()),
        }
    }
}
/// "all" spans from the first retained sample; the bucket is the smallest step giving at most
/// 80 points, as `AIUsage.bucketFor` does. One extra step keeps the first sample inside the
/// range after the end is aligned down to a bucket boundary.
pub fn all_timing(span: u64) -> (u64, u64) {
    let steps = [1, 7, 14, 30, 91].map(|days| days * 86400);
    let step = steps.into_iter().find(|s| span / s <= 80).unwrap_or(steps[4]);
    ((span.div_ceil(step) + 1) * step, step)
}
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct TimeRange {
    pub start: u64,
    pub end: u64,
    pub step: u64,
    pub partial_start: u64,
}
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Series {
    pub id: String,
    pub label: String,
    pub dimensions: BTreeMap<String, String>,
    pub points: Vec<(u64, f64)>,
}
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Available,
    Missing,
    Unavailable,
    Partial,
}
#[derive(Clone, Debug, Serialize)]
pub struct Dataset {
    pub id: String,
    pub measurement: String,
    pub agent: String,
    pub unit: String,
    /// Resolve source checks through the snapshot freshness blocks, independent of content version.
    pub freshness_key: String,
    pub scope: Selection,
    pub range: TimeRange,
    pub state: Availability,
    pub series: Vec<Series>,
    pub total: Option<f64>,
    pub details: Value,
    pub warnings: Vec<String>,
    pub version: String,
}
pub fn hash(value: &impl Serialize) -> String {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_vec(value).unwrap_or_default().hash(&mut hash);
    format!("{:016x}", hash.finish())
}
impl Dataset {
    pub fn seal(&mut self) {
        self.version.clear();
        self.version = hash(self);
    }
}

pub fn changes(old: &BTreeMap<String, Dataset>, new: &BTreeMap<String, Dataset>) -> Vec<Value> {
    let mut changes = Vec::new();
    for (id, dataset) in new {
        let Some(previous) = old.get(id) else {
            changes.push(json!({"op":"replace","dataset":dataset}));
            continue;
        };
        if previous.version == dataset.version {
            continue;
        }
        let replacement = json!({"op":"replace","dataset":dataset});
        // Tiny datasets and structural changes are cheaper and clearer to replace.
        if previous.series.len() != dataset.series.len() || dataset.series.is_empty() || previous.scope != dataset.scope
        {
            changes.push(replacement);
            continue;
        }
        let mut patches = Vec::new();
        let mut compatible = true;
        for (before, after) in previous.series.iter().zip(&dataset.series) {
            if before.id != after.id || before.dimensions != after.dimensions {
                compatible = false;
                break;
            }
            let a: BTreeMap<_, _> = before.points.iter().copied().collect();
            let b: BTreeMap<_, _> = after.points.iter().copied().collect();
            let set: Vec<_> = b
                .iter()
                .filter(|(t, v)| a.get(t) != Some(v))
                .map(|(t, v)| (*t, *v))
                .collect();
            let remove: Vec<_> = a.keys().filter(|t| !b.contains_key(t)).copied().collect();
            if !set.is_empty() || !remove.is_empty() {
                patches.push(json!({"id":after.id,"set":set,"remove":remove}));
            }
        }
        let mut meta = serde_json::to_value(dataset).unwrap_or(Value::Null);
        if let Some(meta) = meta.as_object_mut() {
            meta.remove("series");
        }
        let patch = json!({"op":"points","id":id,"base":previous.version,"version":dataset.version,"meta":meta,"series":patches});
        changes.push(
            if compatible && patch.to_string().len() < replacement.to_string().len() {
                patch
            } else {
                replacement
            },
        );
    }
    let ids: BTreeSet<_> = new.keys().collect();
    for id in old.keys().filter(|id| !ids.contains(id)) {
        changes.push(json!({"op":"remove","id":id,"base":old[id].version}));
    }
    changes
}
