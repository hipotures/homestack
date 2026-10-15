//! Applications with managed values and files (Python
//! `test_setup_structured.py`, `test_setup_structured_guest.py`,
//! `test_setup_app_files.py`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod setup_fixture;

use std::cell::RefCell;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use base64::Engine;
use common::FakeSystem;
use hs_cli::AppError;
use hs_cli::setup::catalog::{Catalog, Github, load_catalog, select_entries};
use hs_cli::setup::definitions::{Entry, Params, Validation, app_file_entries, config_entries, parse_setup};
use hs_cli::setup::engine::{Connection, Quiet, execute_plan, inspect_workspace_state, validate_application_config};
use hs_cli::setup::plan::build_plan;
use hs_cli::setup::{Settings, sha256_hex};
use hs_cli::system::Output;
use serde_json::{Value, json};
use setup_fixture::{TestWorkspace, run_guest, settings, target};

fn example_settings(home: &Path) -> Settings {
    let text =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../python/config.example.toml"))
            .unwrap();
    let legacy = hs_config::legacy::read(&text).unwrap();
    let mut cfg = settings(home);
    cfg.setup = parse_setup(legacy.client.setup.as_ref().unwrap()).unwrap();
    cfg
}

fn report(status: &Value, overall: &str) -> String {
    json!({"overallStatus": overall, "checks": {"config.load": {"status": status}}}).to_string()
}

/// Python's `StructuredExecutionTests`: a workspace home with Codex's
/// configuration, the real guest program, a validator and an installer
/// that leave events.
struct Harness {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    path: PathBuf,
    cfg: Settings,
    app: Entry,
    events: Rc<RefCell<Vec<String>>>,
    report: Rc<RefCell<String>>,
    /// What the installer writes into the document, if anything.
    install_writes: Rc<RefCell<Option<String>>>,
    /// What the snapshot leaves in the document, if anything.
    after_snapshot: Rc<RefCell<Option<String>>>,
}

impl Harness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let path = home.join(".codex/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "model = \"private-workspace-model\"\n[tui]\ntheme = \"custom\"\n",
        )
        .unwrap();
        let cfg = example_settings(&home);
        let app = cfg.item("codex");
        Self {
            _tmp: tmp,
            home,
            path,
            cfg,
            app,
            events: Rc::default(),
            report: Rc::new(RefCell::new(report(&json!("ok"), "fail"))),
            install_writes: Rc::default(),
            after_snapshot: Rc::default(),
        }
    }

    fn leaves(&self) -> Vec<Entry> {
        config_entries(&self.app)
    }

    fn workspace(&self) -> TestWorkspace {
        let mut workspace = TestWorkspace::new(&self.home);
        let events = Rc::clone(&self.events);
        let home = self.home.clone();
        let after = Rc::clone(&self.after_snapshot);
        workspace.guest = Box::new(move |operation, payload| {
            events.borrow_mut().push(operation.to_owned());
            if operation == "identity" {
                return Some(Ok(json!({"ok": true})));
            }
            let mut payload = payload.clone();
            payload["home"] = json!(home.display().to_string());
            let answer = run_guest(&payload);
            if operation == "snapshot"
                && let Some(text) = after.borrow().as_ref()
            {
                std::fs::write(home.join(".codex/config.toml"), text).unwrap();
            }
            Some(Ok(answer))
        });
        let events = Rc::clone(&self.events);
        let report = Rc::clone(&self.report);
        let writes = Rc::clone(&self.install_writes);
        let path = self.path.clone();
        workspace.answer = Box::new(move |command, _| {
            if command.contains("doctor") {
                events.borrow_mut().push("validator".into());
                return Ok(Output {
                    code: 1,
                    stdout: report.borrow().clone(),
                    stderr: "SECRET".into(),
                });
            }
            if command.contains("install.sh") {
                events.borrow_mut().push("installer".into());
                if let Some(text) = writes.borrow().as_ref() {
                    std::fs::write(&path, text).unwrap();
                }
            }
            Ok(Output::default())
        });
        workspace
    }

    fn execute(&self, entries: &[Entry], expand: bool) -> Value {
        let plan = build_plan(&self.cfg, &target(), entries, None, false, expand).unwrap();
        let mut workspace = self.workspace();
        execute_plan(
            &self.cfg,
            &FakeSystem::new(),
            &plan,
            Connection::Open(&mut workspace),
            &mut Quiet,
        )
    }

    fn count(&self, event: &str) -> usize {
        self.events.borrow().iter().filter(|e| *e == event).count()
    }

    fn index(&self, event: &str) -> usize {
        self.events
            .borrow()
            .iter()
            .position(|e| e == event)
            .unwrap_or_else(|| panic!("no {event}: {:?}", self.events.borrow()))
    }

    fn has(&self, event: &str) -> bool {
        self.events.borrow().iter().any(|e| e == event)
    }

    fn registry(&self) -> String {
        std::fs::read_to_string(self.home.join(".local/state/homestack/setup.json")).unwrap_or_default()
    }
}

trait Item {
    fn item(&self, id: &str) -> Entry;
}

impl Item for Settings {
    fn item(&self, id: &str) -> Entry {
        self.setup.item(id).unwrap().clone()
    }
}

fn with_params(entry: &Entry, change: impl FnOnce(&mut hs_cli::setup::definitions::Application)) -> Entry {
    let mut entry = entry.clone();
    if let Params::Application(p) = &mut entry.params {
        change(p);
    }
    entry
}

/// A validator answering `code` with `stdout`.
fn validator_workspace(home: &Path, code: i32, stdout: &str) -> TestWorkspace {
    let mut workspace = TestWorkspace::new(home);
    let stdout = stdout.to_owned();
    workspace.answer = Box::new(move |_, _| {
        Ok(Output {
            code,
            stdout: stdout.clone(),
            stderr: "SECRET".into(),
        })
    });
    workspace
}

#[test]
fn guest_payload_uses_stdin_not_process_arguments() {
    let home = tempfile::tempdir().unwrap();
    let mut workspace = TestWorkspace::scripted(home.path(), |_, _| Ok(json!({"ok": true})));
    let content = "PRIVATE".repeat(50_000);
    hs_cli::setup::guest::guest(
        &mut workspace,
        &settings(home.path()),
        "structured-write",
        hs_cli::guest_args! {"relative": "config.json", "content": content},
    )
    .unwrap();
    assert_eq!(workspace.guests[0]["content"], json!(content));
    assert!(
        workspace
            .commands
            .iter()
            .all(|(command, _)| !command.contains("PRIVATE"))
    );
}

#[test]
fn exit_code_validator() {
    let home = tempfile::tempdir().unwrap();
    let cfg = example_settings(home.path());
    let application = with_params(&cfg.item("codex"), |p| {
        let mut validator = p.validation.clone().unwrap();
        validator.kind = "exit-code".into();
        validator.path.clear();
        validator.accepted.clear();
        p.validation = Some(validator);
    });
    for code in [0, 1, 127] {
        let mut workspace = validator_workspace(home.path(), code, "SECRET");
        let result = validate_application_config(&mut workspace, &cfg, &application);
        if code == 0 {
            result.unwrap();
        } else {
            assert!(result.unwrap_err().0.contains("validator exited"));
        }
    }
}

#[test]
fn status_field_is_the_only_compatibility_signal() {
    let home = tempfile::tempdir().unwrap();
    let cfg = example_settings(home.path());
    for status in ["ok", "warning"] {
        let mut workspace = validator_workspace(home.path(), 1, &report(&json!(status), "fail"));
        validate_application_config(&mut workspace, &cfg, &cfg.item("codex")).unwrap();
    }
}

#[test]
fn invalid_reports_are_sanitized() {
    let home = tempfile::tempdir().unwrap();
    let cfg = example_settings(home.path());
    for (code, output, error) in [
        (1, report(&json!("fail"), "fail"), "config.load=fail"),
        (0, "SECRET invalid JSON".to_owned(), "malformed JSON"),
        (0, "{}".to_owned(), "missing status path"),
        (0, report(&json!("SECRET"), "fail"), "unaccepted status"),
        (0, report(&json!({"SECRET": "value"}), "fail"), "unaccepted status"),
        (0, report(&json!(["SECRET"]), "fail"), "unaccepted status"),
        (0, report(&Value::Null, "fail"), "unaccepted status"),
        (127, report(&json!("ok"), "fail"), "unavailable"),
        (255, report(&json!("ok"), "fail"), "unavailable"),
    ] {
        let mut workspace = validator_workspace(home.path(), code, &output);
        let message = validate_application_config(&mut workspace, &cfg, &cfg.item("codex"))
            .unwrap_err()
            .0;
        assert!(message.contains(error), "{message}");
        assert!(!message.contains("SECRET"));
    }
}

#[test]
fn one_write_and_snapshot_for_many_leaves_and_final_validation_order() {
    let h = Harness::new();
    let result = h.execute(std::slice::from_ref(&h.app), true);
    assert_eq!(result["ok"], json!(true), "{result}");
    assert_eq!(h.count("structured-write"), 1);
    assert_eq!(h.count("snapshot"), 1);
    assert_eq!(h.count("structured-read"), 2);
    assert!(h.index("snapshot") < h.index("installer"));
    assert!(h.index("installer") < h.index("structured-write"));
    assert!(h.index("structured-write") < h.index("validator"));
    assert!(h.index("validator") < h.index("state-record"));
    assert!(!result.to_string().contains("private-workspace-model"));
    let registry = h.registry();
    assert!(!registry.contains("private-workspace-model"));
    assert!(!registry.contains("candidate"));
}

#[test]
fn noop_has_no_write_or_snapshot_and_unrelated_keys_are_not_drift() {
    let h = Harness::new();
    assert_eq!(h.execute(&h.leaves(), false)["ok"], json!(true));
    h.events.borrow_mut().clear();
    let inode = h.path.metadata().unwrap().ino();
    let result = h.execute(&h.leaves(), false);
    assert_eq!(result["ok"], json!(true), "{result}");
    assert!(
        result["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["status"] == json!("already-ready"))
    );
    assert!(!h.has("structured-write") && !h.has("snapshot") && !h.has("validator"));
    assert_eq!(h.path.metadata().unwrap().ino(), inode);
}

#[test]
fn validator_runs_for_installer_only() {
    let h = Harness::new();
    let before = std::fs::read(&h.path).unwrap();
    let result = h.execute(std::slice::from_ref(&h.app), false);
    assert_eq!(result["ok"], json!(true), "{result}");
    assert!(h.index("installer") < h.index("validator"));
    assert!(!h.has("structured-write"));
    assert_eq!(std::fs::read(&h.path).unwrap(), before);
}

#[test]
fn installer_only_failure_preserves_config_and_does_not_record_state() {
    let h = Harness::new();
    let before = std::fs::read(&h.path).unwrap();
    *h.report.borrow_mut() = report(&json!("fail"), "fail");
    let result = h.execute(std::slice::from_ref(&h.app), false);
    assert_eq!(result["ok"], json!(false));
    assert!(
        result["results"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("config.load=fail")
    );
    assert_eq!(std::fs::read(&h.path).unwrap(), before);
    assert!(!h.has("structured-restore") && !h.has("state-record"));
}

#[test]
fn failed_validation_restores_original_bytes_and_mode() {
    let h = Harness::new();
    let before = std::fs::read(&h.path).unwrap();
    std::fs::set_permissions(&h.path, std::fs::Permissions::from_mode(0o640)).unwrap();
    *h.report.borrow_mut() = report(&json!("fail"), "fail");
    let result = h.execute(std::slice::from_ref(&h.app), true);
    assert_eq!(result["ok"], json!(false));
    assert_eq!(std::fs::read(&h.path).unwrap(), before);
    assert_eq!(h.path.metadata().unwrap().permissions().mode() & 0o777, 0o640);
    assert_eq!(h.count("structured-restore"), 1);
    assert!(!h.has("state-record"));
}

#[test]
fn failed_validation_removes_only_new_generated_file() {
    let h = Harness::new();
    std::fs::remove_file(&h.path).unwrap();
    *h.report.borrow_mut() = report(&json!("fail"), "fail");
    let result = h.execute(&h.leaves(), false);
    assert_eq!(result["ok"], json!(false));
    assert!(!h.path.exists());
    assert!(!h.has("state-record"));
}

#[test]
fn invalid_status_types_restore_generated_configuration() {
    let h = Harness::new();
    let before = std::fs::read(&h.path).unwrap();
    for status in [json!({"status": "fail"}), json!(["fail"]), Value::Null] {
        *h.report.borrow_mut() = report(&status, "fail");
        h.events.borrow_mut().clear();
        let result = h.execute(&h.leaves(), false);
        assert_eq!(result["ok"], json!(false));
        assert_eq!(std::fs::read(&h.path).unwrap(), before);
        assert!(h.has("structured-restore"));
        assert!(!h.has("state-record"));
    }
}

#[test]
fn structural_conflict_blocks_installer_and_all_mutations() {
    let h = Harness::new();
    std::fs::write(&h.path, "tui = \"not a table\"\n").unwrap();
    let before = std::fs::read(&h.path).unwrap();
    let result = h.execute(std::slice::from_ref(&h.app), true);
    assert_eq!(result["ok"], json!(false));
    assert!(!h.has("installer") && !h.has("snapshot") && !h.has("structured-write"));
    assert_eq!(std::fs::read(&h.path).unwrap(), before);
}

fn by_key(state: &Value) -> Vec<(Vec<String>, Value)> {
    state["items"]
        .as_object()
        .unwrap()
        .values()
        .filter(|item| item.get("key").is_some())
        .map(|item| {
            let key = item["key"]
                .as_array()
                .unwrap()
                .iter()
                .map(|k| k.as_str().unwrap().to_owned())
                .collect();
            (key, item.clone())
        })
        .collect()
}

fn find(items: &[(Vec<String>, Value)], key: &[&str]) -> Value {
    items
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, item)| item.clone())
        .unwrap()
}

#[test]
fn leaf_inspection_is_batched_and_exposes_only_declared_values() {
    let h = Harness::new();
    std::fs::write(
        &h.path,
        "approval_policy = \"never\"\nsandbox_mode = \"other\"\nmodel = \"SECRET\"\n",
    )
    .unwrap();
    let mut workspace = h.workspace();
    let state = inspect_workspace_state(&mut workspace, &h.cfg, &target(), std::slice::from_ref(&h.app)).unwrap();
    let items = by_key(&state);
    assert_eq!(find(&items, &["approval_policy"])["state"], json!("matching"));
    assert_eq!(find(&items, &["sandbox_mode"])["state"], json!("different"));
    assert_eq!(find(&items, &["approvals_reviewer"])["state"], json!("missing"));
    assert_eq!(find(&items, &["approval_policy"])["current"], json!("never"));
    assert_eq!(find(&items, &["sandbox_mode"])["current"], json!("other"));
    assert!(find(&items, &["approvals_reviewer"]).get("current").is_none());
    assert_eq!(h.count("structured-read"), 1);
    assert!(!state.to_string().contains("SECRET"));
}

#[test]
fn structural_inspection_keeps_independent_leaf_states() {
    let h = Harness::new();
    std::fs::write(&h.path, "approval_policy = \"never\"\ntui = \"incompatible\"\n").unwrap();
    let mut workspace = h.workspace();
    let state = inspect_workspace_state(&mut workspace, &h.cfg, &target(), std::slice::from_ref(&h.app)).unwrap();
    let items = by_key(&state);
    assert_eq!(find(&items, &["approval_policy"])["state"], json!("matching"));
    assert_eq!(find(&items, &["approvals_reviewer"])["state"], json!("missing"));
    assert_eq!(find(&items, &["tui", "status_line"])["state"], json!("unavailable"));
    assert_eq!(h.count("structured-read"), 1);
}

#[test]
fn file_copy_conflicts_with_structured_patch_but_siblings_do_not() {
    let h = Harness::new();
    let copy = Entry {
        id: "copy".into(),
        group: "files".into(),
        handler: "file".into(),
        label: "Copy".into(),
        description: "Copy whole file".into(),
        params: Params::File {
            path: "~/.codex/config.toml".into(),
            application: None,
        },
        depends_on: Vec::new(),
    };
    let leaves = h.leaves();
    let error = build_plan(&h.cfg, &target(), &[copy, leaves[0].clone()], None, false, true).unwrap_err();
    assert!(error.0.contains("Overlapping"), "{}", error.0);
    let plan = build_plan(&h.cfg, &target(), &leaves, None, false, true).unwrap();
    assert_eq!(plan.entries.len(), leaves.len());
    let Params::Structured { key, .. } = &leaves[0].params else {
        panic!()
    };
    assert_eq!(plan.public()["actions"][0]["key"], json!(key));
}

#[test]
fn multiple_config_files_validate_once_and_restore_only_changed_files() {
    let mut h = Harness::new();
    let first = h.app.application().unwrap().config_files[0].clone();
    let mut enabled = toml::Table::new();
    enabled.insert("enabled".into(), toml::Value::Boolean(true));
    let mut second = first.clone();
    second.id = "other".into();
    second.path = "~/.codex/other.json".into();
    second.format = "json".into();
    second.values = enabled.clone();
    let mut unchanged = first.clone();
    unchanged.id = "unchanged".into();
    unchanged.path = "~/.codex/unchanged.yaml".into();
    unchanged.format = "yaml".into();
    unchanged.values = enabled;
    let other_path = h.home.join(".codex/other.json");
    std::fs::write(&other_path, "{\"unrelated\": \"preserve\"}\n").unwrap();
    let unchanged_path = h.home.join(".codex/unchanged.yaml");
    std::fs::write(&unchanged_path, "enabled: true\ncustom: preserve\n").unwrap();
    let paths = [h.path.clone(), other_path, unchanged_path];
    let before: Vec<Vec<u8>> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    h.app = with_params(&h.app, |p| p.config_files = vec![first, second, unchanged]);
    *h.report.borrow_mut() = report(&json!("fail"), "fail");
    let result = h.execute(std::slice::from_ref(&h.app), true);
    assert_eq!(result["ok"], json!(false));
    assert_eq!(h.count("validator"), 1);
    assert_eq!(h.count("structured-write"), 2);
    assert_eq!(h.count("structured-restore"), 2);
    let last_write = h.events.borrow().iter().rposition(|e| e == "structured-write").unwrap();
    assert!(last_write < h.index("validator"));
    assert_eq!(
        paths.iter().map(|p| std::fs::read(p).unwrap()).collect::<Vec<_>>(),
        before
    );
    assert!(!h.has("state-record"));
}

#[test]
fn application_without_validator_still_parses_all_formats() {
    let mut h = Harness::new();
    for (format, content) in [("toml", "broken = ["), ("json", "{\"broken\":"), ("yaml", "broken: [")] {
        let mut config = h.app.application().unwrap().config_files[0].clone();
        config.format = format.into();
        config.values = toml::Table::new();
        config.values.insert("enabled".into(), toml::Value::Boolean(true));
        let application = with_params(&h.app, |p| {
            p.validation = None;
            p.config_files = vec![config];
        });
        std::fs::write(&h.path, content).unwrap();
        h.events.borrow_mut().clear();
        let result = h.execute(std::slice::from_ref(&application), true);
        assert_eq!(result["ok"], json!(false), "{format}");
        assert!(!h.has("installer") && !h.has("validator"));
        assert_eq!(std::fs::read_to_string(&h.path).unwrap(), content);
        std::fs::remove_file(&h.path).unwrap();
        h.events.borrow_mut().clear();
        let result = h.execute(std::slice::from_ref(&application), true);
        assert_eq!(result["ok"], json!(true), "{format}: {result}");
        assert!(!h.has("validator"));
    }
    h.app = h.cfg.item("codex");
}

#[test]
fn failed_application_does_not_restore_another_applications_config() {
    let h = Harness::new();
    let mut other_file = h.app.application().unwrap().config_files[0].clone();
    other_file.id = "other".into();
    other_file.path = "~/other.json".into();
    other_file.format = "json".into();
    other_file.values = toml::Table::new();
    other_file.values.insert("enabled".into(), toml::Value::Boolean(true));
    let mut other = with_params(&h.app, |p| {
        p.validation = None;
        p.config_files = vec![other_file];
    });
    other.id = "other".into();
    other.label = "Other".into();
    let before = std::fs::read(&h.path).unwrap();
    *h.report.borrow_mut() = report(&json!("fail"), "fail");
    let result = h.execute(&[other, h.app.clone()], true);
    assert_eq!(result["ok"], json!(false));
    let written: Value = serde_json::from_str(&std::fs::read_to_string(h.home.join("other.json")).unwrap()).unwrap();
    assert_eq!(written, json!({"enabled": true}));
    assert_eq!(std::fs::read(&h.path).unwrap(), before);
    let registry: Value = serde_json::from_str(&h.registry()).unwrap();
    assert!(registry["items"].get("other").is_some());
    assert!(registry["items"].get("codex").is_none());
}

#[test]
fn installer_changes_are_preserved_in_merged_final_document() {
    let h = Harness::new();
    *h.install_writes.borrow_mut() = Some("model = \"installer-choice\"\n".into());
    let result = h.execute(std::slice::from_ref(&h.app), true);
    assert_eq!(result["ok"], json!(true), "{result}");
    let text = std::fs::read_to_string(&h.path).unwrap();
    assert!(text.contains("model = \"installer-choice\""), "{text}");
    assert!(text.contains("approval_policy = \"never\""), "{text}");
}

#[test]
fn config_change_after_preflight_blocks_patch() {
    let h = Harness::new();
    *h.after_snapshot.borrow_mut() = Some("model = \"concurrent-edit\"\n".into());
    let result = h.execute(&h.leaves(), true);
    assert_eq!(result["ok"], json!(false));
    assert_eq!(
        std::fs::read_to_string(&h.path).unwrap(),
        "model = \"concurrent-edit\"\n"
    );
    assert!(!h.has("structured-write"));
}

fn encoded(content: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(content)
}

fn guest_op(home: &Path, values: Value) -> Value {
    let mut payload = json!({"home": home.display().to_string()});
    payload
        .as_object_mut()
        .unwrap()
        .extend(values.as_object().unwrap().clone());
    run_guest(&payload)
}

#[test]
fn read_missing_and_write_new_file_uses_safe_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let missing = guest_op(
        home,
        json!({"operation": "structured-read", "relative": ".codex/config.toml"}),
    );
    assert_eq!(
        missing,
        json!({"ok": true, "content": null, "sha256": null, "mode": null})
    );
    let content = b"approval_policy = 'never'\n";
    let result = guest_op(
        home,
        json!({"operation": "structured-write", "relative": ".codex/config.toml",
                                       "content": encoded(content), "expected_sha256": null}),
    );
    assert_eq!(result, json!({"ok": true, "changed": true}));
    assert_eq!(std::fs::read(home.join(".codex/config.toml")).unwrap(), content);
    assert_eq!(
        home.join(".codex/config.toml").metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn write_preserves_mode_and_noop_keeps_inode() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let path = home.join(".codex/config.toml");
    std::fs::create_dir(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "approval_policy = 'user'\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let inode = path.metadata().unwrap().ino();
    let state = guest_op(
        home,
        json!({"operation": "structured-read", "relative": ".codex/config.toml"}),
    );
    assert_eq!(state["mode"], json!(0o640));
    let no_op = guest_op(
        home,
        json!({"operation": "structured-write", "relative": ".codex/config.toml",
                                      "content": state["content"], "expected_sha256": state["sha256"]}),
    );
    assert_eq!(no_op, json!({"ok": true, "changed": false}));
    assert_eq!(path.metadata().unwrap().ino(), inode);
    let updated = b"approval_policy = 'never'\n";
    let changed = guest_op(
        home,
        json!({"operation": "structured-write", "relative": ".codex/config.toml",
                                        "content": encoded(updated), "expected_sha256": state["sha256"]}),
    );
    assert_eq!(changed, json!({"ok": true, "changed": true}));
    assert_eq!(std::fs::read(&path).unwrap(), updated);
    assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o640);
}

#[test]
fn write_cas_conflict_leaves_file_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::write(home.join("config.json"), "{\"one\": 1}\n").unwrap();
    let refused = guest_op(
        home,
        json!({"operation": "structured-write", "relative": "config.json",
                                       "content": encoded(b"{\"one\": 2}\n"), "expected_sha256": sha256_hex(b"different")}),
    );
    assert!(refused["error"].as_str().unwrap().contains("CAS conflict"), "{refused}");
    assert_eq!(
        std::fs::read_to_string(home.join("config.json")).unwrap(),
        "{\"one\": 1}\n"
    );
}

#[test]
fn fifo_is_rejected_without_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(tmp.path().join("config.toml"))
        .status()
        .unwrap();
    assert!(status.success());
    let refused = guest_op(
        tmp.path(),
        json!({"operation": "structured-read", "relative": "config.toml"}),
    );
    assert!(refused["error"].as_str().unwrap().contains("regular file"), "{refused}");
}

#[test]
fn symlink_is_rejected_for_read_write_and_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::write(home.join("target"), "private").unwrap();
    std::os::unix::fs::symlink(home.join("target"), home.join("config.toml")).unwrap();
    for values in [
        json!({"operation": "structured-read", "relative": "config.toml"}),
        json!({"operation": "structured-write", "relative": "config.toml", "content": encoded(b"new"), "expected_sha256": sha256_hex(b"private")}),
        json!({"operation": "structured-restore", "relative": "config.toml", "snapshot": null, "expected_sha256": sha256_hex(b"private"), "existed": false}),
    ] {
        let refused = guest_op(home, values);
        let error = refused["error"].as_str().unwrap().to_lowercase();
        assert!(error.contains("symlink"), "{refused}");
    }
    assert!(
        home.join("config.toml")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_to_string(home.join("target")).unwrap(), "private");
}

#[test]
fn restore_existing_file_uses_snapshot_and_preserves_original_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let path = home.join(".codex/config.toml");
    std::fs::create_dir(path.parent().unwrap()).unwrap();
    let original = b"approval_policy = 'user'\n";
    std::fs::write(&path, original).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let snapshot = guest_op(
        home,
        json!({"operation": "snapshot", "paths": [".codex/config.toml"], "items": ["codex"],
                                         "vmid": 200, "name": "workspace"}),
    );
    assert_eq!(snapshot["created"], json!(true));
    let candidate = b"approval_policy = 'never'\n";
    guest_op(
        home,
        json!({"operation": "structured-write", "relative": ".codex/config.toml",
                          "content": encoded(candidate), "expected_sha256": sha256_hex(original)}),
    );
    let restored = guest_op(
        home,
        json!({"operation": "structured-restore", "relative": ".codex/config.toml",
                                         "snapshot": snapshot["id"], "expected_sha256": sha256_hex(candidate), "existed": true}),
    );
    assert_eq!(restored, json!({"ok": true, "changed": true}));
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o640);
}

#[test]
fn restore_new_file_removes_only_matching_candidate_without_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let candidate = b"{\"new\": true}\n";
    guest_op(
        home,
        json!({"operation": "structured-write", "relative": ".config/new.json",
                          "content": encoded(candidate), "expected_sha256": null}),
    );
    let restored = guest_op(
        home,
        json!({"operation": "structured-restore", "relative": ".config/new.json",
                                         "snapshot": null, "expected_sha256": sha256_hex(candidate), "existed": false}),
    );
    assert_eq!(restored, json!({"ok": true, "changed": true}));
    assert!(!home.join(".config/new.json").exists());
}

#[test]
fn restore_rejects_candidate_cas_mismatch_and_snapshot_membership() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::write(home.join("config.toml"), "before").unwrap();
    let snapshot = guest_op(
        home,
        json!({"operation": "snapshot", "paths": ["config.toml"], "items": ["codex"],
                                         "vmid": 200, "name": "workspace"}),
    );
    std::fs::write(home.join("config.toml"), "after").unwrap();
    let refused = guest_op(
        home,
        json!({"operation": "structured-restore", "relative": "config.toml", "snapshot": snapshot["id"],
                                        "expected_sha256": sha256_hex(b"different"), "existed": true}),
    );
    assert!(refused["error"].as_str().unwrap().contains("CAS conflict"), "{refused}");
    let refused = guest_op(
        home,
        json!({"operation": "structured-restore", "relative": "other.toml", "snapshot": snapshot["id"],
                                        "expected_sha256": sha256_hex(b"after"), "existed": true}),
    );
    assert!(refused["error"].as_str().unwrap().contains("not part"), "{refused}");
}

const CREDENTIALS: &str = "~/.claude/.credentials.json";
const STATUSLINE: &str = "~/.claude/statusline.sh";

fn claude_settings(home: &Path, files: &[&str]) -> Result<Settings, AppError> {
    let mut cfg = settings(home);
    let list: Vec<String> = files.iter().map(|f| format!("{f:?}")).collect();
    cfg.setup = parse_setup(
        &format!("[[items]]\nid = \"claude\"\nfiles = [{}]\n", list.join(", "))
            .parse()
            .unwrap(),
    )?;
    Ok(cfg)
}

fn desktop_home() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".claude")).unwrap();
    std::fs::write(tmp.path().join(".claude/.credentials.json"), "{}").unwrap();
    std::fs::write(tmp.path().join(".claude/statusline.sh"), "#!/bin/sh\n").unwrap();
    tmp
}

#[test]
fn files_expand_to_owned_file_entries_and_round_trip() {
    let home = tempfile::tempdir().unwrap();
    let cfg = claude_settings(home.path(), &[CREDENTIALS, STATUSLINE]).unwrap();
    let claude = cfg.item("claude");
    let children = app_file_entries(&claude);
    assert_eq!(
        children.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        [
            "claude:file:~/.claude/.credentials.json",
            "claude:file:~/.claude/statusline.sh"
        ]
    );
    assert!(children.iter().all(|e| e.group == "app" && e.handler == "file"));
    assert!(
        children
            .iter()
            .all(|e| matches!(&e.params, Params::File { application: Some(a), .. } if a == "claude"))
    );
    let mut document = toml::Table::new();
    let items: Vec<toml::Value> = cfg
        .setup
        .items
        .iter()
        .map(|e| toml::Value::Table(e.definition()))
        .collect();
    let groups: Vec<toml::Value> = cfg
        .setup
        .groups
        .iter()
        .map(|g| {
            let mut t = toml::Table::new();
            t.insert("id".into(), toml::Value::String(g.id.clone()));
            t.insert("label".into(), toml::Value::String(g.label.clone()));
            t.insert("description".into(), toml::Value::String(g.description.clone()));
            toml::Value::Table(t)
        })
        .collect();
    document.insert("groups".into(), toml::Value::Array(groups));
    document.insert("items".into(), toml::Value::Array(items));
    let text = toml::to_string(&document).unwrap();
    assert_eq!(parse_setup(&text.parse().unwrap()).unwrap(), cfg.setup);
}

#[test]
fn unsafe_or_duplicate_files_are_rejected() {
    let home = tempfile::tempdir().unwrap();
    for files in [&["/etc/passwd"][..], &["~/../x"], &[STATUSLINE, STATUSLINE], &[""]] {
        assert!(claude_settings(home.path(), files).is_err(), "{files:?}");
    }
}

#[test]
fn file_items_cannot_claim_an_application() {
    let text = "[[items]]\nid = \"x\"\ngroup = \"files\"\nhandler = \"file\"\nlabel = \"X\"\ndescription = \"\"\npath = \"~/.x\"\napplication = \"claude\"\n";
    assert!(parse_setup(&text.parse().unwrap()).is_err());
}

#[test]
fn application_selection_copies_its_files_after_installation() {
    let home = desktop_home();
    let cfg = claude_settings(home.path(), &[CREDENTIALS, STATUSLINE]).unwrap();
    let plan = build_plan(&cfg, &target(), &[cfg.item("claude")], None, false, true).unwrap();
    assert_eq!(
        plan.entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        [
            "claude",
            "claude:file:~/.claude/.credentials.json",
            "claude:file:~/.claude/statusline.sh"
        ]
    );
}

#[test]
fn single_file_can_be_selected_by_stable_id() {
    let home = desktop_home();
    let cfg = claude_settings(home.path(), &[CREDENTIALS, STATUSLINE]).unwrap();
    let identity = || -> Result<Value, AppError> { panic!("unexpected GitHub") };
    let catalog = || -> Catalog { panic!("unexpected GitHub") };
    let github = Github {
        identity: &identity,
        catalog: &catalog,
    };
    let (entries, _) = select_entries(
        &cfg,
        &["app=claude:file:~/.claude/statusline.sh".to_owned()],
        None,
        &github,
    )
    .unwrap();
    assert_eq!(
        entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["claude:file:~/.claude/statusline.sh"]
    );
    let plan = build_plan(&cfg, &target(), &entries, None, false, true).unwrap();
    assert_eq!(
        plan.entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["claude:file:~/.claude/statusline.sh"]
    );
}

#[test]
fn catalog_reports_desktop_availability_per_file() {
    let home = desktop_home();
    std::fs::remove_file(home.path().join(".claude/statusline.sh")).unwrap();
    let cfg = claude_settings(home.path(), &[CREDENTIALS, STATUSLINE]).unwrap();
    let catalog = load_catalog(&cfg, None);
    let row = catalog
        .rows(&cfg)
        .into_iter()
        .find(|r| r["id"] == json!("claude"))
        .unwrap();
    let files: Vec<(String, String)> = row["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["path"].as_str().unwrap().to_owned(),
                f["availability"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        files,
        [
            (CREDENTIALS.to_owned(), "ready".to_owned()),
            (STATUSLINE.to_owned(), "missing".to_owned())
        ]
    );
}

#[test]
fn the_validator_of_the_example_is_python_s() {
    let home = tempfile::tempdir().unwrap();
    let cfg = example_settings(home.path());
    assert_eq!(
        cfg.item("codex").application().unwrap().validation,
        Some(Validation {
            command: "codex doctor --json".into(),
            kind: "json-path".into(),
            path: vec!["checks".into(), "config.load".into(), "status".into()],
            accepted: vec!["ok".into(), "warning".into()],
        })
    );
}
