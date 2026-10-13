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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    /// Another rule whose drops, picks and renames this one adds to.
    pub extends: Option<String>,
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
    /// Rust list items with no Python counterpart: path of a list → pattern
    /// (`*` matches any text) of the string items to remove, or
    /// `FIELD=PATTERN` for the object items whose string `FIELD` matches.
    #[serde(default)]
    pub rust_drop_items: BTreeMap<String, String>,
    /// Numbers that may differ by this share of the larger one (paths as in
    /// `python_drop`). For values a live cluster changes between two reads.
    #[serde(default)]
    pub relative_tolerance: BTreeMap<String, f64>,
    /// Numbers that may differ by this much.
    #[serde(default)]
    pub absolute_tolerance: BTreeMap<String, f64>,
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
    #[error("rule {0:?} extends itself or a rule that does not exist")]
    Extends(String),
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

    pub fn get(&self, id: &str) -> Result<Rule, RuleError> {
        let rule = self
            .rule
            .iter()
            .find(|rule| rule.id == id)
            .ok_or_else(|| RuleError::Missing(id.to_owned()))?;
        if rule.reason.trim().is_empty() {
            return Err(RuleError::NoReason(id.to_owned()));
        }
        let Some(base_id) = &rule.extends else {
            return Ok(rule.clone());
        };
        if base_id == id
            || self
                .rule
                .iter()
                .find(|r| &r.id == base_id)
                .and_then(|r| r.extends.as_ref())
                .is_some()
        {
            return Err(RuleError::Extends(id.to_owned()));
        }
        let base = self.get(base_id).map_err(|_| RuleError::Extends(id.to_owned()))?;
        let mut merged = rule.clone();
        merged.python_select = rule.python_select.clone().or(base.python_select);
        for (target, source) in [
            (&mut merged.python_drop, base.python_drop),
            (&mut merged.rust_drop, base.rust_drop),
            (&mut merged.python_ops_drop, base.python_ops_drop),
            (&mut merged.rust_ops_drop, base.rust_ops_drop),
        ] {
            target.extend(source);
        }
        merged.python_pick.extend(base.python_pick);
        merged.python_rename.extend(base.python_rename);
        merged.rust_drop_items.extend(base.rust_drop_items);
        merged.relative_tolerance.extend(base.relative_tolerance);
        merged.absolute_tolerance.extend(base.absolute_tolerance);
        Ok(merged)
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
        for (path, pattern) in &self.rust_drop_items {
            drop_items(&mut value, &path.split('.').collect::<Vec<_>>(), pattern);
        }
        value
    }

    /// Replace Rust numbers within a tolerance of Python's by Python's, so
    /// only real differences remain.
    pub fn reconcile(&self, python: &Value, rust: &mut Value) {
        for (path, share) in &self.relative_tolerance {
            align(python, rust, &path.split('.').collect::<Vec<_>>(), &|a, b| {
                (a - b).abs() <= share * a.abs().max(b.abs())
            });
        }
        for (path, limit) in &self.absolute_tolerance {
            align(python, rust, &path.split('.').collect::<Vec<_>>(), &|a, b| {
                (a - b).abs() <= *limit
            });
        }
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

fn drop_items(value: &mut Value, path: &[&str], pattern: &str) {
    match path.split_first() {
        None => {
            if let Value::Array(items) = value {
                let field = pattern.split_once('=');
                items.retain(|item| {
                    let text = match (item, field) {
                        (Value::Object(map), Some((key, _))) => map.get(key).and_then(Value::as_str),
                        (_, Some(_)) => None,
                        _ => item.as_str(),
                    };
                    let wanted = field.map_or(pattern, |(_, wanted)| wanted);
                    !text.is_some_and(|text| glob(wanted, text))
                });
            }
        }
        Some((first, rest)) => match (value, *first) {
            (Value::Array(items), "*") => items.iter_mut().for_each(|item| drop_items(item, rest, pattern)),
            (Value::Object(map), key) => {
                if let Some(item) = map.get_mut(key) {
                    drop_items(item, rest, pattern);
                }
            }
            _ => {}
        },
    }
}

fn align(python: &Value, rust: &mut Value, path: &[&str], close: &dyn Fn(f64, f64) -> bool) {
    match path.split_first() {
        None => {
            if let (Some(a), Some(b)) = (python.as_f64(), rust.as_f64())
                && close(a, b)
            {
                *rust = python.clone();
            }
        }
        Some((first, rest)) => match (python, rust, *first) {
            (Value::Array(left), Value::Array(right), "*") => {
                for (a, b) in left.iter().zip(right.iter_mut()) {
                    align(a, b, rest, close);
                }
            }
            (Value::Object(left), Value::Object(right), key) => {
                if let (Some(a), Some(b)) = (left.get(key), right.get_mut(key)) {
                    align(a, b, rest, close);
                }
            }
            _ => {}
        },
    }
}

/// Paths where two JSON values differ, at most `limit` of them.
#[must_use]
pub fn differences(python: &Value, rust: &Value, limit: usize) -> Vec<String> {
    let mut found = Vec::new();
    diff(python, rust, String::new(), limit, &mut found);
    found
}

fn diff(python: &Value, rust: &Value, path: String, limit: usize, found: &mut Vec<String>) {
    if found.len() >= limit || python == rust {
        return;
    }
    match (python, rust) {
        (Value::Object(left), Value::Object(right)) => {
            let keys: std::collections::BTreeSet<&String> = left.keys().chain(right.keys()).collect();
            for key in keys {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                match (left.get(key), right.get(key)) {
                    (Some(a), Some(b)) => diff(a, b, child, limit, found),
                    (Some(a), None) => found.push(format!("{child}: only Python has {a}")),
                    (None, Some(b)) => found.push(format!("{child}: only Rust has {b}")),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(left), Value::Array(right)) if left.len() == right.len() => {
            for (index, (a, b)) in left.iter().zip(right).enumerate() {
                diff(a, b, format!("{path}.{index}"), limit, found);
            }
        }
        _ => found.push(format!("{path}: Python {python}, Rust {rust}")),
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
