//! Inspection and execution of a setup, and its SSH connection (Python
//! `test_setup.py::EngineTests`, `SSHTests`, `AdditionalSafetyTests`,
//! `test_setup_state.py::HostStateTests`, `test_alien_workspace.py`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod setup_fixture;

use std::cell::Cell;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;

use common::FakeSystem;
use hs_cli::AppError;
use hs_cli::master::Master;
use hs_cli::setup::cli::check_role;
use hs_cli::setup::definitions::{Application, CODEX_RECIPE, Entry, Params};
use hs_cli::setup::engine::{Connection, Observer, execute_plan, inspect_workspace_state};
use hs_cli::setup::guest::require_tool;
use hs_cli::setup::plan::{Plan, all_bins, build_plan};
use hs_cli::setup::workspace::{SshWorkspace, Workspace};
use hs_cli::system::Output;
use serde_json::{Value, json};
use setup_fixture::{TestWorkspace, entry, run_guest, settings, target};

#[derive(Default)]
struct Events {
    activity: Vec<String>,
    progress: Vec<(String, String)>,
    terminal: Vec<&'static str>,
}

impl Observer for Events {
    fn progress(&mut self, id: &str, state: &str) {
        self.progress.push((id.to_owned(), state.to_owned()));
    }

    fn activity(&mut self, _id: Option<&str>, message: &str) {
        self.activity.push(message.to_owned());
    }

    fn terminal(&mut self, operation: &mut dyn FnMut()) {
        self.terminal.push("suspend");
        operation();
        self.terminal.push("restore");
    }
}

fn plan(home: &Path, entries: &[Entry], unattended: bool, include_configs: bool) -> Plan {
    build_plan(&settings(home), &target(), entries, None, unattended, include_configs).unwrap()
}

fn run(home: &Path, plan: &Plan, workspace: &mut dyn Workspace, events: &mut Events) -> Value {
    execute_plan(
        &settings(home),
        &FakeSystem::new(),
        plan,
        Connection::Open(workspace),
        events,
    )
}

fn statuses(result: &Value) -> Vec<String> {
    result["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["status"].as_str().unwrap().to_owned())
        .collect()
}

fn application(command: &str, interactive: bool) -> Entry {
    let mut custom = entry("codex");
    let mut params = Application::new(command);
    if !interactive {
        params.interaction = "non-interactive".into();
    }
    custom.params = Params::Application(Box::new(params));
    custom
}

/// Every guest call answers `ok`, environment with `changed`.
fn ok_guest(changed: Value) -> impl FnMut(&str, &Value) -> Result<Value, AppError> {
    move |operation, _| {
        Ok(match operation {
            "environment" => json!({"ok": true, "changed": changed, "paths": [".bashrc", ".profile"]}),
            "metadata" => json!({"ok": true, "items": []}),
            "snapshot" => {
                json!({"ok": true, "created": true, "id": "snapshot-id", "path": "~/.local/state/homestack/snapshot-id"})
            }
            "state-read" => json!({"ok": true, "registry": {}}),
            _ => json!({"ok": true}),
        })
    }
}

#[test]
fn execution_activity_reports_preflight_snapshot_shell_write_and_state_record() {
    let home = tempfile::tempdir().unwrap();
    let plan = plan(home.path(), &[entry("bash")], false, true);
    let mut workspace = TestWorkspace::scripted(home.path(), ok_guest(json!([".profile"])));
    let mut events = Events::default();
    let result = run(home.path(), &plan, &mut workspace, &mut events);
    assert_eq!(result["ok"], json!(true), "{result}");
    for expected in [
        "Verify workspace connection",
        "Preflight Bash",
        "Create operation snapshot",
        "Operation snapshot created: ~/.local/state/homestack/snapshot-id",
        "Write Bash shell configuration",
        "Record setup state",
        "Setup execution complete",
    ] {
        assert!(
            events.activity.iter().any(|m| m == expected),
            "{expected}: {:?}",
            events.activity
        );
    }
}

#[test]
fn all_preflight_before_mutation_one_connection_and_stop_on_failure() {
    let home = tempfile::tempdir().unwrap();
    let plan = plan(
        home.path(),
        &[entry("bash"), entry("codex"), entry("opencode")],
        false,
        false,
    );
    let opened = Cell::new(0);
    let log: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let mut connect = || -> Result<Box<dyn Workspace>, AppError> {
        opened.set(opened.get() + 1);
        let guests = Arc::clone(&log);
        let mut workspace = TestWorkspace::scripted(home.path(), move |operation, payload| {
            let apply = payload["apply"] == json!(true);
            guests
                .lock()
                .unwrap()
                .push(format!("{operation}{}", if apply { ":apply" } else { "" }));
            ok_guest(json!([".bashrc"]))(operation, payload)
        });
        let commands = Arc::clone(&log);
        workspace.answer = Box::new(move |command, _| {
            let install = command.contains(CODEX_RECIPE) || command.contains("opencode.ai/install");
            commands
                .lock()
                .unwrap()
                .push(if install { "install".into() } else { "check".into() });
            Ok(Output {
                code: i32::from(command.contains(CODEX_RECIPE)),
                ..Output::default()
            })
        });
        Ok(Box::new(workspace))
    };
    let mut events = Events::default();
    let result = execute_plan(
        &settings(home.path()),
        &FakeSystem::new(),
        &plan,
        Connection::Connect(&mut connect),
        &mut events,
    );
    assert_eq!(statuses(&result), ["succeeded", "failed", "not-run"]);
    assert_eq!(opened.get(), 1);
    let log = log.lock().unwrap().clone();
    let first_write = log
        .iter()
        .position(|e| e == "environment:apply" || e == "install")
        .unwrap();
    let last_check = log.iter().rposition(|e| e == "paths").unwrap();
    assert!(last_check < first_write, "{log:?}");
}

#[test]
fn blocked_preflight_prevents_all_selected_writes() {
    let home = tempfile::tempdir().unwrap();
    let plan = plan(home.path(), &[entry("bash"), entry("codex")], false, false);
    let mut workspace = TestWorkspace::scripted(home.path(), ok_guest(json!([".bashrc"])));
    // Bash's preflight finds no bash; Codex's finds what it needs.
    let mut asked = false;
    workspace.answer = Box::new(move |command, _| {
        let missing = command.contains("command -v bash") && !asked;
        asked |= missing;
        Ok(Output {
            code: i32::from(missing),
            ..Output::default()
        })
    });
    let result = run(home.path(), &plan, &mut workspace, &mut Events::default());
    assert_eq!(statuses(&result), ["blocked", "not-run"]);
    assert!(!workspace.guests.iter().any(|g| g["apply"] == json!(true)));
    assert!(!workspace.commands.iter().any(|(c, _)| c.contains(CODEX_RECIPE)));
}

#[test]
fn failed_download_pipeline_real_shell_and_no_output_leak() {
    let home = tempfile::tempdir().unwrap();
    let custom = application("curl -fsSL file:///a-file-that-does-not-exist | sh", false);
    let plan = plan(home.path(), &[custom], true, true);
    let local = home.path().display().to_string();
    let mut workspace = TestWorkspace::scripted(home.path(), ok_guest(json!([])));
    // HOME and the working directory are this test's own, never production.
    workspace.answer = Box::new(move |command, _| {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(command.replace("/home/user", &local))
            .output()
            .unwrap();
        Ok(Output {
            code: output.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: "SECRET_OUTPUT".into(),
        })
    });
    let result = run(home.path(), &plan, &mut workspace, &mut Events::default());
    let detail = result["results"][0]["detail"].as_str().unwrap();
    assert!(detail.contains("Installer failed"), "{result}");
    assert!(!result.to_string().contains("SECRET"));
}

#[test]
fn installed_application_selection_runs_update_and_failed_postcheck_stops() {
    let home = tempfile::tempdir().unwrap();
    let codex = entry("codex");
    let plan = plan(home.path(), std::slice::from_ref(&codex), false, false);
    let mut workspace = TestWorkspace::scripted(home.path(), ok_guest(json!([])));
    let result = run(home.path(), &plan, &mut workspace, &mut Events::default());
    assert_eq!(statuses(&result), ["succeeded"]);
    assert!(result["results"][0]["detail"].as_str().unwrap().contains("updated"));
    let installs = workspace
        .commands
        .iter()
        .filter(|(c, _)| c.contains(CODEX_RECIPE))
        .count();
    assert_eq!(installs, 1);

    let checks = Cell::new(0);
    let mut workspace = TestWorkspace::scripted(home.path(), ok_guest(json!([])));
    let counter = std::rc::Rc::new(checks);
    let seen = std::rc::Rc::clone(&counter);
    workspace.answer = Box::new(move |command, _| {
        let check = command.contains("codex --version");
        if check {
            seen.set(seen.get() + 1);
        }
        // Not installed at preflight; the check after the installer fails.
        Ok(Output {
            code: i32::from(check),
            ..Output::default()
        })
    });
    let result = run(home.path(), &plan, &mut workspace, &mut Events::default());
    assert_eq!(statuses(&result), ["failed"]);
    assert!(
        result["results"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("verification failed")
    );
    assert_eq!(counter.get(), 2);
}

#[test]
fn interactive_pty_handoff_restores_on_failure() {
    let home = tempfile::tempdir().unwrap();
    let custom = application("read answer", true);
    let plan = plan(home.path(), &[custom], false, true);
    let mut workspace = TestWorkspace::scripted(home.path(), ok_guest(json!([])));
    workspace.answer = Box::new(|_, interactive| {
        Ok(Output {
            code: if interactive { 4 } else { 0 },
            ..Output::default()
        })
    });
    let mut events = Events::default();
    let result = run(home.path(), &plan, &mut workspace, &mut events);
    assert_eq!(statuses(&result), ["failed"]);
    assert_eq!(events.terminal, ["suspend", "restore"]);
    assert!(
        workspace
            .commands
            .iter()
            .any(|(c, interactive)| *interactive && c.contains("read answer"))
    );
}

#[test]
fn transport_failure_is_not_missing_tool() {
    let home = tempfile::tempdir().unwrap();
    let mut workspace = TestWorkspace::new(home.path());
    workspace.answer = Box::new(|_, _| {
        Ok(Output {
            code: 255,
            ..Output::default()
        })
    });
    let error = require_tool(&mut workspace, &settings(home.path()), "rsync", &[]).unwrap_err();
    assert!(error.0.contains("transport"), "{}", error.0);
}

#[test]
fn matching_environment_reports_no_files_changed() {
    let home = tempfile::tempdir().unwrap();
    let plan = plan(home.path(), &[entry("bash")], false, true);
    let mut workspace = TestWorkspace::scripted(home.path(), ok_guest(json!([])));
    let result = run(home.path(), &plan, &mut workspace, &mut Events::default());
    assert_eq!(result["results"][0]["status"], json!("already-ready"));
    assert_eq!(
        result["results"][0]["detail"],
        json!("Shell configuration already matches; no files changed")
    );
}

#[test]
fn inspection_uses_live_check_and_registry_metadata_without_storing_version() {
    let home = tempfile::tempdir().unwrap();
    let mut workspace = TestWorkspace::scripted(home.path(), |operation, _| {
        Ok(match operation {
            "state-read" => json!({"ok": true, "state_path": "~/.local/state/homestack/setup.json",
                "registry": {"items": {"codex": {"first_managed_at": "2026-09-11T00:00:00Z", "last_applied_at": "2026-09-11T00:01:00Z"}}}}),
            "metadata" => json!({"ok": true, "items": []}),
            _ => json!({"ok": true}),
        })
    });
    let result = inspect_workspace_state(&mut workspace, &settings(home.path()), &target(), &[entry("codex")]).unwrap();
    let item = &result["items"]["codex"];
    assert_eq!(item["ready"], json!(true));
    assert_eq!(item["state"], json!("installed"));
    assert_eq!(item["last_applied_at"], json!("2026-09-11T00:01:00Z"));
    assert!(item.get("version").is_none());
}

fn apply_bash(home: &Path) -> Value {
    let cfg = settings(home);
    let applied = run_guest(&json!({"operation": "environment", "home": home.display().to_string(),
                                    "profile": "bash", "bins": all_bins(&cfg), "apply": true}));
    run_guest(
        &json!({"operation": "state-record", "home": home.display().to_string(), "vmid": 200,
                      "name": "workspace", "id": "bash", "handler": "environment",
                      "paths": applied["paths"], "snapshot": null}),
    );
    applied
}

#[test]
fn environment_inspection_reports_file_drift_requiring_overwrite() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    assert!(!apply_bash(home)["changed"].as_array().unwrap().is_empty());
    let inspect = || {
        let mut workspace = TestWorkspace::new(home);
        inspect_workspace_state(&mut workspace, &settings(home), &target(), &[entry("bash")]).unwrap()["items"]["bash"]
            .clone()
    };
    let unchanged = inspect();
    assert_eq!(unchanged["ready"], json!(true));
    assert_eq!(unchanged["will_overwrite"], json!(false));
    assert_eq!(unchanged["changed_since_apply"], json!([]));
    assert_eq!(unchanged["differs"], json!([]));
    let mut profile = std::fs::read_to_string(home.join(".profile")).unwrap();
    profile.push_str("#\n");
    std::fs::write(home.join(".profile"), profile).unwrap();
    let modified = inspect();
    assert_eq!(modified["ready"], json!(false));
    assert_eq!(modified["will_overwrite"], json!(true));
    assert_eq!(modified["changed_since_apply"], json!([".profile"]));
    assert_eq!(modified["state"], json!("needs update"));
    // Which files differ from what the setup writes (marked in the TUI).
    assert_eq!(modified["differs"], json!([".profile"]));
    std::fs::remove_file(home.join(".bashrc")).unwrap();
    let missing = inspect();
    let changed = missing["changed_since_apply"].as_array().unwrap();
    assert!(changed.contains(&json!(".bashrc")) && changed.contains(&json!(".profile")));
}

#[test]
fn execute_plan_snapshots_external_shell_drift_before_recording_baseline() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    apply_bash(home);
    let root = home.join(".local/state/homestack");
    let registry = |path: &Path| -> Value { serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap() };
    let profile_of = |registry: &Value| {
        registry["items"]["bash"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == json!(".profile"))
            .unwrap()
            .clone()
    };
    let baseline = profile_of(&registry(&root.join("setup.json")));
    let original = std::fs::read(home.join(".profile")).unwrap();
    let original_bashrc = std::fs::read(home.join(".bashrc")).unwrap();
    let bashrc_inode = home.join(".bashrc").metadata().unwrap().ino();
    let profile_inode = home.join(".profile").metadata().unwrap().ino();
    let mut edited = original.clone();
    edited.extend_from_slice(b"# external edit\n");
    std::fs::write(home.join(".profile"), &edited).unwrap();
    let plan = plan(home, &[entry("bash")], false, true);
    let mut workspace = TestWorkspace::new(home);
    let result = run(home, &plan, &mut workspace, &mut Events::default());
    assert_eq!(result["ok"], json!(true), "{result}");
    assert_eq!(result["results"][0]["status"], json!("succeeded"));
    assert_eq!(result["results"][0]["detail"], json!("Shell configuration verified"));
    assert_eq!(result["snapshot"]["created"], json!(true));
    let snapshot = root.join(result["snapshot"]["id"].as_str().unwrap());
    let manifest = registry(&snapshot.join("snapshot.json"));
    let paths: Vec<&str> = manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, [".profile"]);
    assert_eq!(std::fs::read(snapshot.join("home/.profile")).unwrap(), edited);
    assert_ne!(home.join(".profile").metadata().unwrap().ino(), profile_inode);
    assert_eq!(std::fs::read(home.join(".profile")).unwrap(), original);
    assert_eq!(home.join(".bashrc").metadata().unwrap().ino(), bashrc_inode);
    assert_eq!(std::fs::read(home.join(".bashrc")).unwrap(), original_bashrc);
    assert_eq!(
        profile_of(&registry(&snapshot.join("setup.before.json")))["sha256"],
        baseline["sha256"]
    );
    let snapshots = |root: &Path| {
        let mut names: Vec<String> = std::fs::read_dir(root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let before = snapshots(&root);
    let repeated = run(home, &plan, &mut TestWorkspace::new(home), &mut Events::default());
    assert_eq!(repeated["ok"], json!(true));
    assert_eq!(repeated["results"][0]["status"], json!("already-ready"));
    assert_eq!(repeated["snapshot"]["created"], json!(false));
    assert_eq!(snapshots(&root), before);
}

#[test]
fn execute_plan_snapshot_failure_leaves_shell_and_registry_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    apply_bash(home);
    let mut profile = std::fs::read(home.join(".profile")).unwrap();
    profile.extend_from_slice(b"# external edit\n");
    std::fs::write(home.join(".profile"), &profile).unwrap();
    let inode = home.join(".profile").metadata().unwrap().ino();
    let registry_path = home.join(".local/state/homestack/setup.json");
    let registry = std::fs::read(&registry_path).unwrap();
    let mut workspace = TestWorkspace::new(home);
    workspace.guest = Box::new(|operation, _| (operation == "snapshot").then(|| Err(AppError::new("snapshot failed"))));
    let plan = plan(home, &[entry("bash")], false, true);
    let result = run(home, &plan, &mut workspace, &mut Events::default());
    assert_eq!(result["ok"], json!(false));
    assert_eq!(result["results"][0]["status"], json!("blocked"));
    assert_eq!(result["snapshot"]["created"], json!(false));
    assert_eq!(std::fs::read(home.join(".profile")).unwrap(), profile);
    assert_eq!(home.join(".profile").metadata().unwrap().ino(), inode);
    assert_eq!(std::fs::read(&registry_path).unwrap(), registry);
}

#[test]
fn execute_plan_can_reuse_caller_owned_workspace() {
    let home = tempfile::tempdir().unwrap();
    let mut codex = entry("codex");
    if let Params::Application(p) = &mut codex.params {
        p.config_files.clear();
        p.validation = None;
    }
    let plan = plan(home.path(), &[codex], false, true);
    let mut workspace = TestWorkspace::scripted(home.path(), ok_guest(json!([])));
    let result = run(home.path(), &plan, &mut workspace, &mut Events::default());
    assert_eq!(result["ok"], json!(true), "{result}");
    assert!(workspace.operations().contains(&"state-record".to_owned()));
}

#[test]
fn snapshot_failure_blocks_writes_before_any_selected_action() {
    let home = tempfile::tempdir().unwrap();
    let mut custom = entry("codex");
    if let Params::Application(p) = &mut custom.params {
        p.backup_paths = vec!["~/.config/codex/settings.json".into()];
    }
    let plan = plan(home.path(), &[custom], false, true);
    let mut workspace = TestWorkspace::scripted(home.path(), |operation, payload| {
        if operation == "snapshot" {
            return Err(AppError::new("snapshot failed"));
        }
        ok_guest(json!([]))(operation, payload)
    });
    let result = run(home.path(), &plan, &mut workspace, &mut Events::default());
    assert_eq!(result["ok"], json!(false));
    assert_eq!(result["results"][0]["status"], json!("blocked"));
    assert!(
        result["results"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("snapshot failed")
    );
    assert!(!workspace.commands.iter().any(|(c, _)| c.contains(CODEX_RECIPE)));
    assert!(!workspace.operations().contains(&"state-record".to_owned()));
}

#[test]
fn unattended_guard_is_enforced_by_execution_service() {
    let home = tempfile::tempdir().unwrap();
    let custom = application("read answer", true);
    let plan = Plan {
        target: target(),
        entries: vec![custom],
        catalog_id: None,
        unattended: true,
    };
    let mut connect = || -> Result<Box<dyn Workspace>, AppError> { panic!("Unexpected authentication") };
    let result = execute_plan(
        &settings(home.path()),
        &FakeSystem::new(),
        &plan,
        Connection::Connect(&mut connect),
        &mut Events::default(),
    );
    assert_eq!(result["ok"], json!(false));
    assert_eq!(result["results"][0]["status"], json!("blocked"));
}

#[test]
fn same_connection_serves_actual_environment_file_and_application_handlers() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::write(home.join("fixture"), "Synthetic fixture").unwrap();
    let file = Entry {
        id: "fixture".into(),
        group: "files".into(),
        handler: "file".into(),
        label: "Fixture".into(),
        description: "Synthetic file".into(),
        params: Params::File {
            path: "~/fixture".into(),
            application: None,
        },
        depends_on: Vec::new(),
    };
    let plan = plan(
        home,
        &[file, application("printf done", false), entry("bash")],
        false,
        true,
    );
    let mut workspace = TestWorkspace::scripted(home, ok_guest(json!([".bashrc"])));
    let result = run(home, &plan, &mut workspace, &mut Events::default());
    assert_eq!(result["ok"], json!(true), "{result}");
    assert_eq!(
        workspace.transfers,
        [(
            home.join("fixture").display().to_string(),
            "/home/user/fixture".to_owned()
        )]
    );
    assert!(
        workspace
            .guests
            .iter()
            .any(|g| g["operation"] == json!("environment") && g["apply"] == json!(true))
    );
    assert!(workspace.commands.iter().any(|(c, _)| c.contains("printf done")));
}

#[test]
fn inspection_and_execution_send_alien_identity_to_guest() {
    let home = tempfile::tempdir().unwrap();
    let mut alien = target();
    alien.alien = true;
    let mut workspace = TestWorkspace::scripted(home.path(), ok_guest(json!([])));
    inspect_workspace_state(&mut workspace, &settings(home.path()), &alien, &[]).unwrap();
    let identity = workspace
        .guests
        .iter()
        .find(|g| g["operation"] == json!("identity"))
        .unwrap();
    assert_eq!(identity["alien"], json!(true));
    let plan = build_plan(&settings(home.path()), &alien, &[entry("bash")], None, false, true).unwrap();
    let mut workspace = TestWorkspace::scripted(home.path(), ok_guest(json!([])));
    let result = run(home.path(), &plan, &mut workspace, &mut Events::default());
    assert_eq!(result["ok"], json!(true));
    let identity = workspace
        .guests
        .iter()
        .find(|g| g["operation"] == json!("identity"))
        .unwrap();
    assert_eq!(identity["alien"], json!(true));
}

#[test]
fn repository_and_setup_targets_preserve_alien_role() {
    assert!(check_role(200, &["homestack-alien"]).unwrap());
    assert!(!check_role(200, &["homestack-ws"]).unwrap());
}

#[test]
fn setup_accepts_standalone_alien_but_native_lifecycle_does_not() {
    check_role(200, &["homestack-alien"]).unwrap();
    check_role(200, &["homestack-ws"]).unwrap();
    let error = check_role(200, &["homestack-ws", "homestack-alien"]).unwrap_err();
    assert!(error.0.contains("conflicting"), "{}", error.0);
    assert!(check_role(200, &["homestack-gold"]).is_err());
    assert!(check_role(200, &[]).is_err());
}

/// A master over a fake desktop: every command answers `code`.
fn ssh(system: &Arc<FakeSystem>, options: &[&str]) -> SshWorkspace {
    let mut master = Master::new("user@192.0.2.200");
    master.options = options.iter().map(|o| (*o).to_owned()).collect();
    let system: Arc<dyn hs_cli::system::System> = Arc::clone(system) as Arc<dyn hs_cli::system::System>;
    SshWorkspace::open(system, Path::new("/nonexistent"), master).unwrap()
}

#[test]
fn noninteractive_command_streams_input_without_putting_it_in_argv() {
    let system = Arc::new(FakeSystem::new());
    let mut workspace = ssh(&system, &[]);
    workspace.run("python3 -", Some("PRIVATE_PAYLOAD"), false).unwrap();
    let inputs = system.inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].1, "PRIVATE_PAYLOAD");
    assert!(!inputs[0].0.join(" ").contains("PRIVATE_PAYLOAD"));
}

#[test]
fn one_master_private_socket_options_and_fail_closed_children() {
    let system = Arc::new(FakeSystem::new());
    let options = [
        "-F",
        "/dev/null",
        "-o",
        "IdentityAgent=none",
        "-o",
        "IdentitiesOnly=yes",
        "-i",
        "/tmp/test-key",
    ];
    let mut workspace = ssh(&system, &options);
    let masters = system.masters();
    assert_eq!(masters.len(), 1);
    assert!(masters[0].contains(&"ControlPersist=yes".to_owned()));
    let control = masters[0][masters[0].iter().position(|w| w == "-S").unwrap() + 1].clone();
    let directory = Path::new(&control).parent().unwrap().to_path_buf();
    assert_eq!(directory.metadata().unwrap().permissions().mode() & 0o777, 0o700);
    workspace.run("true", None, false).unwrap();
    workspace.run("read answer", None, true).unwrap();
    workspace.transfer("/tmp/source", "/home/user/file").unwrap();
    drop(workspace);
    assert!(!directory.exists());
    let mut children: Vec<Vec<String>> = system.runs().into_iter().filter(|argv| argv[0] == "ssh").collect();
    children.extend(system.interactive.lock().unwrap().clone());
    assert!(
        children
            .iter()
            .all(|argv| argv.contains(&"ProxyCommand=false".to_owned())
                && argv.contains(&"PubkeyAuthentication=no".to_owned())
                && argv.contains(&"/tmp/test-key".to_owned())
                && argv.contains(&"IdentityAgent=none".to_owned())
                && argv.contains(&"/dev/null".to_owned()))
    );
    let pty = system.interactive.lock().unwrap().clone();
    assert!(pty.iter().any(|argv| argv.contains(&"-tt".to_owned())));
    assert!(system.inputs.lock().unwrap().is_empty());
    let rsync = system.runs().into_iter().find(|argv| argv[0] == "rsync").unwrap();
    let transport = &rsync[rsync.iter().position(|w| w == "-e").unwrap() + 1];
    assert!(transport.contains("ProxyCommand=false"));
    assert!(!rsync.contains(&"--delete".to_owned()));
}

#[test]
fn master_loss_and_cancellation_cleanup() {
    let system = Arc::new(FakeSystem::new());
    let workspace = ssh(&system, &[]);
    drop(workspace);
    assert!(
        system
            .runs()
            .iter()
            .any(|argv| argv.contains(&"-O".to_owned()) && argv.contains(&"exit".to_owned()))
    );
}

#[test]
fn master_disappears_without_running_or_authenticating_child() {
    let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&checks);
    let system = Arc::new(FakeSystem::new().run(move |argv| {
        let check = argv.contains(&"check".to_owned());
        // The first check (opening) answers; later the master is gone.
        if check && seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
            return Ok(Output::failed(255, "gone"));
        }
        Ok(Output::default())
    }));
    let mut workspace = ssh(&system, &[]);
    let error = workspace.run("true", None, false).unwrap_err();
    assert!(error.0.contains("master"), "{}", error.0);
    assert!(!system.runs().iter().any(|argv| argv.last() == Some(&"true".to_owned())));
    assert!(system.inputs.lock().unwrap().is_empty());
}
