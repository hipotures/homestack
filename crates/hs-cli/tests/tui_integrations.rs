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
    let path = dir.path().join("spaceship-dns");
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
    let path = dir.path().join("dns-tsig.key");
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
    let mut editor = Editor::open(dir.path(), 1).unwrap();
    editor.paste("draft");
    press(&mut editor, KeyCode::F(5));
    assert!(editor.message.contains("SPACESHIP_API_SECRET"));
    assert!(!dir.path().join("spaceship-dns").exists());
    assert!(!press(&mut editor, KeyCode::Esc));
    assert!(press(&mut editor, KeyCode::Esc));
}

#[test]
fn rendered_secrets_are_masked_and_long_keys_use_the_panel_width() {
    use ratatui::{Terminal, backend::TestBackend};
    let dir = tempfile::tempdir().unwrap();
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
