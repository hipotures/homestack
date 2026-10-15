//! Patching configuration documents (Python
//! `test_setup_documents.py::SetupDocumentTests`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use hs_cli::setup::documents::{inspect_document, merge_document};
use toml::Value;

fn leaf(path: &[&str], value: Value) -> (Vec<String>, Value) {
    (path.iter().map(|p| (*p).to_owned()).collect(), value)
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn toml(candidate: &str) -> toml::Table {
    candidate.parse().unwrap()
}

fn json(candidate: &str) -> serde_json::Value {
    serde_json::from_str(candidate).unwrap()
}

fn yaml(candidate: &str) -> yaml_rust2::Yaml {
    yaml_rust2::YamlLoader::load_from_str(candidate).unwrap().remove(0)
}

#[test]
fn empty_array_is_a_value_and_unrelated_null_is_preserved() {
    let empty = leaf(&["items"], Value::Array(Vec::new()));
    let (candidate, states) = merge_document(
        "toml",
        Some("items = [1]\nother = \"keep\"\n"),
        std::slice::from_ref(&empty),
    )
    .unwrap();
    assert_eq!(states, ["different"]);
    let parsed = toml(&candidate);
    assert_eq!(parsed["items"], Value::Array(Vec::new()));
    assert_eq!(parsed["other"], text("keep"));
    let (candidate, states) = merge_document(
        "json",
        Some(r#"{"items": [1], "other": null}"#),
        std::slice::from_ref(&empty),
    )
    .unwrap();
    assert_eq!(states, ["different"]);
    let parsed = json(&candidate);
    assert_eq!(parsed["items"], serde_json::json!([]));
    assert!(parsed.as_object().unwrap().contains_key("other") && parsed["other"].is_null());
    let (candidate, states) = merge_document("yaml", Some("items: [1]\nother: null\n"), &[empty]).unwrap();
    assert_eq!(states, ["different"]);
    let parsed = yaml(&candidate);
    assert!(parsed["items"].as_vec().unwrap().is_empty());
    assert!(parsed["other"].is_null());
}

#[test]
fn existing_empty_json_is_malformed() {
    for source in ["", " \n\t"] {
        assert!(merge_document("json", Some(source), &[leaf(&["enabled"], Value::Boolean(true))]).is_err());
    }
}

#[test]
fn toml_root_and_nested_insertion_preserve_unrelated_content() {
    let original = "model = \"workspace-only\"\n\n[projects.\"/home/user/DEV/example\"]\ntrust_level = \"trusted\"\n\n[tui]\ntheme = \"custom\"\n";
    let (candidate, states) = merge_document(
        "toml",
        Some(original),
        &[
            leaf(&["approval_policy"], text("never")),
            leaf(
                &["features", "context_management", "experimental_mode"],
                Value::Boolean(true),
            ),
            leaf(&["tui", "status_line_use_colors"], Value::Boolean(true)),
        ],
    )
    .unwrap();
    assert_eq!(states, ["missing", "missing", "missing"]);
    let parsed = toml(&candidate);
    assert_eq!(parsed["model"], text("workspace-only"));
    assert_eq!(
        parsed["projects"]["/home/user/DEV/example"]["trust_level"],
        text("trusted")
    );
    assert_eq!(parsed["tui"]["theme"], text("custom"));
    assert_eq!(
        parsed["features"]["context_management"]["experimental_mode"],
        Value::Boolean(true)
    );
    assert_eq!(parsed["tui"]["status_line_use_colors"], Value::Boolean(true));
    assert_eq!(parsed["approval_policy"], text("never"));
}

#[test]
fn toml_existing_managed_key_update_and_unrelated_section_preserved() {
    let original = "approval_policy = \"ask\"\nmodel = \"private\"\n\n[projects.\"/secret\"]\ntrust_level = \"trusted\"\n\n[tui]\ntheme = \"custom\"\nstatus_line_use_colors = false\n";
    let (candidate, states) = merge_document(
        "toml",
        Some(original),
        &[
            leaf(&["approval_policy"], text("never")),
            leaf(&["tui", "status_line_use_colors"], Value::Boolean(true)),
        ],
    )
    .unwrap();
    assert_eq!(states, ["different", "different"]);
    let parsed = toml(&candidate);
    assert_eq!(parsed["approval_policy"], text("never"));
    assert_eq!(parsed["tui"]["status_line_use_colors"], Value::Boolean(true));
    assert_eq!(parsed["tui"]["theme"], text("custom"));
    assert_eq!(parsed["projects"]["/secret"]["trust_level"], text("trusted"));
    assert_eq!(parsed["model"], text("private"));
}

#[test]
fn toml_array_is_replaced_atomically() {
    let desired = Value::Array(vec![text("model-with-reasoning"), text("current-dir")]);
    let (candidate, states) = merge_document(
        "toml",
        Some("status_line = [\"model\", \"custom\"]\nother = \"preserve\"\n"),
        &[leaf(&["status_line"], desired.clone())],
    )
    .unwrap();
    assert_eq!(states, ["different"]);
    let parsed = toml(&candidate);
    assert_eq!(parsed["status_line"], desired);
    assert_eq!(parsed["other"], text("preserve"));
}

#[test]
fn toml_noop_returns_original_and_distinguishes_bool_from_integer() {
    let original = "enabled = true\n";
    let (candidate, states) =
        merge_document("toml", Some(original), &[leaf(&["enabled"], Value::Boolean(true))]).unwrap();
    assert_eq!(states, ["matching"]);
    assert_eq!(candidate, original);
    let (candidate, states) = merge_document("toml", Some(original), &[leaf(&["enabled"], Value::Integer(1))]).unwrap();
    assert_eq!(states, ["different"]);
    assert_eq!(toml(&candidate)["enabled"], Value::Integer(1));
}

#[test]
fn missing_toml_file_is_created() {
    let (candidate, states) = merge_document(
        "toml",
        None,
        &[
            leaf(&["approvals_reviewer"], text("user")),
            leaf(&["tui", "theme"], text("dark")),
        ],
    )
    .unwrap();
    assert_eq!(states, ["missing", "missing"]);
    let parsed = toml(&candidate);
    assert_eq!(parsed["approvals_reviewer"], text("user"));
    assert_eq!(parsed["tui"]["theme"], text("dark"));
}

#[test]
fn inline_toml_table_is_promoted_for_nested_insertion() {
    let (candidate, states) = merge_document(
        "toml",
        Some("features = { existing = 1 }\n"),
        &[leaf(
            &["features", "context_management", "experimental_mode"],
            Value::Boolean(true),
        )],
    )
    .unwrap();
    assert_eq!(states, ["missing"]);
    let parsed = toml(&candidate);
    assert_eq!(parsed["features"]["existing"], Value::Integer(1));
    assert_eq!(
        parsed["features"]["context_management"]["experimental_mode"],
        Value::Boolean(true)
    );
}

#[test]
fn structural_conflict_blocks_without_returning_partial_candidate() {
    let error = merge_document(
        "toml",
        Some("features = \"not-a-table\"\n"),
        &[leaf(&["features", "multi_agent"], Value::Boolean(true))],
    )
    .unwrap_err();
    assert!(error.0.contains("requires a mapping"), "{}", error.0);
}

#[test]
fn json_merge_preserves_unrelated_keys() {
    let original =
        r#"{"model": "private", "unrelated": null, "tui": {"theme": "custom"}, "projects": {"/x": {"trust": true}}}"#;
    let (candidate, states) = merge_document(
        "json",
        Some(original),
        &[
            leaf(&["approval_policy"], text("never")),
            leaf(&["tui", "status_line_use_colors"], Value::Boolean(true)),
        ],
    )
    .unwrap();
    assert_eq!(states, ["missing", "missing"]);
    let parsed = json(&candidate);
    assert_eq!(parsed["model"], "private");
    assert!(parsed["unrelated"].is_null());
    assert_eq!(parsed["tui"]["theme"], "custom");
    assert_eq!(parsed["tui"]["status_line_use_colors"], true);
    assert_eq!(parsed["projects"]["/x"]["trust"], true);
    // Python's json.dumps(indent=2): the document's order, new keys last.
    assert!(candidate.starts_with("{\n  \"model\": \"private\",\n  \"unrelated\": null,\n  \"tui\": {\n    \"theme\": \"custom\",\n    \"status_line_use_colors\": true\n  },"), "{candidate}");
    assert!(
        candidate.ends_with("  \"approval_policy\": \"never\"\n}\n"),
        "{candidate}"
    );
}

#[test]
fn yaml_merge_preserves_unrelated_keys_and_aliases() {
    let original = "defaults: &defaults\n  theme: dark\n  unrelated: preserve\nprofile: *defaults\nunrelated: null\n";
    let (candidate, states) =
        merge_document("yaml", Some(original), &[leaf(&["defaults", "theme"], text("light"))]).unwrap();
    assert_eq!(states, ["different"]);
    assert!(candidate.contains("unrelated: preserve"), "{candidate}");
    assert!(candidate.contains("profile:"));
    let parsed = yaml(&candidate);
    assert_eq!(parsed["defaults"]["theme"].as_str(), Some("light"));
    assert_eq!(parsed["profile"]["theme"].as_str(), Some("dark"));
    assert_eq!(parsed["profile"]["unrelated"].as_str(), Some("preserve"));
    assert!(parsed["unrelated"].is_null());
}

#[test]
fn yaml_merge_key_shared_descendant_is_detached_before_edit() {
    let original = "base: &base\n  nested:\n    keep: 1\napp:\n  <<: *base\n";
    let (candidate, states) = merge_document(
        "yaml",
        Some(original),
        &[leaf(&["app", "nested", "keep"], Value::Integer(2))],
    )
    .unwrap();
    assert_eq!(states, ["different"]);
    let parsed = yaml(&candidate);
    assert_eq!(parsed["base"]["nested"]["keep"].as_i64(), Some(1));
    assert_eq!(parsed["app"]["nested"]["keep"].as_i64(), Some(2));
}

#[test]
fn literal_dotted_key_is_not_split() {
    let (candidate, states) = merge_document(
        "toml",
        Some("\"foo.bar\" = \"old\"\n"),
        &[leaf(&["foo.bar"], text("new"))],
    )
    .unwrap();
    assert_eq!(states, ["different"]);
    let parsed = toml(&candidate);
    assert_eq!(parsed["foo.bar"], text("new"));
    assert!(!parsed.contains_key("foo"));
}

#[test]
fn inspection_reports_per_leaf_state_and_unrelated_keys_do_not_drift() {
    let original = "model = \"private\"\napproval_policy = \"never\"\nfeatures = \"bad\"\n";
    let states = inspect_document(
        "toml",
        Some(original),
        &[
            leaf(&["approval_policy"], text("never")),
            leaf(&["sandbox_mode"], text("danger-full-access")),
            leaf(&["features", "multi_agent"], Value::Boolean(true)),
        ],
    )
    .unwrap();
    assert_eq!(states, ["matching", "missing", "unavailable"]);
}

#[test]
fn malformed_documents_and_explicit_yaml_null_are_rejected() {
    for (format, source) in [("toml", "broken = ["), ("json", "{"), ("yaml", "broken: [")] {
        assert!(
            merge_document(format, Some(source), &[leaf(&["enabled"], Value::Boolean(true))]).is_err(),
            "{format}"
        );
    }
    let error = merge_document("yaml", Some("null\n"), &[leaf(&["enabled"], Value::Boolean(true))]).unwrap_err();
    assert!(error.0.contains("root must be a mapping"), "{}", error.0);
}

#[test]
fn json_duplicate_and_nonstandard_numbers_are_rejected() {
    for source in [r#"{"key": 1, "key": 2}"#, r#"{"key": NaN}"#] {
        assert!(
            merge_document("json", Some(source), &[leaf(&["enabled"], Value::Boolean(true))]).is_err(),
            "{source}"
        );
    }
}

#[test]
fn json_and_floats_are_written_as_python_writes_them() {
    let (candidate, _) = merge_document(
        "json",
        Some("{}"),
        &[
            leaf(&["a"], Value::Float(1.0)),
            leaf(&["b"], Value::Float(1e16)),
            leaf(&["c"], Value::Float(0.0001)),
            leaf(&["d"], text("ż\"\n")),
        ],
    )
    .unwrap();
    assert_eq!(
        candidate,
        "{\n  \"a\": 1.0,\n  \"b\": 1e+16,\n  \"c\": 0.0001,\n  \"d\": \"ż\\\"\\n\"\n}\n"
    );
}
