//! The Backup group: BK on the workspace and its retrieval keys on the
//! desktop (Python `test_setup_backup.py`, `test_setup_backup_retrieval.py`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod setup_fixture;

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use base64::Engine;
use common::FakeSystem;
use hs_cli::AppError;
use hs_cli::setup::backup::{
    ASSETS, DIRECTORIES, apply, inspect, managed_paths, managed_state_paths, reconcile_retrieval_keys,
};
use hs_cli::setup::catalog::{Catalog, Github, load_catalog, save_snapshot, select_entries};
use hs_cli::setup::definitions::{Entry, Params, defaults};
use hs_cli::setup::engine::{Connection, Quiet, execute_plan, inspect_workspace_state};
use hs_cli::setup::plan::build_plan;
use hs_cli::setup::{Settings, sha256_hex};
use hs_cli::system::Output;
use serde_json::{Value, json};
use setup_fixture::{TestWorkspace, run_guest, settings, target};

fn bk() -> Entry {
    defaults().into_iter().find(|e| e.id == "bk").unwrap()
}

/// Python's `FakeWorkspace`: systemctl answers from a timer state.
#[derive(Clone)]
struct Timer(Rc<RefCell<(bool, bool)>>);

impl Timer {
    fn new(enabled: bool, active: bool) -> Self {
        Self(Rc::new(RefCell::new((enabled, active))))
    }

    fn answer(&self, commands: Rc<RefCell<Vec<String>>>) -> impl FnMut(&str, bool) -> Result<Output, AppError> + use<> {
        let state = Rc::clone(&self.0);
        move |command, _| {
            commands.borrow_mut().push(command.to_owned());
            if command.contains("systemctl --user enable --now backup.timer") {
                *state.borrow_mut() = (true, true);
            }
            let (enabled, active) = *state.borrow();
            let code = if command.contains("systemctl --user is-enabled backup.timer") {
                i32::from(!enabled)
            } else if command.contains("systemctl --user is-active backup.timer") {
                if active { 0 } else { 3 }
            } else {
                0
            };
            Ok(Output {
                code,
                ..Output::default()
            })
        }
    }
}

/// BK's files by path: content and mode.
type Files = HashMap<String, (Vec<u8>, u32)>;

/// Python's `MemoryGuest`: BK's files kept in memory.
#[derive(Clone, Default)]
struct Memory {
    files: Rc<RefCell<Files>>,
    configuration: Rc<RefCell<bool>>,
    calls: Rc<RefCell<Vec<(String, Value)>>>,
}

impl Memory {
    fn with(files: HashMap<String, (Vec<u8>, u32)>) -> Self {
        let memory = Self::default();
        *memory.files.borrow_mut() = files;
        *memory.configuration.borrow_mut() = true;
        memory
    }

    fn answer(&self) -> impl FnMut(&str, &Value) -> Result<Value, AppError> + use<> {
        let memory = self.clone();
        move |operation, values| {
            memory.calls.borrow_mut().push((operation.to_owned(), values.clone()));
            Ok(match operation {
                "backup-config" => {
                    let create = values["create"] == json!(true);
                    let mut configured = memory.configuration.borrow_mut();
                    let created = create && !*configured;
                    *configured |= create;
                    json!({"ok": true, "exists": *configured, "created": created})
                }
                "backup-authorized-keys" => {
                    json!({"ok": true, "ready": true, "changed": false, "exists": true, "sha256": "authorized-keys-digest"})
                }
                "managed-files-inspect" => {
                    let files = memory.files.borrow();
                    let items: Vec<Value> = values["paths"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|path| {
                            let path = path.as_str().unwrap();
                            match files.get(path) {
                                None => json!({"path": path, "exists": false}),
                                Some((content, mode)) => json!({"path": path, "exists": true, "type": "file",
                                    "sha256": sha256_hex(content), "size": content.len(), "mode": mode}),
                            }
                        })
                        .collect();
                    json!({"ok": true, "items": items})
                }
                "managed-files-install" => {
                    let mut changed = Vec::new();
                    for item in values["files"].as_array().unwrap() {
                        let content = base64::engine::general_purpose::STANDARD
                            .decode(item["content"].as_str().unwrap())
                            .unwrap();
                        let path = item["path"].as_str().unwrap().to_owned();
                        memory.files.borrow_mut().insert(
                            path.clone(),
                            (content, u32::try_from(item["mode"].as_u64().unwrap()).unwrap()),
                        );
                        changed.push(path);
                    }
                    json!({"ok": true, "changed": changed})
                }
                _ => json!({"ok": true}),
            })
        }
    }

    fn installs(&self) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|(op, _)| op == "managed-files-install")
            .count()
    }
}

fn desired_files() -> HashMap<String, (Vec<u8>, u32)> {
    ASSETS
        .iter()
        .map(|asset| (asset.path.to_owned(), (asset.content.to_vec(), asset.mode)))
        .collect()
}

/// A desktop home with both retrieval key pairs of VM 200.
fn desktop() -> (tempfile::TempDir, Settings) {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = settings(tmp.path());
    reconcile_retrieval_keys(&cfg.home, 200).unwrap();
    (tmp, cfg)
}

fn workspace(home: &Path, memory: &Memory, timer: &Timer, commands: &Rc<RefCell<Vec<String>>>) -> TestWorkspace {
    let mut workspace = TestWorkspace::scripted(home, memory.answer());
    workspace.answer = Box::new(timer.answer(Rc::clone(commands)));
    workspace
}

#[test]
fn the_bk_assets_match_the_final_python_reference() {
    let hashes: Value =
        serde_json::from_str(include_str!("../../../parity/fixtures/guest-assets-sha256.json")).unwrap();
    for (asset, name) in ASSETS.iter().zip(["bk.py", "backup.service", "backup.timer"]) {
        assert_eq!(sha256_hex(asset.content), hashes[name], "{name}");
    }
}

#[test]
fn authorized_keys_cannot_be_replaced_by_another_selected_handler() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".ssh")).unwrap();
    std::fs::write(home.path().join(".ssh/authorized_keys"), "# Desktop keys\n").unwrap();
    let login_keys = Entry {
        id: "login-keys".into(),
        group: "files".into(),
        handler: "file".into(),
        label: "Login keys".into(),
        description: "User SSH keys".into(),
        params: Params::File {
            path: "~/.ssh/authorized_keys".into(),
            application: None,
        },
        depends_on: Vec::new(),
    };
    let error = build_plan(
        &settings(home.path()),
        &target(),
        &[login_keys, bk()],
        None,
        false,
        true,
    )
    .unwrap_err();
    assert!(error.0.contains("Overlapping selected writes"), "{}", error.0);
}

#[test]
fn group_item_selectors_catalog_and_plan_are_first_class() {
    let home = tempfile::tempdir().unwrap();
    let cfg = settings(home.path());
    let groups: Vec<&str> = cfg.setup.groups.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(groups, ["files", "env", "app", "cred", "backup", "repo"]);
    assert_eq!(cfg.setup.groups[4].label, "Backup");
    let entry = bk();
    assert_eq!((entry.group.as_str(), entry.handler.as_str()), ("backup", "backup"));
    assert_eq!(entry.params, Params::Backup);
    let identity = || -> Result<Value, AppError> { panic!("unexpected GitHub") };
    let repos = || -> Catalog { panic!("unexpected GitHub") };
    let github = Github {
        identity: &identity,
        catalog: &repos,
    };
    for selector in ["backup=bk", "b=bk"] {
        let (selected, catalog_id) = select_entries(&cfg, &[selector.to_owned()], None, &github).unwrap();
        assert_eq!(selected, std::slice::from_ref(&entry));
        assert!(catalog_id.is_none());
    }
    assert_eq!(
        select_entries(&cfg, &["e=bash".to_owned()], None, &github).unwrap().0[0].id,
        "bash"
    );
    assert_eq!(
        select_entries(&cfg, &["a=codex".to_owned()], None, &github).unwrap().0[0].id,
        "codex"
    );
    let mut catalog = load_catalog(&cfg, None);
    let snapshot = save_snapshot(&cfg, &mut catalog).unwrap();
    let (numeric, used) = select_entries(&cfg, &["backup=1".to_owned()], Some(&snapshot), &github).unwrap();
    assert_eq!(numeric, std::slice::from_ref(&entry));
    assert_eq!(used.as_deref(), Some(snapshot.as_str()));
    let row = catalog
        .rows(&cfg)
        .into_iter()
        .find(|row| row["id"] == json!("bk"))
        .unwrap();
    assert_eq!(row["group"], json!("backup"));
    assert!(row["path_or_repository"].as_str().unwrap().contains("~/.local/bin/bk"));
    let plan = build_plan(&cfg, &target(), &[entry], None, false, true).unwrap();
    let action = &plan.public()["actions"][0];
    assert_eq!(action["handler"], json!("backup"));
    let destinations: Vec<String> = managed_paths().iter().map(|p| format!("~/{p}")).collect();
    assert_eq!(action["destinations"], json!(destinations));
}

#[test]
fn resources_load_from_the_homestack_package() {
    let paths: Vec<&str> = ASSETS.iter().map(|a| a.path).collect();
    assert_eq!(paths, managed_paths());
    assert!(ASSETS[0].content.starts_with(b"#!/usr/bin/env python3"));
    assert!(String::from_utf8_lossy(ASSETS[1].content).contains("ExecStart=%h/.local/bin/bk run"));
    assert!(String::from_utf8_lossy(ASSETS[2].content).contains("Persistent=true"));
}

fn install_payload(expected: &HashMap<String, Value>) -> Value {
    let files: Vec<Value> = ASSETS
        .iter()
        .map(|asset| {
            json!({"path": asset.path,
                   "content": base64::engine::general_purpose::STANDARD.encode(asset.content),
                   "sha256": sha256_hex(asset.content),
                   "expected_sha256": expected.get(asset.path).cloned().unwrap_or(Value::Null),
                   "mode": asset.mode})
        })
        .collect();
    json!(files)
}

#[test]
fn guest_install_is_atomic_idempotent_and_preserves_runtime_data() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let runtime = [
        ("backup/backup.yaml", "user configuration\n"),
        ("backup/backup.tgz", "archive\n"),
        ("backup/backup.log", "log\n"),
        ("backup/status.json", "{}\n"),
        ("backup/.bk.lock", ""),
        ("backup/archive/old.tgz", "history\n"),
        ("backup/.work/keep", "transient\n"),
    ];
    for (relative, content) in runtime {
        std::fs::create_dir_all(home.join(relative).parent().unwrap()).unwrap();
        std::fs::write(home.join(relative), content).unwrap();
    }
    let result = run_guest(
        &json!({"operation": "managed-files-install", "home": home.display().to_string(),
                                   "directories": DIRECTORIES, "files": install_payload(&HashMap::new())}),
    );
    let mut changed: Vec<String> = result["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap().to_owned())
        .collect();
    changed.sort();
    let mut expected = managed_paths();
    expected.sort();
    assert_eq!(changed, expected);
    let inspected = run_guest(
        &json!({"operation": "managed-files-inspect", "home": home.display().to_string(), "paths": managed_paths()}),
    );
    let digests: HashMap<String, Value> = inspected["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| (item["path"].as_str().unwrap().to_owned(), item["sha256"].clone()))
        .collect();
    for asset in &ASSETS {
        let path = home.join(asset.path);
        assert_eq!(std::fs::read(&path).unwrap(), asset.content);
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, asset.mode);
    }
    assert!(home.join("backup").is_dir());
    for (relative, content) in runtime {
        assert_eq!(std::fs::read_to_string(home.join(relative)).unwrap(), content);
    }
    let inodes: Vec<u64> = managed_paths()
        .iter()
        .map(|p| home.join(p).metadata().unwrap().ino())
        .collect();
    let repeated = run_guest(
        &json!({"operation": "managed-files-install", "home": home.display().to_string(),
                                     "directories": DIRECTORIES, "files": install_payload(&digests)}),
    );
    assert_eq!(repeated["changed"], json!([]));
    assert_eq!(
        managed_paths()
            .iter()
            .map(|p| home.join(p).metadata().unwrap().ino())
            .collect::<Vec<_>>(),
        inodes
    );
}

#[test]
fn guest_install_creates_only_required_directories_without_configuration() {
    let tmp = tempfile::tempdir().unwrap();
    run_guest(
        &json!({"operation": "managed-files-install", "home": tmp.path().display().to_string(),
                      "directories": DIRECTORIES, "files": []}),
    );
    for relative in DIRECTORIES {
        assert!(tmp.path().join(relative).is_dir());
    }
    assert!(!tmp.path().join("backup/backup.yaml").exists());
}

#[test]
fn guest_install_rejects_preflight_race() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join(".local/bin/bk");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "changed after preflight").unwrap();
    let content = ASSETS[0].content;
    let refused = run_guest(
        &json!({"operation": "managed-files-install", "home": tmp.path().display().to_string(),
        "directories": DIRECTORIES,
        "files": [{"path": ".local/bin/bk", "content": base64::engine::general_purpose::STANDARD.encode(content),
                   "sha256": sha256_hex(content), "expected_sha256": "0".repeat(64), "mode": 0o755}]}),
    );
    assert!(
        refused["error"].as_str().unwrap().contains("changed since preflight"),
        "{refused}"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "changed after preflight");
}

#[test]
fn inspection_distinguishes_missing_current_drift_and_disabled_timer() {
    let (tmp, cfg) = desktop();
    let commands = Rc::default();
    let mut ws = workspace(tmp.path(), &Memory::default(), &Timer::new(false, false), &commands);
    let missing = inspect(&mut ws, &cfg, 200, true).unwrap();
    assert_eq!(missing["state"], json!("not installed"));
    assert_eq!(missing["ready"], json!(false));
    let timer = Timer::new(true, true);
    let mut ws = workspace(tmp.path(), &Memory::with(desired_files()), &timer, &commands);
    let current = inspect(&mut ws, &cfg, 200, true).unwrap();
    assert_eq!(current["state"], json!("configured"), "{current}");
    assert_eq!(current["ready"], json!(true));
    let mut files = desired_files();
    files.insert(".local/bin/bk".into(), (b"old BK".to_vec(), 0o755));
    let timer_file = files[".config/systemd/user/backup.timer"].0.clone();
    files.insert(".config/systemd/user/backup.timer".into(), (timer_file, 0o600));
    let mut ws = workspace(tmp.path(), &Memory::with(files), &timer, &commands);
    let drifted = inspect(&mut ws, &cfg, 200, true).unwrap();
    assert_eq!(drifted["state"], json!("needs update"));
    let mut snapshot: Vec<&str> = drifted["snapshot_paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    snapshot.sort_unstable();
    assert_eq!(snapshot, [".config/systemd/user/backup.timer", ".local/bin/bk"]);
    timer.0.borrow_mut().0 = false;
    let mut ws = workspace(tmp.path(), &Memory::with(desired_files()), &timer, &commands);
    let disabled = inspect(&mut ws, &cfg, 200, true).unwrap();
    assert_eq!(disabled["state"], json!("needs update"));
    assert_eq!(disabled["will_overwrite"], json!(false));
    for relative in managed_paths() {
        let mut files = desired_files();
        let mode = files[&relative].1;
        files.insert(relative.clone(), (b"outdated".to_vec(), mode));
        let mut ws = workspace(tmp.path(), &Memory::with(files), &Timer::new(true, true), &commands);
        let drifted = inspect(&mut ws, &cfg, 200, true).unwrap();
        assert_eq!(drifted["state"], json!("needs update"));
        assert!(drifted["changed_paths"].as_array().unwrap().contains(&json!(relative)));
    }
}

#[test]
fn preflight_checks_runtime_and_user_systemd() {
    let (tmp, cfg) = desktop();
    let commands: Rc<RefCell<Vec<String>>> = Rc::default();
    let mut ws = workspace(tmp.path(), &Memory::default(), &Timer::new(true, true), &commands);
    inspect(&mut ws, &cfg, 200, true).unwrap();
    let tools: Vec<String> = commands
        .borrow()
        .iter()
        .filter_map(|c| {
            c.split("command -v ")
                .nth(1)
                .map(|rest| rest.split_whitespace().next().unwrap_or_default().to_owned())
        })
        .collect();
    assert_eq!(tools, ["python3", "file", "git", "systemctl"]);
    assert!(
        commands
            .borrow()
            .iter()
            .any(|c| c.contains("import curses, rich, sqlite3"))
    );
    assert!(
        commands
            .borrow()
            .iter()
            .any(|c| c.contains("systemctl --user show-environment"))
    );
    let mut ws = workspace(tmp.path(), &Memory::default(), &Timer::new(true, true), &commands);
    ws.answer = Box::new(|command, _| {
        Ok(Output {
            code: i32::from(command.contains("import curses, rich, sqlite3")),
            ..Output::default()
        })
    });
    let error = inspect(&mut ws, &cfg, 200, true).unwrap_err();
    assert!(error.0.contains("curses, rich and sqlite3"), "{}", error.0);
}

#[test]
fn apply_installs_enables_verifies_and_is_idempotent() {
    let (tmp, cfg) = desktop();
    let commands: Rc<RefCell<Vec<String>>> = Rc::default();
    let memory = Memory::default();
    let mut ws = workspace(tmp.path(), &memory, &Timer::new(false, false), &commands);
    let state = inspect(&mut ws, &cfg, 200, true).unwrap();
    let (status, detail) = apply(&mut ws, &cfg, 200, &state, &mut |_| {}).unwrap();
    let current = inspect(&mut ws, &cfg, 200, true).unwrap();
    let installs = memory.installs();
    let count = commands.borrow().len();
    let (repeat, _) = apply(&mut ws, &cfg, 200, &current, &mut |_| {}).unwrap();
    assert_eq!(status, "succeeded");
    assert!(detail.contains("installed"));
    assert_eq!(repeat, "already-ready");
    assert_eq!(memory.installs(), installs);
    assert_eq!(commands.borrow().len(), count);
    assert!(
        commands
            .borrow()
            .iter()
            .any(|c| c.contains("systemctl --user daemon-reload"))
    );
    assert!(
        commands
            .borrow()
            .iter()
            .any(|c| c.contains("systemctl --user enable --now backup.timer"))
    );
}

#[test]
fn update_sends_only_the_changed_managed_asset() {
    let (tmp, cfg) = desktop();
    let service = ".config/systemd/user/backup.service";
    let mut files = desired_files();
    files.insert(service.into(), (b"old service".to_vec(), 0o644));
    let memory = Memory::with(files);
    let commands = Rc::default();
    let mut ws = workspace(tmp.path(), &memory, &Timer::new(true, true), &commands);
    let state = inspect(&mut ws, &cfg, 200, true).unwrap();
    apply(&mut ws, &cfg, 200, &state, &mut |_| {}).unwrap();
    let install = memory
        .calls
        .borrow()
        .iter()
        .find(|(op, _)| op == "managed-files-install")
        .unwrap()
        .1
        .clone();
    let sent: Vec<&str> = install["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(sent, [service]);
    assert_eq!(*memory.files.borrow(), desired_files());
}

#[test]
fn generic_workspace_inspection_exposes_backup_state() {
    let (tmp, cfg) = desktop();
    let mut ws = workspace(
        tmp.path(),
        &Memory::with(desired_files()),
        &Timer::new(true, true),
        &Rc::default(),
    );
    let state = inspect_workspace_state(&mut ws, &cfg, &target(), &[bk()]).unwrap();
    assert_eq!(state["items"]["bk"]["state"], json!("configured"), "{state}");
    assert_eq!(state["items"]["bk"]["ready"], json!(true));
}

#[test]
fn execute_uses_one_workspace_snapshots_only_changed_assets_and_records_paths() {
    let (tmp, cfg) = desktop();
    let mut files = desired_files();
    files.insert(".local/bin/bk".into(), (b"old BK".to_vec(), 0o755));
    let memory = Memory::with(files);
    let snapshots: Rc<RefCell<Vec<Value>>> = Rc::default();
    let seen = Rc::clone(&snapshots);
    let mut answer = memory.answer();
    let mut ws = TestWorkspace::scripted(tmp.path(), move |operation, values| {
        if operation == "snapshot" || operation == "state-record" {
            seen.borrow_mut()
                .push(json!({"operation": operation, "values": values}));
            return Ok(
                json!({"ok": true, "created": true, "id": "snapshot", "path": "~/.local/state/homestack/snapshot"}),
            );
        }
        answer(operation, values)
    });
    ws.answer = Box::new(Timer::new(true, true).answer(Rc::default()));
    let plan = build_plan(&cfg, &target(), &[bk()], None, false, true).unwrap();
    let mut connect = || -> Result<Box<dyn hs_cli::setup::workspace::Workspace>, AppError> {
        panic!("must not open another session")
    };
    let _ = &mut connect;
    let result = execute_plan(&cfg, &FakeSystem::new(), &plan, Connection::Open(&mut ws), &mut Quiet);
    assert_eq!(result["ok"], json!(true), "{result}");
    let calls = snapshots.borrow().clone();
    let snapshot = calls.iter().find(|c| c["operation"] == json!("snapshot")).unwrap();
    assert_eq!(snapshot["values"]["paths"], json!([".local/bin/bk"]));
    let record = calls.iter().find(|c| c["operation"] == json!("state-record")).unwrap();
    assert_eq!(record["values"]["handler"], json!("backup"));
    assert_eq!(record["values"]["paths"], json!(managed_state_paths()));
    assert!(!json!(calls).to_string().contains("backup/backup.yaml"));
}

#[test]
fn invalid_backup_directory_blocks_the_whole_plan_before_mutation() {
    let (tmp, cfg) = desktop();
    let operations: Rc<RefCell<Vec<String>>> = Rc::default();
    let seen = Rc::clone(&operations);
    let mut ws = TestWorkspace::scripted(tmp.path(), move |operation, _| {
        seen.borrow_mut().push(operation.to_owned());
        if operation == "paths" {
            return Err(AppError::new("File type conflict at ~/backup"));
        }
        Ok(json!({"ok": true}))
    });
    ws.answer = Box::new(Timer::new(false, false).answer(Rc::default()));
    let plan = build_plan(&cfg, &target(), &[bk()], None, false, true).unwrap();
    let result = execute_plan(&cfg, &FakeSystem::new(), &plan, Connection::Open(&mut ws), &mut Quiet);
    assert_eq!(result["ok"], json!(false));
    assert_eq!(result["results"][0]["status"], json!("blocked"));
    assert!(
        result["results"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("File type conflict")
    );
    for operation in ["snapshot", "managed-files-install", "state-record"] {
        assert!(!operations.borrow().iter().any(|o| o == operation), "{operation}");
    }
}

/// Python's `BackupRetrievalIntegrationTests`: a desktop and a guest home,
/// the real guest program, a fake timer.
struct Retrieval {
    _tmp: tempfile::TempDir,
    desktop: PathBuf,
    guest_home: PathBuf,
    cfg: Settings,
    timer: Timer,
    calls: Rc<RefCell<Vec<(String, Value)>>>,
}

impl Retrieval {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let desktop = tmp.path().join("desktop");
        let guest_home = tmp.path().join("guest");
        std::fs::create_dir(&desktop).unwrap();
        std::fs::create_dir(&guest_home).unwrap();
        let cfg = settings(&desktop);
        Self {
            _tmp: tmp,
            desktop,
            guest_home,
            cfg,
            timer: Timer::new(false, false),
            calls: Rc::default(),
        }
    }

    fn workspace(&self) -> TestWorkspace {
        let mut ws = TestWorkspace::new(&self.guest_home);
        let calls = Rc::clone(&self.calls);
        ws.guest = Box::new(move |operation, payload| {
            calls.borrow_mut().push((operation.to_owned(), payload.clone()));
            None
        });
        ws.answer = Box::new(self.timer.answer(Rc::default()));
        ws
    }

    fn apply(&self) -> Result<Value, AppError> {
        let mut ws = self.workspace();
        let state = inspect(&mut ws, &self.cfg, 200, true)?;
        apply(&mut ws, &self.cfg, 200, &state, &mut |_| {})?;
        Ok(state)
    }

    fn keys(&self) -> Vec<(String, Vec<u8>, i64)> {
        let mut keys: Vec<(String, Vec<u8>, i64)> = std::fs::read_dir(self.desktop.join(".ssh/homestack/backup"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().is_file())
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    std::fs::read(e.path()).unwrap(),
                    e.metadata().unwrap().mtime_nsec(),
                )
            })
            .collect();
        keys.sort();
        keys
    }
}

#[test]
fn fresh_install_and_repeated_setup_preserve_keys_and_need_no_backup() {
    let r = Retrieval::new();
    r.apply().unwrap();
    let keys = r.keys();
    assert_eq!(keys.len(), 4);
    let state = inspect(&mut r.workspace(), &r.cfg, 200, true).unwrap();
    assert_eq!(state["state"], json!("configured"), "{state}");
    for relative in ["backup/backup.yaml", "backup/backup.tgz", "backup/status.json"] {
        assert!(!r.guest_home.join(relative).exists());
    }
    let authorized = r.guest_home.join(".ssh/authorized_keys");
    let original = (
        std::fs::read(&authorized).unwrap(),
        authorized.metadata().unwrap().mtime_nsec(),
    );
    r.apply().unwrap();
    assert_eq!(r.keys(), keys);
    assert_eq!(
        (
            std::fs::read(&authorized).unwrap(),
            authorized.metadata().unwrap().mtime_nsec()
        ),
        original
    );
    for (operation, payload) in r.calls.borrow().iter() {
        if operation == "backup-authorized-keys" {
            assert!(!payload.to_string().contains("PRIVATE KEY"));
        }
    }
}

#[test]
fn missing_guest_entry_repairs_without_rotating_keys() {
    let r = Retrieval::new();
    r.apply().unwrap();
    let keys = r.keys();
    let authorized = r.guest_home.join(".ssh/authorized_keys");
    let canonical = std::fs::read_to_string(&authorized).unwrap();
    let first = canonical.split_inclusive('\n').next().unwrap();
    std::fs::write(&authorized, format!("# Keep this comment\n{first}")).unwrap();
    let state = inspect(&mut r.workspace(), &r.cfg, 200, true).unwrap();
    assert_eq!(state["state"], json!("needs update"));
    assert!(
        state["snapshot_paths"]
            .as_array()
            .unwrap()
            .contains(&json!(".ssh/authorized_keys"))
    );
    apply(&mut r.workspace(), &r.cfg, 200, &state, &mut |_| {}).unwrap();
    assert_eq!(r.keys(), keys);
    assert_eq!(
        std::fs::read_to_string(&authorized).unwrap(),
        format!("# Keep this comment\n{canonical}")
    );
}

#[test]
fn asset_update_preserves_credentials_and_runtime_data() {
    let r = Retrieval::new();
    r.apply().unwrap();
    let keys = r.keys();
    let authorized = r.guest_home.join(".ssh/authorized_keys");
    let original = (
        std::fs::read(&authorized).unwrap(),
        authorized.metadata().unwrap().mtime_nsec(),
    );
    let runtime = [
        ("backup.yaml", "user configuration"),
        ("backup.tgz", "archive"),
        ("status.json", "status"),
        ("archive/keep", "history"),
        (".work/keep", "work"),
        ("backup.log", "log"),
        (".bk.lock", "lock"),
    ];
    for (relative, content) in runtime {
        let path = r.guest_home.join("backup").join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    std::fs::write(r.guest_home.join(".local/bin/bk"), "outdated asset").unwrap();
    r.apply().unwrap();
    assert_eq!(r.keys(), keys);
    assert_eq!(
        (
            std::fs::read(&authorized).unwrap(),
            authorized.metadata().unwrap().mtime_nsec()
        ),
        original
    );
    for (relative, content) in runtime {
        assert_eq!(
            std::fs::read_to_string(r.guest_home.join("backup").join(relative)).unwrap(),
            content
        );
    }
}

#[test]
fn inspection_does_not_generate_desktop_keys() {
    let r = Retrieval::new();
    let state = inspect(&mut r.workspace(), &r.cfg, 200, true).unwrap();
    assert_eq!(state["ready"], json!(false));
    assert!(!r.desktop.join(".ssh").exists());
    assert!(!r.guest_home.join(".ssh").exists());
}

#[test]
fn mismatched_pair_blocks_before_guest_mutation() {
    let r = Retrieval::new();
    reconcile_retrieval_keys(&r.desktop, 200).unwrap();
    let directory = r.desktop.join(".ssh/homestack/backup");
    std::fs::copy(
        directory.join("vm200-bk-archive.pub"),
        directory.join("vm200-bk-status.pub"),
    )
    .unwrap();
    let before = r.keys();
    assert!(r.apply().is_err());
    assert_eq!(r.keys(), before);
    assert!(!r.guest_home.join(".local/bin/bk").exists());
    assert!(!r.guest_home.join(".ssh/authorized_keys").exists());
    assert!(
        !r.calls
            .borrow()
            .iter()
            .any(|(op, payload)| op == "managed-files-install"
                || (op == "backup-authorized-keys" && payload["apply"] == json!(true)))
    );
}

#[test]
fn reconciled_keys_are_private_and_canonical() {
    let (tmp, _) = desktop();
    let directory = tmp.path().join(".ssh/homestack/backup");
    assert_eq!(directory.metadata().unwrap().permissions().mode() & 0o777, 0o700);
    for role in ["archive", "status"] {
        let private = directory.join(format!("vm200-bk-{role}"));
        let public = directory.join(format!("vm200-bk-{role}.pub"));
        assert_eq!(private.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(public.metadata().unwrap().permissions().mode() & 0o777, 0o644);
        let line = std::fs::read_to_string(&public).unwrap();
        assert!(
            line.starts_with("ssh-ed25519 ") && line.trim_end().ends_with(&format!("homestack-bk-{role}-vm200")),
            "{line}"
        );
    }
}
