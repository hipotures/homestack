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
    pub fn timing(&self) -> Result<(u64, u64), String> {
        match self.range.as_str() {
            "1h" => Ok((3600, 60)),
            "24h" => Ok((86400, 3600)),
            "7d" => Ok((604800, 21600)),
            "30d" => Ok((2592000, 86400)),
            "90d" => Ok((7776000, 86400)),
            _ => Err("Unknown time range".into()),
        }
    }
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
