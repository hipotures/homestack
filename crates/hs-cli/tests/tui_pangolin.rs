#![allow(clippy::unwrap_used)]
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hs_cli::system::{Output, System};
use hs_cli::tui::{
    pangolin::{self, Report, Wizard},
    state::{App, Overlay, Request},
    theme::Preferences,
};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn open(dir: &std::path::Path) -> Wizard {
    Wizard::open(dir).unwrap()
}
fn edit(w: &mut Wizard, text: &str) {
    w.focus = 1;
    w.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    w.paste(text);
}
fn verify(w: &mut Wizard) {
    let id = w.begin_test().unwrap();
    w.complete(id, report(&w.target.clone(), true));
}
fn report(target: &str, passed: bool) -> Report {
    Report {
        target: target.to_owned(),
        at: "2026-01-02T03:04:05Z".to_owned(),
        passed,
        detail: if passed { "PASS" } else { "SSH authentication failed" }.to_owned(),
        duration_ms: 5,
    }
}

#[test]
fn pangolin_navigation_and_all_eight_stages_at_supported_sizes() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Settings { section: 3 });
    app.key(key(KeyCode::Enter));
    for _ in 0..3 {
        app.key(key(KeyCode::Down));
    }
    app.key(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::Pangolin(_))));
    for (width, height) in [(80, 24), (120, 36), (160, 45)] {
        let buf = hs_cli::tui::draw::render(&app, width, height);
        let text = buf.content.iter().map(|c| c.symbol()).collect::<String>();
        for stage in pangolin::STAGES {
            assert!(text.contains(stage), "Missing {stage} at {width}x{height}");
        }
        assert_eq!(text.matches("[Locked]").count(), 7);
        assert!(text.contains("0/8"));
        assert!(text.contains("SSH connection alias / server hostname"));
    }
    app.key(key(KeyCode::Esc));
    assert!(matches!(app.overlay, Some(Overlay::Integrations { index: 3 })));
}

#[test]
fn pangolin_all_stages_are_navigable_but_only_first_has_actions() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = open(dir.path());
    for step in 1..8 {
        w.key(key(KeyCode::Down));
        assert_eq!(w.selected_step, step);
        assert_eq!(w.focus, 0);
        w.key(key(KeyCode::Tab));
        assert_eq!(w.focus, 0);
        assert!(w.begin_test().is_none());
        w.key(key(KeyCode::F(5)));
        w.paste("must-not-edit");
        assert!(w.target.is_empty());
        w.key(key(KeyCode::F(4)));
        assert!(w.help_open);
        w.key(key(KeyCode::Esc));
        assert!(!w.help_open);
    }
    w.key(key(KeyCode::Down));
    assert_eq!(w.selected_step, 0);
    w.key(key(KeyCode::Enter));
    assert_eq!(w.focus, 1);
    w.key(key(KeyCode::BackTab));
    assert_eq!(w.focus, 0);
    assert!(!w.expanded);
    w.key(key(KeyCode::Char(' ')));
    assert!(w.expanded);
    w.key(key(KeyCode::Char(' ')));
    assert!(!w.expanded);
    assert!(!open(dir.path()).expanded);
    assert!(!dir.path().join("integrations").exists());
}

#[test]
fn pangolin_alias_save_reopen_and_private_atomic_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = open(dir.path());
    assert_eq!(w.status(), "Not configured");
    edit(&mut w, "operator@edge.example.test");
    w.key(key(KeyCode::F(5)));
    assert!(!dir.path().join("integrations/pangolin.toml").exists());
    verify(&mut w);
    assert_eq!(w.status(), "Verified");
    let path = dir.path().join("integrations/pangolin.toml");
    let contents = std::fs::read_to_string(&path).unwrap();
    let settings: toml::Table = toml::from_str(&contents).unwrap();
    assert_eq!(
        settings.get("server_target").unwrap().as_str(),
        Some("operator@edge.example.test")
    );
    assert_eq!(
        settings.get("last_test").unwrap().get("at").unwrap().as_str(),
        Some("2026-01-02T03:04:05Z")
    );
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(
        std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(open(dir.path()).target, w.target);
    assert_eq!(open(dir.path()).status(), "Verified");
    std::fs::write(&path, format!("{contents}\n[unrelated]\nreference = \"keep\"\n")).unwrap();
    edit(&mut w, "ssh-edge");
    verify(&mut w);
    assert!(std::fs::read_to_string(&path).unwrap().contains("reference = \"keep\""));
    assert!(!format!("{w:?}").contains("ssh-edge"));
}

#[test]
fn pangolin_editing_invalidates_verification_and_stale_results_cannot_save() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = open(dir.path());
    edit(&mut w, "edge-one");
    let id = w.begin_test().unwrap();
    assert!(w.begin_test().is_none());
    assert_eq!(w.status(), "Testing");
    edit(&mut w, "edge-two");
    w.complete(id, report("edge-one", true));
    assert!(w.verified.is_none());
    assert!(!dir.path().join("integrations").exists());
    let id = w.begin_test().unwrap();
    w.complete(id, report("edge-two", true));
    assert_eq!(w.status(), "Verified");
    edit(&mut w, "edge-one");
    edit(&mut w, "edge-two");
    assert_eq!(w.status(), "Verification outdated");
    assert!(w.verified.is_none());
    let id = w.begin_test().unwrap();
    w.complete(id, report("edge-two", false));
    assert_eq!(w.status(), "Failed");
    assert_eq!(open(dir.path()).target, "edge-two");
}

#[test]
fn pangolin_failure_cancellation_and_unsaved_escape_preserve_saved_target() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = open(dir.path());
    edit(&mut w, "saved-edge");
    verify(&mut w);
    edit(&mut w, "draft-edge");
    let id = w.begin_test().unwrap();
    w.complete(id, report("draft-edge", false));
    assert_eq!(open(dir.path()).target, "saved-edge");
    let id = w.begin_test().unwrap();
    assert!(!w.key(key(KeyCode::Esc)));
    assert!(!w.testing);
    w.complete(id, report("draft-edge", true));
    assert!(w.verified.is_none());
    assert!(!w.key(key(KeyCode::Esc)));
    assert!(w.key(key(KeyCode::Esc)));
    assert_eq!(open(dir.path()).target, "saved-edge");
}

#[test]
fn pangolin_reopened_wizard_rejects_old_success_and_app_tests_current_draft() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    let mut w = open(dir.path());
    edit(&mut w, "draft-edge");
    app.overlay = Some(Overlay::Pangolin(Box::new(w)));
    let Some(Request::TestPangolin(id, target)) = app.key(key(KeyCode::F(6))) else {
        panic!("Expected SSH test");
    };
    assert_eq!(target, "draft-edge");
    assert!(app.key(key(KeyCode::F(6))).is_none());
    let mut reopened = open(dir.path());
    edit(&mut reopened, "draft-edge");
    reopened.complete(id, report("draft-edge", true));
    assert!(reopened.verified.is_none());
    assert!(!dir.path().join("integrations").exists());
}

fn test_ssh(system: &dyn System, target: &str, prefix: &[String], stop: &AtomicBool) -> Result<String, String> {
    pangolin::test_ssh(
        system,
        target,
        prefix,
        stop,
        &hs_cli::touch::Sounds::disabled(),
        hs_cli::touch::default_player(),
    )
}

struct Mock {
    calls: Mutex<Vec<Vec<String>>>,
    config: String,
    output: Output,
    error: Option<io::ErrorKind>,
}
impl Mock {
    fn new(output: Output) -> Self {
        Self { calls: Mutex::new(Vec::new()), config: "hostname edge.example.test\nuser operator\nport 2222\nstricthostkeychecking ask\nidentityfile /synthetic/private-identity\n".to_owned(), output, error: None }
    }
}
impl System for Mock {
    fn which(&self, _: &str) -> Option<String> {
        Some("ssh".to_owned())
    }
    fn monotonic(&self) -> f64 {
        0.0
    }
    fn run(&self, _: &[String]) -> io::Result<Output> {
        panic!("Must use cancellable execution")
    }
    fn run_master(&self, _: &[String]) -> io::Result<Output> {
        panic!("No persistent master")
    }
    fn run_until(&self, argv: &[String], _: &AtomicBool, timeout: Duration) -> io::Result<Output> {
        assert!(timeout <= Duration::from_secs(45));
        self.calls.lock().unwrap().push(argv.to_vec());
        if argv.iter().any(|arg| arg == "-G") {
            return Ok(Output::stdout(&self.config));
        }
        if let Some(kind) = self.error {
            return Err(io::Error::new(kind, "synthetic-secret-do-not-disclose"));
        }
        Ok(self.output.clone())
    }
}

#[test]
fn pangolin_mocked_ssh_success_resolves_config_and_only_executes_true() {
    let mock = Mock::new(Output::stdout("synthetic-secret-do-not-disclose"));
    let result = test_ssh(
        &mock,
        "operator@edge.example.test",
        &["bridge-prefix".to_owned()],
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(result.contains("SSH authenticated, remote command successful"));
    assert!(!result.contains("synthetic-secret"));
    assert!(!result.contains("private-identity"));
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].iter().any(|a| a == "-G"));
    assert_eq!(
        &calls[1][calls[1].len() - 3..],
        &["--", "operator@edge.example.test", "true"]
    );
    for argv in calls.iter() {
        assert_eq!(argv[0], "bridge-prefix");
        for option in [
            "ControlPath=none",
            "ClearAllForwardings=yes",
            "PermitLocalCommand=no",
            "RemoteCommand=none",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
        ] {
            assert!(argv.iter().any(|a| a == option));
        }
        for forbidden in [
            "StrictHostKeyChecking=no",
            "UserKnownHostsFile=/dev/null",
            "sudo",
            "docker",
            "scp",
        ] {
            assert!(!argv.iter().any(|a| a == forbidden));
        }
    }
}

#[test]
fn pangolin_mocked_auth_unreachable_timeout_and_cancel_are_sanitized() {
    for (output, error, expected) in [
        (
            Output::failed(255, "Permission denied synthetic-secret-do-not-disclose"),
            None,
            "authentication failed",
        ),
        (
            Output::failed(255, "No route to host synthetic-secret-do-not-disclose"),
            None,
            "unreachable",
        ),
        (
            Output::failed(255, "Connection timed out synthetic-secret-do-not-disclose"),
            None,
            "timed out",
        ),
        (Output::default(), Some(io::ErrorKind::TimedOut), "timed out"),
        (Output::default(), Some(io::ErrorKind::Interrupted), "cancelled"),
        (
            Output::failed(
                255,
                "REMOTE HOST IDENTIFICATION HAS CHANGED synthetic-secret-do-not-disclose",
            ),
            None,
            "host key verification failed",
        ),
        (
            Output::failed(1, "synthetic-secret-do-not-disclose"),
            None,
            "remote command failed",
        ),
    ] {
        let mut mock = Mock::new(output);
        mock.error = error;
        let error = test_ssh(&mock, "edge-test", &[], &AtomicBool::new(false)).unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains("synthetic-secret"));
    }
    let mock = Mock::new(Output::default());
    assert!(
        test_ssh(&mock, "edge-test", &[], &AtomicBool::new(true))
            .unwrap_err()
            .contains("cancelled")
    );
    assert!(mock.calls.lock().unwrap().is_empty());
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    {
        let _pending = pangolin::Pending {
            id: 1,
            cancel: cancel.clone(),
        };
    }
    assert!(cancel.load(Ordering::SeqCst));
}

#[test]
fn pangolin_invalid_destinations_and_insecure_config_never_connect() {
    for target in [
        "edge-alias",
        "operator@edge.example.test",
        "192.0.2.5",
        "operator@[2001:db8::5]",
        "::1",
    ] {
        assert!(pangolin::validate_destination(target).is_ok(), "{target}");
    }
    for target in [
        "",
        "-oProxyCommand=bad",
        "ssh://edge",
        "edge;true",
        "a b",
        "a\nb",
        "u:password@edge",
        "u@@edge",
        "[invalid]",
        "user@-edge",
    ] {
        let mock = Mock::new(Output::default());
        assert!(
            test_ssh(&mock, target, &[], &AtomicBool::new(false)).is_err(),
            "{target}"
        );
        assert!(mock.calls.lock().unwrap().is_empty());
    }
    let mut mock = Mock::new(Output::default());
    mock.config = mock
        .config
        .replace("stricthostkeychecking ask", "stricthostkeychecking false");
    assert!(
        test_ssh(&mock, "edge-test", &[], &AtomicBool::new(false))
            .unwrap_err()
            .contains("host key verification")
    );
    assert_eq!(mock.calls.lock().unwrap().len(), 1);
    mock.config = "synthetic-secret-do-not-disclose".to_owned();
    let error = test_ssh(&mock, "edge-test", &[], &AtomicBool::new(false)).unwrap_err();
    assert!(error.contains("resolve"));
    assert!(!error.contains("synthetic-secret"));
}

#[test]
fn pangolin_local_command_deadline_and_cancellation_reap_without_ssh() {
    // Exercise the real bounded process runner with a harmless local program;
    // all SSH tests above use mocked execution and never read SSH credentials.
    let system = hs_cli::system::RealSystem::default();
    let command = vec!["sleep".to_owned(), "5".to_owned()];
    let started = std::time::Instant::now();
    let error = system
        .run_until(&command, &AtomicBool::new(false), Duration::from_millis(40))
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(2));
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let cancel = stop.clone();
    let signal = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(40));
        cancel.store(true, Ordering::SeqCst);
    });
    let error = system.run_until(&command, &stop, Duration::from_secs(5)).unwrap_err();
    signal.join().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert!(started.elapsed() < Duration::from_secs(2));
    let output = system
        .run_until(&["true".to_owned()], &AtomicBool::new(false), Duration::from_secs(1))
        .unwrap();
    assert_eq!(output.code, 0);
}

#[test]
fn pangolin_verified_progress_keeps_future_stages_locked() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = open(dir.path());
    edit(&mut w, "verified-edge");
    let id = w.begin_test().unwrap();
    w.complete(id, report("verified-edge", true));
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Pangolin(Box::new(w)));
    let buf = hs_cli::tui::draw::render(&app, 80, 24);
    let text = buf.content.iter().map(|c| c.symbol()).collect::<String>();
    assert!(text.contains("1/8"));
    assert!(text.contains("Verified"));
    assert_eq!(text.matches("[Locked]").count(), 6);
}

fn rendered(app: &App, width: u16, height: u16) -> String {
    use ratatui::{Terminal, backend::TestBackend};
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| hs_cli::tui::draw::draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..height {
        for x in 0..width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

#[test]
fn pangolin_exact_testbackend_renders_and_responsive_columns() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = open(dir.path());
    edit(&mut w, "edge-lab");
    verify(&mut w);
    w.focus = 0;
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Pangolin(Box::new(w)));
    for (width, height) in [(80, 24), (120, 36), (160, 45)] {
        let text = rendered(&app, width, height);
        assert!(!text.contains("[ Save ]"));
        assert!(!text.contains("F5 save"));
        assert_eq!(text.lines().count(), usize::from(height));
        assert!(text.lines().all(|line| line.chars().count() == usize::from(width)));
        for stage in pangolin::STAGES {
            assert!(text.contains(stage));
        }
        for label in ["[ Test ]", "F4 help", "edge-lab", "Last test: PASS"] {
            assert!(text.contains(label), "Missing {label}");
        }
        let form_row = text
            .lines()
            .position(|line| line.contains("SSH connection alias / server hostname"))
            .unwrap();
        let last_step_row = text
            .lines()
            .position(|line| line.contains("8. Integration verification"))
            .unwrap();
        if width >= 120 {
            assert!(form_row < last_step_row);
            assert!(text.contains("STEPS"));
            assert!(text.contains("No private keys or authentication secrets"));
        } else {
            assert!(form_row > last_step_row);
        }
        if let Some(output) = std::env::var_os("HOMESTACK_PANGOLIN_RENDER_DIR") {
            let output = std::path::PathBuf::from(output);
            std::fs::create_dir_all(&output).unwrap();
            std::fs::write(output.join(format!("pangolin-{width}x{height}.txt")), text).unwrap();
        }
    }
}

#[test]
fn pangolin_context_help_is_readable_for_every_locked_stage_and_blocks_actions() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = open(dir.path());
    edit(&mut w, "draft-edge");
    w.focus = 0;
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Pangolin(Box::new(w)));
    for step in 0..8 {
        if let Some(Overlay::Pangolin(w)) = app.overlay.as_mut() {
            w.select_step(step);
        }
        assert!(app.key(key(KeyCode::F(4))).is_none());
        let text = rendered(&app, 80, 24);
        assert!(text.contains(&format!("help · {}.", step + 1)));
        if step > 2 {
            assert!(text.contains("Locked - not implemented."));
        }
        assert!(app.key(key(KeyCode::F(5))).is_none());
        assert!(app.key(key(KeyCode::F(6))).is_none());
        assert!(!dir.path().join("integrations").exists());
        app.key(key(KeyCode::Esc));
        assert!(matches!(&app.overlay, Some(Overlay::Pangolin(w)) if !w.help_open));
    }
    if let Some(Overlay::Pangolin(w)) = app.overlay.as_mut() {
        w.select_step(0);
    }
    app.key(key(KeyCode::F(4)));
    for _ in 0..4 {
        app.key(key(KeyCode::PageDown));
    }
    let text = rendered(&app, 80, 24);
    assert!(text.contains("configure Pangolin."), "{text}");
    if let Some(output) = std::env::var_os("HOMESTACK_PANGOLIN_RENDER_DIR") {
        let output = std::path::PathBuf::from(output);
        std::fs::create_dir_all(&output).unwrap();
        std::fs::write(output.join("pangolin-help-80x24.txt"), &text).unwrap();
    }
    app.key(key(KeyCode::Home));
    assert!(rendered(&app, 80, 24).contains("Select an OpenSSH alias"));
}

#[test]
fn pangolin_mouse_selects_stages_and_modal_hides_underlying_actions() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Pangolin(Box::new(open(dir.path()))));
    // Render registers hits on this test thread, as in the real event loop.
    rendered(&app, 120, 36);
    let click = |column, row| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    app.mouse(click(5, 6)); // Second stage in the wide navigation pane.
    assert!(matches!(&app.overlay, Some(Overlay::Pangolin(w)) if w.selected_step == 1));
    assert!(app.key(key(KeyCode::F(6))).is_none());
    rendered(&app, 120, 36);
    app.mouse(click(5, 5));
    assert!(matches!(&app.overlay, Some(Overlay::Pangolin(w)) if w.selected_step == 0));
    app.key(key(KeyCode::F(4)));
    rendered(&app, 120, 36);
    app.mouse(click(5, 7));
    assert!(matches!(&app.overlay, Some(Overlay::Pangolin(w)) if w.selected_step == 0 && w.help_open));
    assert!(!dir.path().join("integrations").exists());
}

#[test]
fn pangolin_testing_has_no_save_action_or_verbose_footer_banner() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = open(dir.path());
    edit(&mut w, "edge-test");
    w.begin_test().unwrap();
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Pangolin(Box::new(w)));
    for (width, height) in [(80, 24), (120, 36), (160, 45)] {
        let text = rendered(&app, width, height);
        assert!(text.contains("Connection: Testing SSH"));
        assert!(!text.contains("[ Save ]"));
        assert!(!text.contains("F5 save"));
        assert!(!text.contains("Resolving OpenSSH"));
        assert!(!text.contains("touch your key"));
        assert!(
            text.lines()
                .nth(usize::from(height) - 2)
                .unwrap()
                .trim_matches(|c: char| c == '│' || c.is_whitespace())
                .is_empty()
        );
    }
    assert!(!dir.path().join("integrations").exists());
}

#[test]
fn pangolin_reopening_retains_success_date_report_and_progress_without_retesting() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = open(dir.path());
    edit(&mut w, "persistent-edge");
    verify(&mut w);
    drop(w);
    let w = open(dir.path());
    assert_eq!(w.status(), "Verified");
    let last = w.report.as_ref().unwrap();
    assert_eq!(last.at, "2026-01-02T03:04:05Z");
    assert_eq!(last.target, "persistent-edge");
    assert_eq!(last.duration_ms, 5);
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Pangolin(Box::new(w)));
    let text = rendered(&app, 80, 24);
    assert!(text.contains("1/8 completed"));
    assert!(text.contains("Last test: PASS · 02.01.2026 03:04:05"));
    app.key(key(KeyCode::F(7)));
    assert!(rendered(&app, 80, 24).contains("Date (local): 02.01.2026 03:04:05"));
    app.key(key(KeyCode::Esc));
    if let Some(Overlay::Pangolin(w)) = app.overlay.as_mut() {
        edit(w, "changed-edge");
        assert_eq!(w.status(), "Verification outdated");
        assert!(w.verified.is_none());
    }
}

#[test]
fn pangolin_saved_target_without_matching_success_is_not_verified() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = open(dir.path());
    edit(&mut w, "recorded-edge");
    let id = w.begin_test().unwrap();
    let mut result = report("recorded-edge", true);
    result.detail = "synthetic-sensitive-output-must-not-persist".to_owned();
    w.complete(id, result);
    let path = dir.path().join("integrations/pangolin.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("synthetic-sensitive-output"));
    // An edited local setting cannot inherit verification for another target.
    let changed = text.replacen("server_target = \"recorded-edge\"", "server_target = \"other-edge\"", 1);
    std::fs::write(&path, changed).unwrap();
    assert!(open(dir.path()).verified.is_none());
    std::fs::write(&path, "server_target = \"other-edge\"\n").unwrap();
    assert_eq!(open(dir.path()).status(), "Saved, not verified");
}

#[test]
fn pangolin_ssh_reuses_existing_wait_success_and_failure_sounds() {
    use hs_cli::touch::{Event, Player, Sounds};
    use std::sync::{Arc, mpsc};
    struct CueMock {
        inner: Mock,
        wait: Mutex<mpsc::Receiver<()>>,
    }
    impl System for CueMock {
        fn which(&self, name: &str) -> Option<String> {
            self.inner.which(name)
        }
        fn monotonic(&self) -> f64 {
            0.0
        }
        fn run(&self, _: &[String]) -> io::Result<Output> {
            panic!("No unbounded SSH execution")
        }
        fn run_master(&self, _: &[String]) -> io::Result<Output> {
            panic!("No master")
        }
        fn run_until(&self, argv: &[String], stop: &AtomicBool, timeout: Duration) -> io::Result<Output> {
            if !argv.iter().any(|a| a == "-G") {
                self.wait.lock().unwrap().recv_timeout(Duration::from_secs(2)).unwrap();
            }
            self.inner.run_until(argv, stop, timeout)
        }
    }
    for passed in [true, false] {
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = events.clone();
        let (send, wait) = mpsc::channel();
        let player: Player = Arc::new(move |sounds, event| {
            assert_eq!(sounds.repeat_seconds, 9.0);
            recorded.lock().unwrap().push(event);
            if event == Event::Wait {
                let _ = send.send(());
            }
        });
        let mock = CueMock {
            inner: Mock::new(if passed {
                Output::stdout("")
            } else {
                Output::failed(255, "Permission denied")
            }),
            wait: Mutex::new(wait),
        };
        let sounds = Sounds {
            initial_delay: 0.0,
            repeat_seconds: 9.0,
            ..Sounds::default()
        };
        let result = pangolin::test_ssh(&mock, "sound-test-edge", &[], &AtomicBool::new(false), &sounds, player);
        assert_eq!(result.is_ok(), passed);
        let events = events.lock().unwrap();
        assert_eq!(events.first(), Some(&Event::Wait));
        assert_eq!(events.last(), Some(if passed { &Event::Ok } else { &Event::Fail }));
    }
}

#[test]
fn pangolin_test_date_is_local_readable_and_has_no_fractional_seconds() {
    let mut r = report("date-test-edge", true);
    r.at = "2026-10-20T18:43:35.145881743Z".to_owned();
    r.duration_ms = 12929;
    assert_eq!(r.local_date(7200), "20.10.2026 20:43:35");
    assert_eq!(r.duration(), "12.9 s");
    r.at = "2026-12-31T23:59:59.999Z".to_owned();
    assert_eq!(r.local_date(3600), "01.01.2027 00:59:59");
    r.duration_ms = 5;
    assert_eq!(r.duration(), "5 ms");
    let dir = tempfile::tempdir().unwrap();
    let mut w = open(dir.path());
    edit(&mut w, "date-test-edge");
    let id = w.begin_test().unwrap();
    r.at = "2026-10-20T18:43:35.145881743Z".to_owned();
    r.duration_ms = 12929;
    w.complete(id, r);
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.utc_offset = 7200;
    app.overlay = Some(Overlay::Pangolin(Box::new(w)));
    let text = rendered(&app, 80, 24);
    assert!(text.contains("20.10.2026 20:43:35 · 12.9 s"));
    assert!(!text.contains("145881743"));
    app.key(key(KeyCode::F(7)));
    let text = rendered(&app, 80, 24);
    assert!(text.contains("Date (local): 20.10.2026 20:43:35"));
    assert!(!text.contains("145881743"));
}

#[test]
fn pangolin_success_uses_connection_status_without_redundant_footer_message() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = open(dir.path());
    edit(&mut w, "quiet-success-edge");
    verify(&mut w);
    assert_eq!(w.status(), "Verified");
    assert!(w.message.is_empty());
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Pangolin(Box::new(w)));
    for (width, height) in [(80, 24), (120, 36), (160, 45)] {
        let text = rendered(&app, width, height);
        assert!(text.contains("Connection: Verified"));
        assert!(text.contains("Last test: PASS"));
        assert!(!text.contains("target saved"));
        assert!(!text.contains("remote command successful"));
        assert!(
            text.lines()
                .nth(usize::from(height) - 2)
                .unwrap()
                .trim_matches(|c: char| c == '│' || c.is_whitespace())
                .is_empty()
        );
    }
}

fn prerequisites_wizard(dir: &std::path::Path) -> Wizard {
    let mut w = open(dir);
    edit(&mut w, "prerequisite-lab");
    verify(&mut w);
    w.select_step(1);
    w
}

#[test]
fn pangolin_prerequisites_require_current_verified_and_saved_server() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = open(dir.path());
    w.select_step(1);
    assert!(!w.prerequisites_available());
    w.toggle_prerequisites();
    assert!(!w.prerequisites_confirmed());
    assert!(!dir.path().join("integrations").exists());
    w.select_step(0);
    edit(&mut w, "prerequisite-lab");
    let id = w.begin_test().unwrap();
    w.complete(id, report("prerequisite-lab", false));
    assert!(!w.prerequisites_available());
    verify(&mut w);
    assert!(w.prerequisites_available());
    w.select_step(1);
    assert!(w.begin_test().is_none());
}

#[test]
fn pangolin_prerequisite_headers_keyboard_and_checkbox_persist_without_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = prerequisites_wizard(dir.path());
    assert_eq!(w.completed(), 1);
    w.key(key(KeyCode::Tab));
    assert_eq!(w.focus, 1);
    w.key(key(KeyCode::Right));
    assert!(w.instructions[0]);
    w.key(key(KeyCode::Left));
    assert!(!w.instructions[0]);
    w.key(key(KeyCode::Enter));
    assert!(w.instructions[0]);
    w.key(key(KeyCode::Enter));
    assert!(!w.instructions[0]);
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Enter));
    assert!(w.instructions[1]);
    w.key(key(KeyCode::Left));
    assert!(!w.instructions[1]);
    w.key(key(KeyCode::Right));
    assert!(w.instructions[1]);
    let original_target = w.target.clone();
    w.key(key(KeyCode::Char('u')));
    assert_eq!(w.target, original_target);
    w.key(key(KeyCode::Tab));
    w.key(key(KeyCode::Char(' ')));
    assert!(w.prerequisites_confirmed());
    assert_eq!(w.completed(), 2);
    let path = dir.path().join("integrations/pangolin.toml");
    let stored = std::fs::read_to_string(&path).unwrap();
    let table: toml::Table = toml::from_str(&stored).unwrap();
    let manual = table["prerequisites"].as_table().unwrap();
    assert_eq!(manual.len(), 2);
    assert_eq!(manual["target"].as_str(), Some("prerequisite-lab"));
    assert_eq!(manual["confirmed"].as_bool(), Some(true));
    assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!stored.contains("token"));
    let mut restored = open(dir.path());
    assert!(restored.prerequisites_confirmed());
    restored.select_step(1);
    restored.focus = 3;
    restored.key(key(KeyCode::Char(' ')));
    assert_eq!(restored.completed(), 1);
    assert!(!open(dir.path()).prerequisites_confirmed());
    // Phase 2 never queues a worker or remote operation.
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Pangolin(Box::new(restored)));
    assert!(app.key(key(KeyCode::F(6))).is_none());
    assert!(app.key(key(KeyCode::Char(' '))).is_none());
}

#[test]
fn pangolin_prerequisites_invalidate_on_target_edit_and_do_not_return_after_retest() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = prerequisites_wizard(dir.path());
    w.toggle_prerequisites();
    assert!(w.prerequisites_confirmed());
    w.select_step(0);
    edit(&mut w, "changed-prerequisite-lab");
    assert!(!w.prerequisites_available());
    assert!(!w.prerequisites_confirmed());
    verify(&mut w);
    assert_eq!(w.completed(), 1);
    assert!(!open(dir.path()).prerequisites_confirmed());
    edit(&mut w, "prerequisite-lab");
    verify(&mut w);
    assert!(!open(dir.path()).prerequisites_confirmed());
}

#[test]
fn pangolin_prerequisites_write_failure_preserves_state_and_allows_retry() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = prerequisites_wizard(dir.path());
    let path = dir.path().join("integrations/pangolin.toml");
    let stored = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    w.toggle_prerequisites();
    assert!(!w.prerequisites_confirmed());
    assert_eq!(w.completed(), 1);
    assert!(w.message.contains("Space retries"));
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, format!("{stored}\n[unrelated]\nkeep = true\n")).unwrap();
    w.focus = 3;
    w.key(key(KeyCode::Char(' ')));
    assert!(w.prerequisites_confirmed());
    assert!(w.message.is_empty());
    assert!(std::fs::read_to_string(&path).unwrap().contains("keep = true"));
    // A failed attempt to uncheck does not falsely report that it was saved.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    w.toggle_prerequisites();
    assert!(w.prerequisites_confirmed());
    assert!(!w.message.is_empty());
}

#[test]
fn pangolin_prerequisites_mouse_headers_and_checkbox_use_registered_hits() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Pangolin(Box::new(prerequisites_wizard(dir.path()))));
    let click_label = |app: &mut App, label: &str| {
        let text = rendered(app, 120, 36);
        let (row, line) = text.lines().enumerate().find(|(_, line)| line.contains(label)).unwrap();
        let column = line
            .chars()
            .collect::<Vec<_>>()
            .windows(label.chars().count())
            .position(|chars| chars.iter().collect::<String>() == label)
            .unwrap();
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: column as u16,
            row: row as u16,
            modifiers: KeyModifiers::NONE,
        });
    };
    click_label(&mut app, "▸ Pangolin installation");
    assert!(matches!(&app.overlay, Some(Overlay::Pangolin(w)) if w.instructions[0]));
    click_label(&mut app, "▾ Pangolin installation");
    assert!(matches!(&app.overlay, Some(Overlay::Pangolin(w)) if !w.instructions[0]));
    click_label(&mut app, "▸ Bootstrap API key");
    assert!(matches!(&app.overlay, Some(Overlay::Pangolin(w)) if w.instructions[1]));
    click_label(&mut app, "[ ] I have completed these prerequisites");
    assert!(matches!(&app.overlay, Some(Overlay::Pangolin(w)) if w.prerequisites_confirmed()));
    click_label(&mut app, "[x] I have completed these prerequisites");
    assert!(matches!(&app.overlay, Some(Overlay::Pangolin(w)) if !w.prerequisites_confirmed()));
}

#[test]
fn pangolin_phase2_testbackend_snapshots_help_and_future_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = prerequisites_wizard(dir.path());
    w.instructions = [true; 2];
    w.focus = 3;
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Pangolin(Box::new(w)));
    for (width, height) in [(80, 24), (120, 36), (160, 45)] {
        let text = rendered(&app, width, height);
        for label in [
            "Pangolin installation",
            "Bootstrap API key",
            "I have completed these prerequisites",
            "1/8",
            "F4 help",
        ] {
            assert!(text.contains(label), "Missing {label} at {width}x{height}\n{text}");
        }
        let buffer = hs_cli::tui::draw::render(&app, width, height);
        let (row, line) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("[ ] I have completed"))
            .unwrap();
        let column = line.chars().position(|c| c == '[').unwrap();
        assert_eq!(buffer[(column as u16, row as u16)].bg, app.palette.selection);
        assert!(!text.contains("Awaiting confirmation"));
        assert!(!text.contains("[ Test ]"));
        assert!(!text.contains("F6 test"));
        assert!(!text.contains("[ Save ]"));
        assert!(!text.contains("PASS"));
        assert!(!text.contains("Verified"));
        assert_eq!(text.lines().count(), height as usize);
        if let Some(output) = std::env::var_os("HOMESTACK_PANGOLIN_RENDER_DIR") {
            let output = std::path::PathBuf::from(output);
            std::fs::create_dir_all(&output).unwrap();
            std::fs::write(output.join(format!("pangolin-phase2-{width}x{height}.txt")), &text).unwrap();
        }
        // All expanded instruction text is reachable, even in the compact page.
        let mut seen = text;
        for _ in 0..20 {
            app.key(key(KeyCode::PageDown));
            seen.push_str(&rendered(&app, width, height));
        }
        for action in [
            "createApiKey",
            "listApiKeys",
            "listApiKeyActions",
            "setApiKeyActions",
            "Authorization: Root",
        ] {
            assert!(seen.contains(action), "Missing {action} at {width}x{height}");
        }
        if let Some(Overlay::Pangolin(w)) = app.overlay.as_mut() {
            w.prerequisites_scroll = 0;
        }
    }
    app.key(key(KeyCode::Char(' ')));
    assert!(rendered(&app, 120, 36).contains("2/8"));
    assert!(!rendered(&app, 120, 36).contains("Confirmed by user"));
    app.key(key(KeyCode::F(4)));
    let mut help = String::new();
    for _ in 0..12 {
        help.push_str(&rendered(&app, 80, 24));
        app.key(key(KeyCode::PageDown));
    }
    for phrase in [
        "Root scope",
        "createApiKey",
        "listApiKeys",
        "listApiKeyActions",
        "setApiKeyActions",
        "Step 6",
        "Revoke",
        "manual declaration",
    ] {
        assert!(help.contains(phrase), "Missing {phrase}");
    }
    app.key(key(KeyCode::Esc));
    assert_eq!(pangolin::STAGES[2], "Pangolin discovery");
    for step in 3..8 {
        if let Some(Overlay::Pangolin(w)) = app.overlay.as_mut() {
            w.select_step(step);
        }
        assert!(app.key(key(KeyCode::F(6))).is_none());
        assert!(app.key(key(KeyCode::Char(' '))).is_none());
        assert!(rendered(&app, 120, 36).contains("Locked - not implemented."));
        app.key(key(KeyCode::F(4)));
        if step == 2 {
            assert!(rendered(&app, 120, 36).contains("Pangolin discovery"));
        }
        app.key(key(KeyCode::Esc));
    }
}

#[test]
fn pangolin_retesting_same_server_keeps_manual_confirmation_but_stale_files_cannot_confirm() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = prerequisites_wizard(dir.path());
    w.toggle_prerequisites();
    w.select_step(0);
    verify(&mut w);
    assert!(open(dir.path()).prerequisites_confirmed());
    w.select_step(1);
    let path = dir.path().join("integrations/pangolin.toml");
    let saved = std::fs::read_to_string(&path).unwrap();
    let replaced = saved.replace("prerequisite-lab", "another-lab");
    std::fs::write(&path, &replaced).unwrap();
    w.toggle_prerequisites();
    assert!(w.message.contains("Server settings changed"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), replaced);
    // A manually saved target without successful evidence cannot unlock Step 2.
    std::fs::write(
        &path,
        "server_target = \"unverified-lab\"\n[prerequisites]\ntarget = \"unverified-lab\"\nconfirmed = true\n",
    )
    .unwrap();
    let unverified = open(dir.path());
    assert!(!unverified.prerequisites_available());
    assert!(!unverified.prerequisites_confirmed());
}

#[test]
fn pangolin_prerequisites_enter_continues_without_unchecking_or_executing_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = prerequisites_wizard(dir.path());
    w.focus = 3;
    w.key(key(KeyCode::Char(' ')));
    let path = dir.path().join("integrations/pangolin.toml");
    let saved = std::fs::read_to_string(&path).unwrap();
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Pangolin(Box::new(w)));
    for (width, height) in [(80, 24), (120, 36), (160, 45)] {
        let text = rendered(&app, width, height);
        assert!(text.contains("Enter next"));
        assert!(text.contains("Space toggle"));
        assert!(text.contains("Esc back"));
    }
    assert!(app.key(key(KeyCode::Enter)).is_none());
    assert!(
        matches!(&app.overlay, Some(Overlay::Pangolin(w)) if w.selected_step == 2 && w.prerequisites_confirmed() && w.completed() == 2)
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
    assert!(open(dir.path()).prerequisites_confirmed());
    assert!(rendered(&app, 120, 36).contains("[ Discover ]"));
    assert!(matches!(
        app.key(key(KeyCode::F(6))),
        Some(Request::DiscoverPangolin(_, _))
    ));
}

#[test]
fn pangolin_prerequisites_enter_saves_before_advancing_and_stays_on_write_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = prerequisites_wizard(dir.path());
    w.focus = 3;
    let path = dir.path().join("integrations/pangolin.toml");
    let saved = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    w.key(key(KeyCode::Enter));
    assert_eq!(w.selected_step, 1);
    assert!(!w.prerequisites_confirmed());
    assert!(!w.message.is_empty());
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, saved).unwrap();
    w.key(key(KeyCode::Enter));
    assert_eq!(w.selected_step, 2);
    assert_eq!(w.completed(), 2);
    assert!(open(dir.path()).prerequisites_confirmed());
    w.select_step(1);
    w.focus = 3;
    w.key(key(KeyCode::Char(' ')));
    assert_eq!(w.completed(), 1);
    assert!(!open(dir.path()).prerequisites_confirmed());
}

fn discovery_wizard(dir: &std::path::Path) -> Wizard {
    let mut w = prerequisites_wizard(dir);
    w.toggle_prerequisites();
    w.select_step(2);
    w
}
fn inventory(target: &str, enabled: bool) -> pangolin::Discovery {
    pangolin::Discovery::parse(target, &serde_json::json!({
        "outcome":"Discovered", "facts":{
            "installation":"Docker Compose", "identity":"aaaaaaaaaaaa", "state":"Running",
            "image":"fosrl/pangolin:synthetic", "version_evidence":"Image reference only; running software version unverified",
            "compose":"/srv/synthetic/compose.yml", "config":"/srv/synthetic/config.yml", "config_status":"Literal settings parsed",
            "api_enabled":enabled.to_string(), "api_port":"4567", "api_listening":"Yes", "api_publication":"Loopback only"
        }
    }).to_string()).unwrap()
}
#[test]
fn pangolin_discovery_dependencies_completion_restore_and_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = prerequisites_wizard(dir.path());
    w.select_step(2);
    assert!(w.begin_discovery().is_none());
    w.toggle_prerequisites();
    assert!(w.discovery_available());
    let id = w.begin_discovery().unwrap();
    assert!(w.begin_discovery().is_none());
    assert_eq!(w.discovery_status(), "Discovering");
    assert_eq!(w.status(), "Verified");
    assert_eq!(w.completed(), 2);
    w.complete_discovery(id, inventory(&w.target, false));
    assert_eq!(w.discovery_status(), "Discovered");
    assert_eq!(w.completed(), 3);
    let restored = open(dir.path());
    assert_eq!(restored.completed(), 3);
    assert_eq!(restored.discovery, w.discovery);
    let id = w.begin_discovery().unwrap();
    let mut next = inventory(&w.target, true);
    next.facts.insert("api_port".to_owned(), "4568".to_owned());
    w.complete_discovery(id, next);
    let saved = std::fs::read_to_string(dir.path().join("integrations/pangolin.toml")).unwrap();
    assert_eq!(saved.matches("[discovery]").count(), 1);
    assert_eq!(open(dir.path()).discovery.unwrap().fact("api_port"), "4568");
    w.select_step(0);
    edit(&mut w, "replacement.example.test");
    assert_eq!(w.discovery_status(), "Outdated");
    assert_eq!(w.completed(), 0);
}
#[test]
fn pangolin_discovery_cancellation_stale_result_and_persistence_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = discovery_wizard(dir.path());
    let id = w.begin_discovery().unwrap();
    w.key(key(KeyCode::Esc));
    assert_eq!(w.discovery_status(), "Cancelled");
    assert_eq!(open(dir.path()).discovery_status(), "Cancelled");
    w.complete_discovery(id, inventory(&w.target, true));
    assert_eq!(w.discovery_status(), "Cancelled");
    assert!(w.prerequisites_confirmed());
    let id = w.begin_discovery().unwrap();
    w.select_step(0);
    edit(&mut w, "changed.example.test");
    w.complete_discovery(id, inventory("prerequisite-lab", true));
    assert_ne!(w.discovery_status(), "Discovered");
    let mut w = discovery_wizard(dir.path());
    let id = w.begin_discovery().unwrap();
    let path = dir.path().join("integrations/pangolin.toml");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    w.complete_discovery(id, inventory(&w.target, true));
    assert_eq!(w.discovery_status(), "Failed");
    assert_eq!(w.completed(), 2);
    assert!(w.message.contains("Cannot read"));
}
#[test]
fn pangolin_discovery_mocked_transport_auth_timeout_and_secret_redaction() {
    let mock = Mock::new(Output::stdout(&serde_json::json!({"outcome":"Not found", "facts":{}, "issues":["not_found", "synthetic-secret-never-export"], "environment":"synthetic-secret-never-export"}).to_string()));
    let run = |mock: &Mock, stop: &AtomicBool| {
        pangolin::discover(
            mock,
            "discovery.example.test",
            &[],
            stop,
            &hs_cli::touch::Sounds::disabled(),
            hs_cli::touch::default_player(),
        )
    };
    let report = run(&mock, &AtomicBool::new(false));
    assert_eq!(report.outcome, "Not found");
    assert!(!report.text(0).contains("synthetic-secret"));
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].last().unwrap().starts_with("python3 -B -c '"));
    assert!(!calls[1].iter().any(|s| s == "sudo"));
    drop(calls);
    let failure = Mock::new(Output {
        code: 255,
        stdout: String::new(),
        stderr: "Permission denied synthetic-secret".to_owned(),
    });
    let report = run(&failure, &AtomicBool::new(false));
    assert_eq!(report.outcome, "Failed");
    assert!(report.diagnostic.contains("authentication failed"));
    assert!(!report.diagnostic.contains("synthetic-secret"));
    let mut timeout = Mock::new(Output::stdout(""));
    timeout.error = Some(io::ErrorKind::TimedOut);
    assert_eq!(
        run(&timeout, &AtomicBool::new(false)).diagnostic,
        "Discovery timed out."
    );
    assert_eq!(run(&mock, &AtomicBool::new(true)).outcome, "Cancelled");
}
#[test]
fn pangolin_discovery_malformed_inventory_cannot_claim_success() {
    for text in [
        "not-json synthetic-secret",
        "{}",
        r#"{"outcome":"Discovered","facts":{"identity":false}}"#,
    ] {
        let error = pangolin::Discovery::parse("inventory.example.test", text).unwrap_err();
        assert!(!error.contains("synthetic-secret"));
    }
    let incomplete =
        pangolin::Discovery::parse("inventory.example.test", r#"{"outcome":"Discovered","facts":{}}"#).unwrap();
    assert_eq!(incomplete.outcome, "Partially discovered");
    let mut incomplete = inventory("inventory.example.test", true);
    incomplete.facts.insert("state".to_owned(), "Stopped".to_owned());
    assert_eq!(incomplete.fact("state"), "Stopped");
}
#[test]
fn pangolin_discovery_ui_controls_help_mouse_and_exact_terminal_buffers() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let dir = tempfile::tempdir().unwrap();
    let mut w = discovery_wizard(dir.path());
    w.focus = 1;
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Pangolin(Box::new(w)));
    for (width, height) in [(80, 24), (120, 36), (160, 45)] {
        let text = rendered(&app, width, height);
        for phrase in ["2/8", "Not discovered", "[ Discover ]", "F6 discover", "F4 help"] {
            assert!(text.contains(phrase), "{text}");
        }
        assert!(!text.contains("F6 test"));
    }
    // Test the actual registered mouse control, not a direct state method.
    let text = rendered(&app, 120, 36);
    let (row, column) = text
        .lines()
        .enumerate()
        .find_map(|(r, line)| line.find("[ Discover ]").map(|c| (r, c)))
        .unwrap();
    // Convert UTF-8 byte offset to terminal cells for the Unicode frame.
    let column = text.lines().nth(row).unwrap()[..column].chars().count();
    let request = app.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: column as u16,
        row: row as u16,
        modifiers: KeyModifiers::NONE,
    });
    let Some(Request::DiscoverPangolin(id, target)) = request else {
        panic!("Missing mouse discovery request");
    };
    assert!(app.key(key(KeyCode::F(6))).is_none());
    if let Some(Overlay::Pangolin(w)) = app.overlay.as_mut() {
        w.complete_discovery(id, inventory(&target, false));
    }
    for (width, height) in [(80, 24), (120, 36), (160, 45)] {
        let text = rendered(&app, width, height);
        for phrase in ["3/8", "✓ Discovered", "[ Discover again ]", "[ Detailed report ]"] {
            assert!(text.contains(phrase), "{text}");
        }
        if let Some(path) = std::env::var_os("HOMESTACK_PANGOLIN_RENDER_DIR") {
            let path = std::path::PathBuf::from(path);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join(format!("pangolin-phase3-{width}x{height}.txt")), &text).unwrap();
        }
    }
    // Report is both keyboard-focusable and registered as a mouse control.
    app.key(key(KeyCode::Tab));
    assert!(matches!(&app.overlay, Some(Overlay::Pangolin(w)) if w.focus == 2));
    app.key(key(KeyCode::Enter));
    assert!(matches!(&app.overlay, Some(Overlay::Pangolin(w)) if w.report_open));
    app.key(key(KeyCode::Esc));
    let text = rendered(&app, 120, 36);
    let (row, line) = text
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("[ Detailed report ]"))
        .unwrap();
    let column = line[..line.find("[ Detailed report ]").unwrap()].chars().count();
    app.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: column as u16,
        row: row as u16,
        modifiers: KeyModifiers::NONE,
    });
    assert!(matches!(&app.overlay, Some(Overlay::Pangolin(w)) if w.report_open && w.focus == 2));
    app.key(key(KeyCode::Esc));
    app.key(key(KeyCode::F(7)));
    let mut text = String::new();
    for _ in 0..30 {
        text.push_str(&rendered(&app, 80, 24));
        app.key(key(KeyCode::PageDown));
    }
    assert!(text.contains("historical observation"));
    assert!(text.contains("api publication: Loopback only"));
    app.key(key(KeyCode::Esc));
    app.key(key(KeyCode::F(4)));
    let mut text = String::new();
    for _ in 0..15 {
        text.push_str(&rendered(&app, 80, 24));
        app.key(key(KeyCode::PageDown));
    }
    for phrase in ["read-only", "Docker Compose", "Step 4", "Python 3", "Changing"] {
        assert!(text.contains(phrase), "{phrase}");
    }
    app.key(key(KeyCode::Esc));
    for step in 3..8 {
        if let Some(Overlay::Pangolin(w)) = app.overlay.as_mut() {
            w.select_step(step);
        }
        assert!(app.key(key(KeyCode::F(6))).is_none());
        assert!(app.key(key(KeyCode::F(7))).is_none());
        assert!(rendered(&app, 120, 36).contains("Locked - not implemented."));
    }
}
#[test]
fn pangolin_remote_probe_offline_fixtures() {
    let output = std::process::Command::new("python3")
        .arg("-B")
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/pangolin_probe_test.py"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn pangolin_discovery_confirmation_revocation_survives_reopening_and_worker_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = discovery_wizard(dir.path());
    let id = w.begin_discovery().unwrap();
    w.complete_discovery(id, inventory(&w.target, false));
    w.toggle_prerequisites();
    assert_eq!(w.discovery_status(), "Outdated");
    let mut w = open(dir.path());
    w.toggle_prerequisites();
    assert_eq!(w.discovery_status(), "Outdated");
    assert_eq!(w.completed(), 2);
    w.select_step(2);
    let id = w.begin_discovery().unwrap();
    w.worker_failed(id, "Cannot start the existing SSH askpass bridge.");
    assert!(!w.testing && !w.discovering);
    assert_eq!(w.discovery_status(), "Failed");
    assert_eq!(
        open(dir.path()).discovery.unwrap().diagnostic,
        "Cannot start the existing SSH askpass bridge."
    );
    let id = w.begin_discovery().unwrap();
    w.toggle_prerequisites();
    assert!(!w.testing);
    w.complete_discovery(id, inventory(&w.target, false));
    assert_eq!(w.discovery_status(), "Outdated");
}

#[test]
fn pangolin_discovery_failures_render_compactly_without_unknown_inventory() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    for (outcome, diagnostic, category, action) in [
        (
            "Permission denied",
            "SSH connected, but Docker metadata is not accessible to this account.",
            "Permission denied",
            "Grant appropriate Docker access",
        ),
        (
            "Not found",
            "Pangolin installation not found in accessible supported runtimes. Custom layouts are not searched.",
            "Not found",
            "Check the selected server",
        ),
        (
            "Partially discovered",
            "Pangolin is installed but its service is stopped.",
            "Stopped service",
            "Ask the server administrator",
        ),
        (
            "Unsupported installation",
            "Multiple Pangolin installations detected. Automatic selection is unsafe.",
            "Ambiguous installation",
            "Inspect F7 Details",
        ),
        (
            "Failed",
            "SSH authentication failed; check the configured identity and access.",
            "SSH authentication failed",
            "Check the configured SSH identity",
        ),
    ] {
        let mut w = discovery_wizard(dir.path());
        w.focus = 1;
        let mut report = pangolin::Discovery::failure(&w.target, outcome, diagnostic);
        report.facts.insert("ssh_user".to_owned(), "operator".to_owned());
        if category == "Stopped service" {
            report.facts.insert("installation".to_owned(), "Docker".to_owned());
            report.facts.insert("state".to_owned(), "Stopped".to_owned());
        }
        w.discovery = Some(report);
        w.message = diagnostic.to_owned(); // Worker startup errors can also populate this field.
        app.overlay = Some(Overlay::Pangolin(Box::new(w)));
        for (width, height) in [(80, 24), (120, 36)] {
            let text = rendered(&app, width, height);
            for phrase in [category, "SSH account: operator", "Server: prerequisite-lab", action] {
                assert!(text.contains(phrase), "{text}");
            }
            for absent in [
                "Unknown",
                "Unavailable",
                "Software version:",
                "Config:",
                "API port:",
                "Future bootstrap",
            ] {
                assert!(!text.contains(absent), "{text}");
            }
            assert!(
                text.contains(&format!("✗ {category}")),
                "Missing visible failure status: {text}"
            );
            assert_eq!(text.matches(category).count(), 1, "{text}");
            assert_eq!(text.matches(action).count(), 1, "{text}");
            assert!(text.matches(diagnostic).count() <= 1, "Duplicate diagnostic: {text}");
            if category == "Permission denied"
                && let Some(path) = std::env::var_os("HOMESTACK_PANGOLIN_RENDER_DIR")
            {
                let path = std::path::PathBuf::from(path);
                std::fs::create_dir_all(&path).unwrap();
                std::fs::write(path.join(format!("pangolin-permission-{width}x{height}.txt")), &text).unwrap();
            }
        }
        app.key(key(KeyCode::F(7)));
        let mut detail = String::new();
        for _ in 0..8 {
            detail.push_str(&rendered(&app, 80, 24));
            app.key(key(KeyCode::PageDown));
        }
        assert!(detail.contains("SSH account: operator"));
        assert!(!detail.contains("Unknown"));
        assert!(!detail.contains("api port:"));
        app.key(key(KeyCode::Esc));
    }
}

#[test]
fn pangolin_discovery_resolves_account_from_openssh_including_auth_failures() {
    let output =
        serde_json::json!({"outcome":"Permission denied", "facts":{}, "issues":["docker_permission"]}).to_string();
    let mock = Mock::new(Output::stdout(&output));
    let discover = |mock: &Mock, target: &str| {
        pangolin::discover(
            mock,
            target,
            &[],
            &AtomicBool::new(false),
            &hs_cli::touch::Sounds::disabled(),
            hs_cli::touch::default_player(),
        )
    };
    let report = discover(&mock, "discovery.example.test");
    assert_eq!(report.known("ssh_user"), Some("operator"));
    assert!(report.summary().contains("SSH account: operator"));
    let mock = Mock::new(Output {
        code: 255,
        stdout: String::new(),
        stderr: "Permission denied synthetic-secret".to_owned(),
    });
    let report = discover(&mock, "discovery.example.test");
    assert_eq!(report.known("ssh_user"), Some("operator"));
    assert_eq!(report.problem().unwrap().0, "SSH authentication failed");
    assert!(!report.text(0).contains("synthetic-secret"));
    let mut unresolved = Mock::new(Output::stdout(""));
    unresolved.config.clear();
    let report = discover(&unresolved, "discovery.example.test");
    assert!(report.known("ssh_user").is_none());
    assert!(report.summary().contains("SSH account not resolved"));
    let report = discover(&unresolved, "alternate@discovery.example.test");
    assert!(report.summary().contains("SSH account: alternate"));
}

#[test]
fn pangolin_discovery_details_only_list_observed_inventory() {
    let mut report = inventory("discovery.example.test", false);
    report.facts.insert("ssh_user".to_owned(), "operator".to_owned());
    report
        .facts
        .insert("working_directory".to_owned(), "Unknown".to_owned());
    report.facts.insert("service_file".to_owned(), String::new());
    assert!(report.summary().contains("Installation: Docker Compose"));
    let detail = report.text(0);
    assert!(detail.contains("installation: Docker Compose"));
    assert!(detail.contains("config: /srv/synthetic/config.yml"));
    assert!(!detail.contains("working directory:"));
    assert!(!detail.contains("service file:"));
    assert_eq!(detail.matches("SSH account: operator").count(), 1);
    assert!(!detail.contains("Unknown"));
    report.diagnostic = "SSH connected, but Docker metadata is not accessible to this account.\nSSH connected, but Docker metadata is not accessible to this account.".to_owned();
    assert_eq!(
        report
            .text(0)
            .matches("SSH connected, but Docker metadata is not accessible to this account.")
            .count(),
        1
    );
}

#[test]
fn pangolin_header_reports_overall_progress_without_borrowing_another_steps_status() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = discovery_wizard(dir.path());
    w.discovery = Some(pangolin::Discovery::failure(
        &w.target,
        "Permission denied",
        "SSH connected, but Docker metadata is not accessible to this account.",
    ));
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Pangolin(Box::new(w)));
    for step in 0..8 {
        if let Some(Overlay::Pangolin(w)) = app.overlay.as_mut() {
            w.select_step(step);
        }
        for (width, height) in [(80, 24), (120, 36)] {
            let text = rendered(&app, width, height);
            let header = text.lines().nth(1).unwrap();
            assert!(header.contains("2/8 completed"), "{text}");
            for status in ["Verified", "Permission denied", "Confirmed", "Locked"] {
                assert!(!header.contains(status), "{text}");
            }
            if step >= 3 {
                assert!(text.contains("Locked - not implemented."), "{text}");
                assert!(!text.contains("Verified"), "{text}");
                assert!(!text.contains("Permission denied"), "{text}");
            }
        }
    }
}
