//! `parity/normalize.toml`: the allowed differences between Python and
//! Rust output. Without a rule, any difference is a failure.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rules {
    #[serde(default)]
    pub rule: Vec<Rule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    /// Dot-separated path of the part of the Python output to compare.
    pub python_select: Option<String>,
    /// Python key → Rust key, applied to the selected object.
    #[serde(default)]
    pub python_rename: BTreeMap<String, String>,
    /// Python keys with no Rust counterpart.
    #[serde(default)]
    pub python_drop: Vec<String>,
    /// Rust keys with no Python counterpart.
    #[serde(default)]
    pub rust_drop: Vec<String>,
    /// Why the difference is allowed (decision or mapping reference).
    pub reason: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    #[error("no normalization rule {0:?}")]
    Missing(String),
    #[error("normalization rule {0:?} needs a reason")]
    NoReason(String),
    #[error("normalization rule {0:?} is defined twice")]
    Duplicate(String),
    #[error("rule {rule:?}: the Python output has nothing at {path:?}")]
    Select { rule: String, path: String },
}

impl Rules {
    pub fn validate(&self) -> Result<(), RuleError> {
        let mut seen = std::collections::BTreeSet::new();
        for rule in &self.rule {
            if rule.reason.trim().is_empty() {
                return Err(RuleError::NoReason(rule.id.clone()));
            }
            if !seen.insert(rule.id.as_str()) {
                return Err(RuleError::Duplicate(rule.id.clone()));
            }
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<&Rule, RuleError> {
        let rule = self
            .rule
            .iter()
            .find(|rule| rule.id == id)
            .ok_or_else(|| RuleError::Missing(id.to_owned()))?;
        if rule.reason.trim().is_empty() {
            return Err(RuleError::NoReason(id.to_owned()));
        }
        Ok(rule)
    }
}

impl Rule {
    pub fn python(&self, output: &Value) -> Result<Value, RuleError> {
        let mut value = output;
        if let Some(path) = &self.python_select {
            for key in path.split('.') {
                value = value.get(key).ok_or_else(|| RuleError::Select {
                    rule: self.id.clone(),
                    path: path.clone(),
                })?;
            }
        }
        let mut value = value.clone();
        if let Value::Object(map) = &mut value {
            for key in &self.python_drop {
                map.remove(key);
            }
            for (from, to) in &self.python_rename {
                if let Some(item) = map.remove(from) {
                    map.insert(to.clone(), item);
                }
            }
        }
        Ok(value)
    }

    #[must_use]
    pub fn rust(&self, output: &Value) -> Value {
        let mut value = output.clone();
        if let Value::Object(map) = &mut value {
            for key in &self.rust_drop {
                map.remove(key);
            }
        }
        value
    }
}
