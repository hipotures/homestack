//! Herdr: installer, user service, linger and the desktop's machine profile
//! (Python `test_setup_herdr.py`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod setup_fixture;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use common::FakeSystem;
use hs_cli::AppError;
use hs_cli::setup::Settings;
use hs_cli::setup::catalog::load_catalog;
use hs_cli::setup::definitions::{Entry, Params, SetupConfig, defaults, parse_setup};
use hs_cli::setup::engine::{Connection, Observer, execute_plan};
use hs_cli::setup::herdr::{
    INSTALL_COMMAND, UNIT, apply, connect_desktop, desktop_profile, preflight, running_sessions, verify_desktop_alias,
};
use hs_cli::setup::plan::build_plan;
use hs_cli::setup::{HERDR_UNIT_PATH, Target};
use hs_cli::system::Output;
use serde_json::{Value, json};
use setup_fixture::{TestWorkspace, settings};

fn target() -> Target {
    Target {
        name: "gpu".into(),
        ..setup_fixture::target()
    }
}

fn profile() -> Value {
    json!({"id": "profile-id", "target": "user@gpu", "label": "gpu", "session": "default", "enabled": true})
}

fn herdr_entry() -> Entry {
    defaults().into_iter().find(|e| e.id == "herdr").unwrap()
}

/// What the workspace and the desktop look like to Herdr's setup.
#[derive(Debug, Clone)]
struct Runtime {
    installed: bool,
    sessions: Vec<(String, bool)>,
    active: bool,
    enabled: bool,
    linger: bool,
    profile: Option<Value>,
    /// Every command fails.
    broken: bool,
    /// The linger command fails.
    no_linger: bool,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            installed: true,
            sessions: Vec::new(),
            active: false,
            enabled: false,
            linger: false,
            profile: Some(profile()),
            broken: false,
            no_linger: false,
        }
    }
}

type Shared = Rc<RefCell<Runtime>>;
type Desktop = Arc<Mutex<Runtime>>;

/// A workspace answering Herdr's commands from `runtime`; guest calls run
/// the real program in `home`.
fn workspace(home: &Path, runtime: &Shared, commands: &Rc<RefCell<Vec<String>>>) -> TestWorkspace {
    let mut ws = TestWorkspace::new(home);
    let state = Rc::clone(runtime);
    let log = Rc::clone(commands);
    ws.answer = Box::new(move |command, _| {
        log.borrow_mut().push(command.to_owned());
        let mut r = state.borrow_mut();
        if r.broken {
            return Ok(Output {
                code: 1,
                ..Output::default()
            });
        }
        let ok = |code: bool| {
            Ok(Output {
                code: i32::from(!code),
                ..Output::default()
            })
        };
        if command.contains("\"$HOME/.local/bin/herdr\" --version") {
            ok(r.installed)
        } else if command.contains("session list --json") {
            let sessions: Vec<Value> = r
                .sessions
                .iter()
                .map(|(n, running)| json!({"name": n, "running": running}))
                .collect();
            Ok(Output::stdout(&json!({"sessions": sessions}).to_string()))
        } else if command.contains(INSTALL_COMMAND) {
            r.installed = true;
            ok(true)
        } else if command.contains("show-user user") {
            Ok(Output::stdout(if r.linger { "yes\n" } else { "no\n" }))
        } else if command.contains("enable-linger user") {
            if r.no_linger {
                return ok(false);
            }
            r.linger = true;
            ok(true)
        } else if command.contains("is-enabled herdr.service") {
            ok(r.enabled)
        } else if command.contains("is-active herdr.service") {
            Ok(Output {
                code: if r.active { 0 } else { 3 },
                ..Output::default()
            })
        } else if command.contains("enable herdr.service") {
            r.enabled = true;
            ok(true)
        } else if command.contains("start herdr.service") {
            r.active = true;
            match r.sessions.iter_mut().find(|(n, _)| n == "default") {
                Some(session) => session.1 = true,
                None => r.sessions.push(("default".into(), true)),
            }
            ok(true)
        } else {
            ok(true)
        }
    });
    ws
}

/// A desktop whose `herdr machine list` shows the runtime's profile and
/// whose alias of the workspace is right.
fn desktop_system(desktop: &Desktop, calls: &Arc<Mutex<Vec<Vec<String>>>>) -> FakeSystem {
    let state = Arc::clone(desktop);
    let log = Arc::clone(calls);
    FakeSystem::new().run(move |argv| {
        log.lock().unwrap().push(argv.to_vec());
        if argv[..2] == ["herdr", "machine"] && argv[2] == "list" {
            let profiles: Vec<Value> = state.lock().unwrap().profile.iter().cloned().collect();
            return Ok(Output::stdout(&json!(profiles).to_string()));
        }
        if argv[0] == "ssh" && argv[1] == "-G" {
            return Ok(Output::stdout(
                "hostname 192.0.2.200\nuser user\nidentitiesonly yes\nidentityfile ~/.ssh/example-hardware-key\n",
            ));
        }
        Ok(Output::default())
    })
}

/// Records the desktop registration and answers it.
struct Register {
    calls: Vec<Vec<String>>,
    activity: Vec<String>,
    answer: Output,
    desktop: Option<Desktop>,
    seen_active: Option<bool>,
    runtime: Option<Shared>,
}

impl Register {
    fn new(answer: Output) -> Self {
        Self {
            calls: Vec::new(),
            activity: Vec::new(),
            answer,
            desktop: None,
            seen_active: None,
            runtime: None,
        }
    }
}

impl Observer for Register {
    fn activity(&mut self, _id: Option<&str>, message: &str) {
        self.activity.push(message.to_owned());
    }

    fn register_desktop(&mut self, argv: &[String]) -> Result<Output, AppError> {
        self.calls.push(argv.to_vec());
        if let Some(runtime) = &self.runtime {
            self.seen_active = Some(runtime.borrow().active);
        }
        if self.answer.code == 0
            && let Some(desktop) = &self.desktop
        {
            desktop.lock().unwrap().profile = Some(profile());
        }
        Ok(self.answer.clone())
    }
}

struct Case {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    cfg: Settings,
    runtime: Shared,
    desktop: Desktop,
    commands: Rc<RefCell<Vec<String>>>,
    calls: Arc<Mutex<Vec<Vec<String>>>>,
}

impl Case {
    fn new(runtime: Runtime) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let desktop = Arc::new(Mutex::new(runtime.clone()));
        Self {
            cfg: settings(&home),
            home,
            _tmp: tmp,
            runtime: Rc::new(RefCell::new(runtime)),
            desktop,
            commands: Rc::default(),
            calls: Arc::default(),
        }
    }

    fn workspace(&self) -> TestWorkspace {
        workspace(&self.home, &self.runtime, &self.commands)
    }

    fn system(&self) -> FakeSystem {
        desktop_system(&self.desktop, &self.calls)
    }

    fn apply(&self, state: &Value, register: &mut Register) -> Result<(String, String), AppError> {
        apply(
            &mut self.workspace(),
            &self.cfg,
            &self.system(),
            &target(),
            state,
            register,
            "herdr",
            false,
        )
    }

    fn scripts(&self) -> String {
        self.commands.borrow().join("\n")
    }
}

fn changed_state(installed: bool) -> Value {
    json!({"installed": installed, "changed": true, "files": []})
}

#[test]
fn default_and_example_catalog_put_herdr_after_claude() {
    let text = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/fixtures/config.example.toml"),
    )
    .unwrap();
    let example = parse_setup(hs_config::legacy::read(&text).unwrap().client.setup.as_ref().unwrap()).unwrap();
    for setup in [
        SetupConfig::default(),
        parse_setup(&toml::Table::new()).unwrap(),
        example,
    ] {
        let apps: Vec<&str> = setup
            .items
            .iter()
            .filter(|e| e.group == "app")
            .map(|e| e.id.as_str())
            .take(3)
            .collect();
        assert_eq!(apps, ["codex", "claude", "herdr"]);
    }
    let tmp = tempfile::tempdir().unwrap();
    let cfg = settings(tmp.path());
    let row = load_catalog(&cfg, None)
        .rows(&cfg)
        .into_iter()
        .find(|r| r["id"] == json!("herdr"))
        .unwrap();
    assert_eq!(row["index"], json!(3));
    assert_eq!(row["selector"], json!("app=herdr"));
}

#[test]
fn plan_owns_service_path_and_rejects_overlap() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = settings(tmp.path());
    let unit = tmp.path().join(HERDR_UNIT_PATH);
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, "desktop unit").unwrap();
    let conflicting = Entry {
        id: "unit".into(),
        group: "files".into(),
        handler: "file".into(),
        label: "Unit".into(),
        description: String::new(),
        params: Params::File {
            path: format!("~/{HERDR_UNIT_PATH}"),
            application: None,
        },
        depends_on: Vec::new(),
    };
    let error = build_plan(&cfg, &target(), &[herdr_entry(), conflicting], None, false, true).unwrap_err();
    assert!(error.0.contains("Overlapping"), "{}", error.0);
    let plan = build_plan(&cfg, &target(), &[herdr_entry()], None, false, true).unwrap();
    let action = &plan.public()["actions"][0];
    assert_eq!(action["desktop_label"], json!("gpu"));
    assert!(
        action["security_key_touch"]
            .as_str()
            .unwrap()
            .contains("authentication")
    );
}

#[test]
fn new_install_starts_service_before_desktop_registration() {
    let case = Case::new(Runtime {
        installed: false,
        profile: None,
        ..Runtime::default()
    });
    let mut register = Register::new(Output::default());
    register.desktop = Some(Arc::clone(&case.desktop));
    register.runtime = Some(Rc::clone(&case.runtime));
    let (status, detail) = case.apply(&changed_state(false), &mut register).unwrap();
    assert_eq!(status, "succeeded");
    assert!(detail.contains("enabled and active"), "{detail}");
    assert_eq!(register.seen_active, Some(true));
    let scripts = case.scripts();
    assert!(scripts.contains(INSTALL_COMMAND));
    assert!(scripts.contains("XDG_RUNTIME_DIR=/run/user/1000"));
    assert!(scripts.contains("loginctl --no-ask-password enable-linger user"));
    assert!(scripts.find(INSTALL_COMMAND).unwrap() < scripts.find("enable-linger").unwrap());
}

#[test]
fn existing_unmanaged_sessions_never_update_start_or_stop() {
    let case = Case::new(Runtime {
        sessions: vec![("default".into(), true), ("agents".into(), true)],
        ..Runtime::default()
    });
    let (_, detail) = case
        .apply(&changed_state(true), &mut Register::new(Output::default()))
        .unwrap();
    assert!(detail.contains("existing sessions preserved"), "{detail}");
    let scripts = case.scripts();
    for forbidden in [
        INSTALL_COMMAND,
        "herdr update",
        "start herdr.service",
        "restart",
        "server stop",
        "session stop",
    ] {
        assert!(!scripts.contains(forbidden), "{forbidden}");
    }
    assert!(scripts.contains("enable herdr.service"));
}

#[test]
fn active_service_is_not_restarted_or_updated() {
    let case = Case::new(Runtime {
        active: true,
        sessions: vec![("default".into(), true)],
        ..Runtime::default()
    });
    case.apply(&changed_state(true), &mut Register::new(Output::default()))
        .unwrap();
    assert!(
        !case
            .commands
            .borrow()
            .iter()
            .any(|c| c.contains("start herdr.service") || c.contains("herdr update"))
    );
}

#[test]
fn failed_installer_does_not_configure_service_or_desktop() {
    let case = Case::new(Runtime {
        installed: false,
        broken: true,
        ..Runtime::default()
    });
    let mut ws = case.workspace();
    let mut register = Register::new(Output::default());
    let error = apply(
        &mut ws,
        &case.cfg,
        &case.system(),
        &target(),
        &changed_state(false),
        &mut register,
        "herdr",
        false,
    )
    .unwrap_err();
    assert!(error.0.contains("installer failed"), "{}", error.0);
    assert!(register.calls.is_empty());
    assert!(ws.guests.is_empty());
}

#[test]
fn failed_linger_does_not_write_service_or_connect() {
    let case = Case::new(Runtime {
        no_linger: true,
        ..Runtime::default()
    });
    let mut ws = case.workspace();
    let mut register = Register::new(Output::default());
    let error = apply(
        &mut ws,
        &case.cfg,
        &case.system(),
        &target(),
        &changed_state(true),
        &mut register,
        "herdr",
        false,
    )
    .unwrap_err();
    assert!(error.0.contains("administrator"), "{}", error.0);
    assert!(ws.guests.is_empty());
    assert!(register.calls.is_empty());
}

#[test]
fn invalid_session_inspection_refuses_to_start() {
    let tmp = tempfile::tempdir().unwrap();
    let mut ws = TestWorkspace::new(tmp.path());
    ws.answer = Box::new(|_, _| Ok(Output::stdout(r#"{"sessions":[{"name":"default"}]}"#)));
    let error = running_sessions(&mut ws, &settings(tmp.path())).unwrap_err();
    assert!(error.0.contains("refusing"), "{}", error.0);
}

#[test]
fn service_install_is_atomic_and_cannot_overwrite_concurrent_edits() {
    let case = Case::new(Runtime::default());
    case.apply(&changed_state(true), &mut Register::new(Output::default()))
        .unwrap();
    let unit = case.home.join(HERDR_UNIT_PATH);
    assert_eq!(std::fs::read(&unit).unwrap(), UNIT);
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&unit.metadata().unwrap().permissions()) & 0o777,
        0o600
    );
    assert!(
        std::fs::read_to_string(&unit)
            .unwrap()
            .contains("ExecStartPre=-%h/.local/bin/herdr update")
    );
    std::fs::write(&unit, "concurrent edit").unwrap();
    let error = case
        .apply(&changed_state(true), &mut Register::new(Output::default()))
        .unwrap_err();
    assert!(error.0.contains("changed since preflight"), "{}", error.0);
    assert_eq!(std::fs::read_to_string(&unit).unwrap(), "concurrent edit");
}

#[test]
fn existing_desktop_profile_is_renamed_and_enabled_without_authentication() {
    let mut old = profile();
    old["label"] = json!("old");
    old["enabled"] = json!(false);
    let case = Case::new(Runtime {
        profile: Some(old),
        ..Runtime::default()
    });
    let mut register = Register::new(Output::default());
    connect_desktop(&case.system(), &case.cfg, &target(), &mut register, "herdr", false).unwrap();
    assert!(register.calls.is_empty());
    let commands: Vec<Vec<String>> = case
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|a| a[2] != "list")
        .cloned()
        .collect();
    assert_eq!(
        commands,
        [
            ["herdr", "machine", "rename", "profile-id", "--label", "gpu"]
                .map(str::to_owned)
                .to_vec(),
            ["herdr", "machine", "enable", "profile-id"].map(str::to_owned).to_vec(),
        ]
    );
}

#[test]
fn new_desktop_profile_uses_vm_label_and_visible_security_key_notice() {
    let case = Case::new(Runtime {
        profile: None,
        ..Runtime::default()
    });
    let mut register = Register::new(Output::default());
    connect_desktop(&case.system(), &case.cfg, &target(), &mut register, "herdr", false).unwrap();
    assert_eq!(
        register.calls,
        [[
            "herdr",
            "machine",
            "add",
            "user@gpu",
            "--label",
            "gpu",
            "--remote-session",
            "default"
        ]
        .map(str::to_owned)
        .to_vec()]
    );
    assert!(register.activity.iter().any(|m| m.contains("YubiKey")));
}

#[test]
fn desktop_failure_is_reported_as_partial() {
    let case = Case::new(Runtime {
        profile: None,
        ..Runtime::default()
    });
    let mut register = Register::new(Output {
        code: 1,
        ..Output::default()
    });
    let error = connect_desktop(&case.system(), &case.cfg, &target(), &mut register, "herdr", false).unwrap_err();
    assert!(error.0.contains("guest service is configured"), "{}", error.0);
}

#[test]
fn tui_interactive_decision_failure_shows_manual_command_without_external_output() {
    let case = Case::new(Runtime {
        profile: None,
        ..Runtime::default()
    });
    let mut register = Register::new(Output {
        code: 1,
        stdout: "requires interactive approval: PRIVATE_OUTPUT".into(),
        stderr: String::new(),
    });
    let error = connect_desktop(&case.system(), &case.cfg, &target(), &mut register, "herdr", false).unwrap_err();
    assert!(error.0.contains("interactive decision"), "{}", error.0);
    assert!(error.0.contains("herdr machine add user@gpu"));
    assert!(!error.0.contains("PRIVATE_OUTPUT"));
}

#[test]
fn conflicting_label_blocks_and_ip_profile_is_reused() {
    let mut by_ip = profile();
    by_ip["target"] = json!("user@192.0.2.200");
    let case = Case::new(Runtime {
        profile: Some(by_ip.clone()),
        ..Runtime::default()
    });
    assert_eq!(
        desktop_profile(&case.system(), &case.cfg, &target()).unwrap(),
        Some(by_ip)
    );
    let mut other = profile();
    other["target"] = json!("user@other");
    let case = Case::new(Runtime {
        profile: Some(other),
        ..Runtime::default()
    });
    let error = desktop_profile(&case.system(), &case.cfg, &target()).unwrap_err();
    assert!(error.0.contains("label conflict"), "{}", error.0);
}

#[test]
fn unattended_missing_desktop_profile_blocks_before_installer() {
    let case = Case::new(Runtime {
        profile: None,
        ..Runtime::default()
    });
    let error = preflight(&mut case.workspace(), &case.cfg, &case.system(), &target(), true).unwrap_err();
    assert!(error.0.contains("interactive terminal"), "{}", error.0);
    assert!(!case.scripts().contains(INSTALL_COMMAND));
}

#[test]
fn alias_must_resolve_to_exact_workspace_and_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = settings(tmp.path());
    let output = "hostname 192.0.2.200\nuser user\nidentitiesonly yes\nidentityfile ~/.ssh/example-hardware-key\n";
    let right = FakeSystem::new().run(move |_| Ok(Output::stdout(output)));
    verify_desktop_alias(&right, &cfg, &target()).unwrap();
    let wrong = FakeSystem::new().run(move |_| Ok(Output::stdout(&output.replace("192.0.2.200", "192.0.2.201"))));
    let error = verify_desktop_alias(&wrong, &cfg, &target()).unwrap_err();
    assert!(error.0.contains("alias does not match"), "{}", error.0);
}

#[test]
fn blocked_herdr_preflight_prevents_other_selected_actions() {
    let case = Case::new(Runtime::default());
    let codex = defaults().into_iter().find(|e| e.id == "codex").unwrap();
    let plan = build_plan(&case.cfg, &target(), &[codex, herdr_entry()], None, false, true).unwrap();
    // No desktop herdr: Herdr's preflight is blocked.
    let system = case
        .system()
        .which(|name| (name != "herdr").then(|| format!("/usr/bin/{name}")));
    let mut ws = case.workspace();
    let result = execute_plan(
        &case.cfg,
        &system,
        &plan,
        Connection::Open(&mut ws),
        &mut Register::new(Output::default()),
    );
    assert_eq!(result["ok"], json!(false));
    let statuses: Vec<&str> = result["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["not-run", "blocked"]);
    assert!(!case.scripts().contains(hs_cli::setup::definitions::CODEX_RECIPE));
}

#[test]
fn refresh_restores_linger_after_mounting_persistent_home() {
    let vendor = hs_core::cloudinit::vendor_snippet(hs_core::cloudinit::HomeFormat::Keep);
    let mount = vendor.find("mount \"$home_path\"").unwrap();
    let linger = vendor.find("loginctl enable-linger \"$user_name\"").unwrap();
    assert!(mount < linger);
    assert!(vendor.contains("if [ -f \"$home_path/.config/systemd/user/herdr.service\" ]"));
}

fn execute(case: &Case, desktop_exit: i32) -> Value {
    let plan = build_plan(&case.cfg, &target(), &[herdr_entry()], None, false, true).unwrap();
    let mut ws = case.workspace();
    let mut register = Register::new(Output {
        code: desktop_exit,
        ..Output::default()
    });
    register.desktop = Some(Arc::clone(&case.desktop));
    execute_plan(
        &case.cfg,
        &case.system(),
        &plan,
        Connection::Open(&mut ws),
        &mut register,
    )
}

#[test]
fn full_fresh_setup_records_verified_service_and_install_time() {
    let case = Case::new(Runtime {
        installed: false,
        profile: None,
        ..Runtime::default()
    });
    let result = execute(&case, 0);
    assert_eq!(result["ok"], json!(true), "{result}");
    assert!(case.runtime.borrow().active && case.runtime.borrow().linger);
    assert_eq!(std::fs::read(case.home.join(HERDR_UNIT_PATH)).unwrap(), UNIT);
    let registry: Value =
        serde_json::from_str(&std::fs::read_to_string(case.home.join(".local/state/homestack/setup.json")).unwrap())
            .unwrap();
    let record = &registry["items"]["herdr"];
    assert!(record["installed_at"].is_string());
    assert_eq!(record["files"][0]["path"], json!(HERDR_UNIT_PATH));
}

#[test]
fn full_setup_snapshots_previous_unit_and_preserves_unmanaged_server() {
    let case = Case::new(Runtime {
        sessions: vec![("default".into(), true)],
        profile: None,
        ..Runtime::default()
    });
    let unit = case.home.join(HERDR_UNIT_PATH);
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, "old service").unwrap();
    let result = execute(&case, 0);
    assert_eq!(result["ok"], json!(true), "{result}");
    assert_eq!(result["snapshot"]["created"], json!(true));
    let snapshot = case
        .home
        .join(".local/state/homestack")
        .join(result["snapshot"]["id"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(snapshot.join("home").join(HERDR_UNIT_PATH)).unwrap(),
        "old service"
    );
    assert!(
        case.runtime
            .borrow()
            .sessions
            .iter()
            .any(|(n, running)| n == "default" && *running)
    );
    assert!(!case.runtime.borrow().active);
    assert!(
        !case
            .commands
            .borrow()
            .iter()
            .any(|c| c.contains("start herdr.service") || c.contains(INSTALL_COMMAND))
    );
}

#[test]
fn failed_desktop_registration_keeps_guest_service_without_recording_success() {
    let case = Case::new(Runtime {
        installed: false,
        profile: None,
        ..Runtime::default()
    });
    let result = execute(&case, 1);
    assert_eq!(result["ok"], json!(false));
    assert_eq!(result["results"][0]["status"], json!("failed"));
    assert!(
        result["results"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("guest service is configured")
    );
    assert!(case.runtime.borrow().active);
    assert!(case.home.join(HERDR_UNIT_PATH).exists());
    assert!(!case.home.join(".local/state/homestack/setup.json").exists());
}
