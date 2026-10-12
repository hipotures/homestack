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
    /// Build the compared object from several Python paths: Rust key →
    /// dot-separated Python path. Replaces `python_select`.
    #[serde(default)]
    pub python_pick: BTreeMap<String, String>,
    /// Python key → Rust key, applied to the selected object.
    #[serde(default)]
    pub python_rename: BTreeMap<String, String>,
    /// Python keys with no Rust counterpart: dot-separated paths where `*`
    /// stands for every element of a list or value of an object.
    #[serde(default)]
    pub python_drop: Vec<String>,
    /// Rust keys with no Python counterpart, as `python_drop`.
    #[serde(default)]
    pub rust_drop: Vec<String>,
    /// Python operations that have no Rust counterpart: patterns where `*`
    /// matches any text.
    #[serde(default)]
    pub python_ops_drop: Vec<String>,
    /// Rust operations that have no Python counterpart, as `python_ops_drop`.
    #[serde(default)]
    pub rust_ops_drop: Vec<String>,
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
        let mut value = if self.python_pick.is_empty() {
            match &self.python_select {
                Some(path) => self.lookup(output, path)?.clone(),
                None => output.clone(),
            }
        } else {
            let mut picked = serde_json::Map::new();
            for (key, path) in &self.python_pick {
                picked.insert(key.clone(), self.lookup(output, path)?.clone());
            }
            Value::Object(picked)
        };
        for path in &self.python_drop {
            drop_path(&mut value, &path.split('.').collect::<Vec<_>>());
        }
        if let Value::Object(map) = &mut value {
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
        for path in &self.rust_drop {
            drop_path(&mut value, &path.split('.').collect::<Vec<_>>());
        }
        value
    }

    /// Python operations without the ones this rule drops.
    #[must_use]
    pub fn python_ops(&self, ops: &[String]) -> Vec<String> {
        keep_ops(ops, &self.python_ops_drop)
    }

    /// Rust operations without the ones this rule drops.
    #[must_use]
    pub fn rust_ops(&self, ops: &[String]) -> Vec<String> {
        keep_ops(ops, &self.rust_ops_drop)
    }

    fn lookup<'v>(&self, output: &'v Value, path: &str) -> Result<&'v Value, RuleError> {
        let mut value = output;
        for key in path.split('.') {
            value = value.get(key).ok_or_else(|| RuleError::Select {
                rule: self.id.clone(),
                path: path.to_owned(),
            })?;
        }
        Ok(value)
    }
}

fn drop_path(value: &mut Value, path: &[&str]) {
    let Some((first, rest)) = path.split_first() else {
        return;
    };
    match (value, *first) {
        (Value::Array(items), "*") => items.iter_mut().for_each(|item| drop_path(item, rest)),
        (Value::Object(map), "*") => map.values_mut().for_each(|item| drop_path(item, rest)),
        (Value::Object(map), key) if rest.is_empty() => {
            map.remove(key);
        }
        (Value::Object(map), key) => {
            if let Some(item) = map.get_mut(key) {
                drop_path(item, rest);
            }
        }
        _ => {}
    }
}

fn keep_ops(ops: &[String], drop: &[String]) -> Vec<String> {
    ops.iter()
        .filter(|op| !drop.iter().any(|pattern| glob(pattern, op)))
        .cloned()
        .collect()
}

/// `*` matches any text, everything else itself.
#[must_use]
pub fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return pattern == text;
    };
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}
