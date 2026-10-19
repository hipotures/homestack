#![allow(clippy::unwrap_used)]
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hs_cli::tui::{
    integrations::Editor,
    state::{App, Overlay},
    theme::Preferences,
};
use std::os::unix::fs::PermissionsExt;

fn press(editor: &mut Editor, code: KeyCode) -> bool {
    editor.key(KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn credentials_save_privately_without_losing_unrelated_entries() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("integrations")).unwrap();
    let path = dir.path().join("integrations/spaceship-dns");
    std::fs::write(
        &path,
        "# Keep this comment\nOTHER=value\nSPACESHIP_API_KEY=old\nSPACESHIP_API_SECRET=secret\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut editor = Editor::open(dir.path(), 1).unwrap();
    assert!(!editor.visible);
    editor.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    editor.paste(&"long-key-".repeat(50));
    press(&mut editor, KeyCode::F(4));
    assert!(editor.visible);
    press(&mut editor, KeyCode::F(5));
    assert_eq!(editor.message, "Saved");
    assert!(!editor.visible);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# Keep this comment\nOTHER=value"));
    assert!(text.contains(&"long-key-".repeat(50)));
    assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    let saved = Editor::open(dir.path(), 1).unwrap();
    assert_eq!(saved.values, editor.values);
    assert!(!format!("{saved:?}").contains("secret"));
}

#[test]
fn bind_tsig_round_trips_and_unknown_syntax_is_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("integrations")).unwrap();
    let path = dir.path().join("integrations/dns-tsig.key");
    std::fs::write(
        &path,
        "// local DNS\nkey \"local.\" { algorithm hmac-sha256; secret \"c2VjcmV0\"; };\n",
    )
    .unwrap();
    let mut editor = Editor::open(dir.path(), 2).unwrap();
    editor.save().unwrap();
    let loaded = Editor::open(dir.path(), 2).unwrap();
    assert_eq!(loaded.values, editor.values);
    std::fs::write(&path, "unknown syntax").unwrap();
    assert!(Editor::open(dir.path(), 2).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "unknown syntax");
}

#[test]
fn validation_and_discard_never_write_partial_credentials() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("integrations")).unwrap();
    let mut editor = Editor::open(dir.path(), 1).unwrap();
    editor.paste("draft");
    press(&mut editor, KeyCode::F(5));
    assert!(editor.message.contains("SPACESHIP_API_SECRET"));
    assert!(!dir.path().join("integrations/spaceship-dns").exists());
    assert!(!press(&mut editor, KeyCode::Esc));
    assert!(press(&mut editor, KeyCode::Esc));
}

#[test]
fn rendered_secrets_are_masked_and_long_keys_use_the_panel_width() {
    use ratatui::{Terminal, backend::TestBackend};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("integrations")).unwrap();
    let mut editor = Editor::open(dir.path(), 1).unwrap();
    editor.paste(&"ABCDEFGH".repeat(30));
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Integration(Box::new(editor)));
    let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
    let render = |terminal: &mut Terminal<TestBackend>, app: &App| {
        terminal.draw(|frame| hs_cli::tui::draw::draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };
    let hidden = render(&mut terminal, &app);
    assert!(hidden.contains("settings"));
    assert!(hidden.contains("Show"));
    assert!(!hidden.contains("ABCDEFGH"));
    assert!(hidden.contains(&"*".repeat(90)));
    app.key(KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE));
    let shown = render(&mut terminal, &app);
    assert!(shown.contains("Hide"));
    assert!(shown.contains("ABCDEFGH"));
}

#[test]
fn tsig_test_requires_a_valid_signature_and_authoritative_answer() {
    use hs_cli::tui::integrations::verify_tsig_response;
    let answer = ";; ->>HEADER<<- opcode: QUERY, status: NOERROR, id: 1\n;; flags: qr aa; QUERY: 1, ANSWER: 1\nexample.test. 60 IN SOA ns.example.test. admin.example.test. 1 2 3 4 5\n;; TSIG PSEUDOSECTION:\nkey. 0 ANY TSIG hmac-sha256. 1 300 32 signature 1 NOERROR 0\n";
    assert!(verify_tsig_response(true, answer, "").is_ok());
    for bad in [
        answer.replace("TSIG PSEUDOSECTION", "UNSIGNED"),
        answer.replace("qr aa;", "qr rd;"),
        answer.replace(" SOA ", " TXT "),
        answer.replace("status: NOERROR", "status: REFUSED"),
        answer.replace("signature", "BADSIG"),
        format!("{answer};; Couldn't verify signature: tsig verify failure\n"),
    ] {
        assert!(verify_tsig_response(true, &bad, "").is_err());
    }
    assert!(verify_tsig_response(true, answer, "Couldn't verify signature").is_err());
    assert!(verify_tsig_response(false, answer, "").is_err());
}

#[test]
fn test_targets_round_trip_without_entering_credentials_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("integrations")).unwrap();
    let mut editor = Editor::open(dir.path(), 1).unwrap();
    editor.values = vec!["key".to_owned(), "secret".to_owned(), "example.test".to_owned()];
    editor.save().unwrap();
    let loaded = Editor::open(dir.path(), 1).unwrap();
    assert_eq!(loaded.zone(), "example.test");
    let credentials = std::fs::read_to_string(dir.path().join("integrations/spaceship-dns")).unwrap();
    assert!(!credentials.contains("example.test"));
    let preferences = std::fs::read_to_string(dir.path().join("integrations/settings.toml")).unwrap();
    assert!(preferences.contains("example.test"));
    assert!(!preferences.contains("secret"));
}

#[test]
fn testing_uses_drafts_and_ignores_results_after_edits_or_reopening() {
    use hs_cli::tui::state::Request;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("integrations")).unwrap();
    let mut editor = Editor::open(dir.path(), 1).unwrap();
    editor.values = vec![
        "draft-key".to_owned(),
        "draft-secret".to_owned(),
        "example.test".to_owned(),
    ];
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Integration(Box::new(editor)));
    let request = app.key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE)).unwrap();
    let Request::TestIntegration(id, draft) = request else {
        panic!("Expected connection test");
    };
    assert_eq!(draft.values[0], "draft-key");
    assert!(!dir.path().join("integrations/spaceship-dns").exists());
    assert!(app.key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE)).is_none());
    app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    app.integration_test_result(
        id,
        hs_cli::tui::integrations::TestReport::completed(&draft, Ok("Stale result".to_owned()), 10),
    );
    let Some(Overlay::Integration(editor)) = app.overlay.as_ref() else {
        panic!("Expected editor");
    };
    assert!(!editor.message.contains("Stale"));
    app.overlay = Some(Overlay::Integration(Box::new(Editor::open(dir.path(), 1).unwrap())));
    app.integration_test_result(
        id,
        hs_cli::tui::integrations::TestReport::completed(&draft, Ok("Stale result".to_owned()), 10),
    );
    let Some(Overlay::Integration(editor)) = app.overlay.as_ref() else {
        panic!("Expected editor");
    };
    assert!(!editor.message.contains("Stale"));
}

#[tokio::test]
async fn invalid_targets_fail_before_network_access() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("integrations")).unwrap();
    let mut editor = Editor::open(dir.path(), 1).unwrap();
    editor.values = vec!["key".to_owned(), "secret".to_owned(), "../invalid".to_owned()];
    assert!(editor.begin_test().is_none());
    assert!(editor.test_connection().await.is_err());
}

#[tokio::test]
async fn ovh_zone_check_authenticates_a_read_request_and_rejects_denied_access() {
    use axum::{
        Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::get,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let denied = Arc::new(AtomicBool::new(false));
    let router = Router::new()
        .route("/auth/time", get(|| async { axum::Json(1_700_000_000_i64) }))
        .route(
            "/domain/zone/example.test",
            get(|State(denied): State<Arc<AtomicBool>>, headers: HeaderMap| async move {
                assert_eq!(headers["X-Ovh-Application"], "test-ak");
                assert_eq!(headers["X-Ovh-Consumer"], "test-ck");
                assert!(headers["X-Ovh-Signature"].to_str().unwrap().starts_with("$1$"));
                if denied.load(Ordering::Relaxed) {
                    StatusCode::FORBIDDEN.into_response()
                } else {
                    axum::Json(serde_json::json!({"zone": "example.test"})).into_response()
                }
            }),
        )
        .with_state(Arc::clone(&denied));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = hs_deploy::ovh::OvhClient::connect(hs_deploy::ovh::OvhCredentials {
        application_key: "test-ak".to_owned(),
        application_secret: "test-as".to_owned(),
        consumer_key: "test-ck".to_owned(),
        endpoint: format!("http://{address}"),
    })
    .await
    .unwrap();
    assert!(client.check_zone("example.test").await.is_ok());
    denied.store(true, Ordering::Relaxed);
    assert!(client.check_zone("example.test").await.is_err());
    server.abort();
}

#[test]
fn enter_after_zone_focuses_test_and_enter_activates_it() {
    use hs_cli::tui::{draw::render, state::Request};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("integrations")).unwrap();
    let mut editor = Editor::open(dir.path(), 1).unwrap();
    editor.values = vec!["key".to_owned(), "secret".to_owned(), "example.test".to_owned()];
    editor.select(2);
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Integration(Box::new(editor)));
    assert!(app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).is_none());
    for (width, height) in [(80, 24), (120, 36)] {
        let buffer = render(&app, width, height);
        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("▶ [ Test ]"));
        assert!(text.contains("example.test"));
    }
    let Some(Overlay::Integration(editor)) = app.overlay.as_mut() else {
        panic!("Expected editor");
    };
    assert!(editor.on_test_button());
    editor.paste("ignored");
    assert_eq!(editor.zone(), "example.test");
    editor.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
    assert_eq!(editor.focus, 2);
    editor.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let request = app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(request, Some(Request::TestIntegration(_, _))));
    assert!(app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).is_none());
}

#[test]
fn detailed_report_persists_target_date_failure_and_redacts_credentials() {
    use hs_cli::tui::{draw::render, integrations::TestReport};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("integrations")).unwrap();
    let mut editor = Editor::open(dir.path(), 0).unwrap();
    editor.values = vec![
        "application-key".to_owned(),
        "application-secret".to_owned(),
        "consumer-key".to_owned(),
        "ovh-eu".to_owned(),
        "example.test".to_owned(),
    ];
    let id = editor.begin_test().unwrap();
    let report = TestReport::completed(&editor, Err("OVH GET /domain/zone/example.test: 403 Forbidden: application-key application-secret consumer-key: This call has not been granted".to_owned()), 234);
    assert!(!report.passed);
    assert!(!report.at.is_empty());
    assert!(report.detail.contains("403 Forbidden"));
    for secret in &editor.values[..3] {
        assert!(!report.detail.contains(secret));
    }
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Integration(Box::new(editor)));
    app.integration_test_result(id, report.clone());
    let Some(Overlay::Integration(editor)) = app.overlay.as_ref() else {
        panic!("Expected editor");
    };
    assert!(editor.report_open);
    let buffer = render(&app, 100, 36);
    let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
    for expected in [
        "FAIL",
        "403 Forbidden",
        "Date (UTC)",
        "example.test",
        "ovh-eu",
        "234 ms",
    ] {
        assert!(text.contains(expected), "Missing {expected}");
    }
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let Some(Overlay::Integration(editor)) = app.overlay.as_ref() else {
        panic!("Expected editor");
    };
    assert!(!editor.report_open);
    let mut loaded = Editor::open(dir.path(), 0).unwrap();
    assert_eq!(loaded.report, Some(report.clone()));
    loaded.values = editor.values.clone();
    loaded.save().unwrap();
    assert_eq!(Editor::open(dir.path(), 0).unwrap().report, Some(report));
    let saved = std::fs::read_to_string(dir.path().join("integrations/settings.toml")).unwrap();
    for secret in &editor.values[..3] {
        assert!(!saved.contains(secret));
    }
}

#[test]
fn integration_list_shows_saved_test_outcomes_dates_and_zones() {
    use hs_cli::tui::{draw::render, integrations::TestReport};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("integrations")).unwrap();
    let preferences = Preferences::fallback(&dir.path().join("tui.toml"));
    for (provider, passed, zone) in [(0, false, "ovh.test"), (1, true, "spaceship.test")] {
        TestReport {
            provider,
            passed,
            zone: zone.to_owned(),
            at: "2026-10-19T12:34:56Z".to_owned(),
            target: "test endpoint".to_owned(),
            duration_ms: 100,
            detail: "Test result".to_owned(),
        }
        .save(dir.path())
        .unwrap();
    }
    let mut app = App::new(preferences, false).unwrap();
    app.overlay = Some(Overlay::Integrations { index: 0 });
    for (width, height) in [(80, 24), (120, 36)] {
        let buffer = render(&app, width, height);
        let rows: Vec<String> = (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let ovh = rows.iter().position(|row| row.contains("OVH")).unwrap();
        let spaceship = rows.iter().position(|row| row.contains("Spaceship")).unwrap();
        assert!(rows[ovh].contains("FAIL  2026-10-19 12:34 UTC"));
        assert!(rows[spaceship].contains("PASS  2026-10-19 12:34 UTC"));
        let fail_x = rows[ovh].chars().position(|c| c == 'F').unwrap();
        assert_eq!(
            buffer[(u16::try_from(fail_x).unwrap(), u16::try_from(ovh).unwrap())].fg,
            app.palette.error
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("Local DNS (TSIG)") && row.contains("Missing"))
        );
        if width == 120 {
            assert!(rows[ovh].contains("ovh.test"));
            assert!(rows[spaceship].contains("spaceship.test"));
        }
    }
}

#[test]
fn testing_remembers_zone_before_completion_without_saving_draft_credentials() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("integrations")).unwrap();
    let credentials = "SPACESHIP_API_KEY=original-key\nSPACESHIP_API_SECRET=original-secret\n";
    std::fs::write(dir.path().join("integrations/spaceship-dns"), credentials).unwrap();
    let mut editor = Editor::open(dir.path(), 1).unwrap();
    editor.values[0] = "draft-key".to_owned();
    editor.select(2);
    editor.paste("example.test");
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Integration(Box::new(editor)));
    assert!(matches!(
        app.key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE)),
        Some(hs_cli::tui::state::Request::TestIntegration(_, _))
    ));
    app.preferences.save_theme("Nord").unwrap();
    let reloaded = Editor::open(dir.path(), 1).unwrap();
    assert_eq!(reloaded.zone(), "example.test");
    assert_eq!(reloaded.values[0], "original-key");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("integrations/spaceship-dns")).unwrap(),
        credentials
    );
}

#[test]
fn leaving_form_remembers_local_dns_target_even_without_valid_credentials() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("integrations")).unwrap();
    let mut editor = Editor::open(dir.path(), 2).unwrap();
    editor.select(3);
    editor.paste("example.test");
    editor.select(4);
    editor.paste("192.0.2.53");
    assert!(press(&mut editor, KeyCode::Esc));
    let reloaded = Editor::open(dir.path(), 2).unwrap();
    assert_eq!(reloaded.zone(), "example.test");
    assert_eq!(reloaded.values[4], "192.0.2.53");
    assert!(!dir.path().join("integrations/dns-tsig.key").exists());
}

#[test]
fn passing_test_saves_credentials_and_disables_save_until_another_edit() {
    use hs_cli::tui::{draw::render, integrations::TestReport};
    let dir = tempfile::tempdir().unwrap();
    let mut editor = Editor::open(dir.path(), 1).unwrap();
    editor.paste("verified-key");
    editor.select(1);
    editor.paste("verified-secret");
    editor.select(2);
    editor.paste("example.test");
    let id = editor.begin_test().unwrap();
    assert!(editor.has_changes());
    let report = TestReport::completed(&editor, Ok("Credentials accepted".to_owned()), 100);
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Integration(Box::new(editor)));
    app.integration_test_result(id, report);
    let loaded = Editor::open(dir.path(), 1).unwrap();
    assert_eq!(loaded.values[0], "verified-key");
    assert_eq!(loaded.values[1], "verified-secret");
    assert_eq!(loaded.zone(), "example.test");
    assert_eq!(
        std::fs::metadata(&loaded.path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let Some(Overlay::Integration(editor)) = app.overlay.as_ref() else {
        panic!("Expected editor");
    };
    assert!(!editor.has_changes());
    assert!(editor.message.is_empty());
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let save_colour = |app: &App| {
        let buffer = render(app, 120, 36);
        for y in 0..36 {
            let row: String = (0..120).map(|x| buffer[(x, y)].symbol()).collect();
            if let Some(byte) = row.find("F5 Save") {
                let x = u16::try_from(row[..byte].chars().count()).unwrap();
                return buffer[(x, y)].fg;
            }
        }
        panic!("Save control missing");
    };
    assert_eq!(save_colour(&app), app.palette.muted);
    app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert_eq!(save_colour(&app), app.palette.hotkey);
    let Some(Overlay::Integration(editor)) = app.overlay.as_ref() else {
        panic!("Expected editor");
    };
    assert!(editor.has_changes());
}

#[test]
fn failed_and_stale_tests_do_not_save_draft_credentials() {
    use hs_cli::tui::integrations::TestReport;
    for stale in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut editor = Editor::open(dir.path(), 1).unwrap();
        editor.paste("draft-key");
        editor.select(1);
        editor.paste("draft-secret");
        editor.select(2);
        editor.paste("example.test");
        let id = editor.begin_test().unwrap();
        let report = TestReport::completed(
            &editor,
            if stale {
                Ok("Accepted".to_owned())
            } else {
                Err("HTTP 403 Forbidden".to_owned())
            },
            100,
        );
        if stale {
            editor.paste(".changed");
        }
        let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
        app.overlay = Some(Overlay::Integration(Box::new(editor)));
        app.integration_test_result(id, report);
        assert!(!dir.path().join("integrations/spaceship-dns").exists());
        let Some(Overlay::Integration(editor)) = app.overlay.as_ref() else {
            panic!("Expected editor");
        };
        assert!(editor.has_changes());
        assert_eq!(editor.report_open, !stale);
    }
}

#[test]
fn passing_test_reports_save_failure_and_keeps_changes_pending() {
    use hs_cli::tui::integrations::TestReport;
    let dir = tempfile::tempdir().unwrap();
    let mut editor = Editor::open(dir.path(), 1).unwrap();
    editor.paste("draft-key");
    editor.select(1);
    editor.paste("draft-secret");
    editor.select(2);
    editor.paste("example.test");
    let id = editor.begin_test().unwrap();
    std::fs::create_dir(&editor.path).unwrap();
    let report = TestReport::completed(&editor, Ok("Accepted".to_owned()), 100);
    let mut app = App::new(Preferences::fallback(&dir.path().join("tui.toml")), false).unwrap();
    app.overlay = Some(Overlay::Integration(Box::new(editor)));
    app.integration_test_result(id, report);
    let Some(Overlay::Integration(editor)) = app.overlay.as_ref() else {
        panic!("Expected editor");
    };
    assert!(editor.has_changes());
    assert!(editor.message.contains("automatic save failed"));
    assert!(editor.report_open);
    assert!(!editor.message.contains("draft-secret"));
}
