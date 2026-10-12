//! `rust/parity/manifest.toml`: every Python test and its Rust replacement.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

/// Phases of `docs/migration/PLAN.md`.
pub const PHASES: &[&str] = &[
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14a", "14b", "14c", "14d", "14e", "14f",
    "15",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub tests: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// `tests/<file>.py::<Class>::<method>`
    pub python: String,
    /// Phase that ports (or excluded) the test.
    pub phase: String,
    pub status: Status,
    /// Rust tests that replace it, as `cargo test -- --list` names them.
    #[serde(default)]
    pub rust: Vec<String>,
    /// Why an excluded test is not ported.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Todo,
    Ported,
    Excluded,
}

impl Manifest {
    /// Everything wrong with the manifest, given the current Python tests and
    /// the names of all Rust tests. Empty means the manifest is complete.
    #[must_use]
    pub fn check(&self, python_tests: &[String], rust_tests: &BTreeSet<String>) -> Vec<String> {
        let mut problems = Vec::new();
        let mut entries: BTreeMap<&str, &Entry> = BTreeMap::new();
        for entry in &self.tests {
            if entries.insert(entry.python.as_str(), entry).is_some() {
                problems.push(format!("{}: listed more than once", entry.python));
            }
            if !PHASES.contains(&entry.phase.as_str()) {
                problems.push(format!("{}: unknown phase {:?}", entry.python, entry.phase));
            }
            match entry.status {
                Status::Excluded if entry.reason.as_deref().is_none_or(|r| r.trim().is_empty()) => {
                    problems.push(format!("{}: excluded without a reason", entry.python));
                }
                Status::Ported if entry.rust.is_empty() => {
                    problems.push(format!("{}: ported but names no Rust test", entry.python));
                }
                Status::Todo | Status::Ported | Status::Excluded => {}
            }
            for rust in &entry.rust {
                if !rust_tests.contains(rust) {
                    problems.push(format!("{}: Rust test {rust} does not exist", entry.python));
                }
            }
        }
        let current: BTreeSet<&str> = python_tests.iter().map(String::as_str).collect();
        for test in &current {
            if !entries.contains_key(test) {
                problems.push(format!("{test}: missing from the manifest"));
            }
        }
        for test in entries.keys() {
            if !current.contains(test) {
                problems.push(format!("{test}: no longer exists in Python; update the manifest"));
            }
        }
        problems
    }

    /// Rust tests that port Python tests of `phase`.
    #[must_use]
    pub fn rust_tests_for_phase(&self, phase: &str) -> Vec<String> {
        self.tests
            .iter()
            .filter(|entry| entry.phase == phase)
            .flat_map(|entry| entry.rust.iter().cloned())
            .collect()
    }
}

/// Test names from `cargo test -- --list --format terse` output.
#[must_use]
pub fn parse_rust_test_list(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|line| line.strip_suffix(": test"))
        .map(str::to_owned)
        .collect()
}
