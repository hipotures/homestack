//! Manifest checking and output normalization.

use std::collections::BTreeSet;

use hs_parity::manifest::Manifest;
use hs_parity::normalize::Rules;
use serde_json::json;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn names(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

const MANIFEST: &str = r#"
tests = [
  { python = "tests/test_a.py::A::test_one", phase = "2", status = "todo" },
  { python = "tests/test_a.py::A::test_two", phase = "2", status = "ported", rust = ["status::tests::two"] },
  { python = "tests/test_b.py::B::test_tui", phase = "0", status = "excluded", reason = "TUI" },
]
"#;

#[test]
fn a_complete_manifest_passes() -> TestResult {
    let manifest: Manifest = toml::from_str(MANIFEST)?;
    let python = [
        "tests/test_a.py::A::test_one",
        "tests/test_a.py::A::test_two",
        "tests/test_b.py::B::test_tui",
    ];
    let problems = manifest.check(&python.map(str::to_owned), &names(&["status::tests::two"]));
    assert_eq!(problems, Vec::<String>::new());
    Ok(())
}

#[test]
fn every_gap_is_reported() -> TestResult {
    let manifest: Manifest = toml::from_str(MANIFEST)?;
    let python = [
        "tests/test_a.py::A::test_one",
        "tests/test_a.py::A::test_new",
        "tests/test_b.py::B::test_tui",
    ];
    let problems = manifest.check(&python.map(str::to_owned), &names(&[]));
    assert_eq!(problems.len(), 3, "{problems:#?}");
    assert!(
        problems
            .iter()
            .any(|p| p.contains("test_new") && p.contains("missing from the manifest"))
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("test_two") && p.contains("no longer exists"))
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("status::tests::two") && p.contains("does not exist"))
    );
    Ok(())
}

#[test]
fn entries_must_be_consistent() {
    let bad = [
        r#"tests = [{ python = "t::A::x", phase = "2", status = "excluded" }]"#,
        r#"tests = [{ python = "t::A::x", phase = "2", status = "excluded", reason = " " }]"#,
        r#"tests = [{ python = "t::A::x", phase = "2", status = "ported" }]"#,
        r#"tests = [{ python = "t::A::x", phase = "16", status = "todo" }]"#,
        r#"tests = [{ python = "t::A::x", phase = "2", status = "done" }]"#,
        r#"tests = [{ python = "t::A::x", phase = "2", status = "todo", typo = 1 }]"#,
        r#"tests = [{ python = "t::A::x", phase = "2", status = "todo" }, { python = "t::A::x", phase = "2", status = "todo" }]"#,
    ];
    for text in bad {
        let problems = match toml::from_str::<Manifest>(text) {
            Ok(manifest) => manifest.check(&["t::A::x".to_owned()], &BTreeSet::new()),
            Err(_) => vec!["parse error".to_owned()],
        };
        assert!(!problems.is_empty(), "accepted: {text}");
    }
}

#[test]
fn rust_tests_for_a_phase_are_listed() -> TestResult {
    let manifest: Manifest = toml::from_str(MANIFEST)?;
    assert_eq!(manifest.rust_tests_for_phase("2"), ["status::tests::two"]);
    assert!(manifest.rust_tests_for_phase("3").is_empty());
    Ok(())
}

#[test]
fn normalization_selects_renames_and_drops() -> TestResult {
    let rules: Rules = toml::from_str(
        r#"
        [[rule]]
        id = "r"
        python_select = "pve"
        python_rename = { version = "pveversion" }
        python_drop = ["extra"]
        rust_drop = ["server_only"]
        reason = "because"
        "#,
    )?;
    let rule = rules.get("r")?;
    let python = json!({"ok": true, "pve": {"version": "9", "node": "a", "extra": 1}});
    let rust = json!({"pveversion": "9", "node": "a", "server_only": 2});
    assert_eq!(rule.python(&python)?, json!({"pveversion": "9", "node": "a"}));
    assert_eq!(rule.rust(&rust), json!({"pveversion": "9", "node": "a"}));
    assert!(rules.get("missing").is_err());
    Ok(())
}

#[test]
fn every_rule_needs_a_reason() {
    let rules = toml::from_str::<Rules>(
        r#"
        [[rule]]
        id = "r"
        reason = ""
        "#,
    );
    assert!(rules.is_err() || rules.is_ok_and(|rules| rules.get("r").is_err()));
}

#[test]
fn the_checked_in_files_parse() -> TestResult {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../parity");
    let _: Manifest = toml::from_str(&std::fs::read_to_string(root.join("manifest.toml"))?)?;
    let rules: Rules = toml::from_str(&std::fs::read_to_string(root.join("normalize.toml"))?)?;
    rules.validate()?;
    Ok(())
}
