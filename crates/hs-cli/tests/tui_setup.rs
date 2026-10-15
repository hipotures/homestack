//! The setup box of the TUI (phase 13b, step 5): Python `test_setup_tui.py`,
//! `test_setup_structured_ui.py` and the TUI tests of the setup groups. The
//! box is one section per group (Tab walks them) instead of one tree with a
//! root; the worker lends the terminal instead of Textual's `suspend`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod setup_fixture;
mod tui_fixture;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use hs_cli::AppError;
use hs_cli::setup::Settings;
use hs_cli::setup::catalog::{Catalog, Github, load_catalog, select_entries};
use hs_cli::setup::definitions::{
    Application, ConfigFile, Entry, Params, config_entries, config_entry_id, parse_setup,
};
use hs_cli::setup::engine::Observer;
use hs_cli::setup::herdr::AUTH_NOTICE;
use hs_cli::setup::plan::Plan;
use hs_cli::setup::workspace::Workspace;
use hs_cli::system::Output;
use hs_cli::touch::Sounds;
use hs_cli::tui::draw::render;
use hs_cli::tui::setup::{Ask, SetupBox, Stage, Tone, clean_line};
use hs_cli::tui::setup_work::{Event, Tell, Watcher, startup, with_terminal};
use hs_cli::tui::state::{App, Request};
use ratatui::buffer::Buffer;
use serde_json::{Value, json};
use setup_fixture::{settings, target};
use tui_fixture::{position, screen};

// ---- fixtures ----

struct Boxed {
    _home: tempfile::TempDir,
    setup: SetupBox,
}

fn with_settings(settings_of: impl FnOnce(&Path) -> Settings, state: Option<Value>) -> Boxed {
    let home = tempfile::tempdir().unwrap();
    let cfg = settings_of(home.path());
    let catalog = load_catalog(&cfg, None);
    let state = state.map(|mut state| {
        if state.get("state_path").is_none() {
            state["state_path"] = json!("~/.local/state/homestack/setup.json");
        }
        state
    });
    Boxed {
        setup: SetupBox::new(cfg, target(), catalog, state),
        _home: home,
    }
}

/// Python `SetupApp(test_config(), TARGET)`.
fn default_box() -> Boxed {
    with_settings(settings, None)
}

fn state_box(state: Value) -> Boxed {
    with_settings(settings, Some(state))
}

fn example_settings(home: &Path) -> Settings {
    example_settings_from(home, |text| text)
}

fn example_settings_from(home: &Path, edit: impl FnOnce(String) -> String) -> Settings {
    let text =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../python/config.example.toml"))
            .unwrap();
    let legacy = hs_config::legacy::read(&edit(text)).unwrap();
    let mut cfg = settings(home);
    cfg.setup = parse_setup(legacy.client.setup.as_ref().unwrap()).unwrap();
    cfg
}

/// Python `runtime_example_config`: newer Codex values through TOML.
fn runtime_example_settings(home: &Path) -> Settings {
    example_settings_from(home, |text| {
        let marker = "sandbox_mode = \"danger-full-access\"\n\n";
        let additions = format!(
            "{marker}[setup.items.config_files.values.agents]\nenabled = true\nmax_concurrent_threads_per_session = 12\n\n"
        );
        text.replacen(marker, &additions, 1)
    })
}

/// Python `fixture_config`: Codex with one declared document.
fn fixture_settings(home: &Path) -> Settings {
    let values: toml::Table = toml::from_str(
        r#"
"root.key" = "literal"
[tui]
status_line = ["model", "current-dir", "git-branch"]
status_line_use_colors = true
[features]
multi_agent = true
[features.context_management]
experimental_mode = true
"#,
    )
    .unwrap();
    let mut cfg = settings(home);
    for entry in &mut cfg.setup.items {
        if entry.id == "codex"
            && let Params::Application(application) = &mut entry.params
        {
            application.config_files = vec![ConfigFile {
                id: "config".to_owned(),
                path: "~/.fixture/config.toml".to_owned(),
                format: "toml".to_owned(),
                values: values.clone(),
            }];
            application.validation = None;
        }
    }
    cfg
}

/// Python `config_with_file`: one more file item.
fn settings_with_file(home: &Path, path: &str) -> Settings {
    let mut cfg = settings(home);
    let raw: toml::Table = toml::from_str(&format!(
        "[[items]]\nid = \"fixture-file\"\ngroup = \"files\"\nhandler = \"file\"\nlabel = \"Fixture file\"\ndescription = \"Fixture file\"\npath = \"{path}\"\n"
    ))
    .unwrap();
    cfg.setup = parse_setup(&raw).unwrap();
    cfg
}

fn leaf(key: &[&str]) -> String {
    config_entry_id(
        "codex",
        "config",
        &key.iter().map(|k| (*k).to_owned()).collect::<Vec<_>>(),
    )
}

fn key(setup: &mut SetupBox, code: KeyCode) -> Option<Ask> {
    setup.key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn typed(setup: &mut SetupBox, text: &str) {
    for c in text.chars() {
        key(setup, KeyCode::Char(c));
    }
}

fn set_filter(setup: &mut SetupBox, text: &str) {
    key(setup, KeyCode::Char('/'));
    while !setup.filter.is_empty() {
        key(setup, KeyCode::Backspace);
    }
    typed(setup, text);
    key(setup, KeyCode::Tab);
}

fn label(setup: &SetupBox, id: &str) -> String {
    setup.nodes[id].plain()
}

fn entry<'a>(setup: &'a SetupBox, id: &str) -> &'a Entry {
    setup.catalog.entries.iter().find(|e| e.id == id).unwrap()
}

fn selected(setup: &SetupBox) -> Vec<String> {
    setup.selected.iter().cloned().collect()
}

fn set_of(ids: &[&str]) -> Vec<String> {
    let mut ids: Vec<String> = ids.iter().map(|id| (*id).to_owned()).collect();
    ids.sort();
    ids.dedup();
    ids
}

fn section(setup: &SetupBox, group: &str) -> usize {
    setup.settings.setup.groups.iter().position(|g| g.id == group).unwrap()
}

fn review_text(setup: &SetupBox) -> String {
    match &setup.stage {
        Stage::Review { lines, .. } => lines.join("\n"),
        other => panic!("not in review: {other:?}"),
    }
}

fn run_lines(setup: &SetupBox) -> Vec<String> {
    match &setup.stage {
        Stage::Running { lines, .. } => lines.clone(),
        other => panic!("not running: {other:?}"),
    }
}

/// The TUI with the box open on its workspaces panel.
fn tui(setup: SetupBox) -> (tui_fixture::Fixture, ()) {
    let mut fixture = tui_fixture::app();
    fixture.app.setup = Some(setup);
    (fixture, ())
}

fn open(app: &App) -> &SetupBox {
    app.setup.as_ref().unwrap()
}

fn open_mut(app: &mut App) -> &mut SetupBox {
    app.setup.as_mut().unwrap()
}

fn mouse(app: &mut App, kind: MouseEventKind, (column, row): (u16, u16)) -> Option<Request> {
    app.mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn click(app: &mut App, at: (u16, u16)) -> Option<Request> {
    mouse(app, MouseEventKind::Down(MouseButton::Left), at)
}

fn right_click(app: &mut App, at: (u16, u16)) -> Option<Request> {
    mouse(app, MouseEventKind::Down(MouseButton::Right), at)
}

/// The cell of the checkbox and of the name of the row whose label starts
/// with `text`.
fn row_parts(buf: &Buffer, text: &str) -> ((u16, u16), (u16, u16)) {
    let (x, y) = position(buf, text);
    ((x + 1, y), (x + 8, y))
}

fn app_key(app: &mut App, code: KeyCode) -> Option<Request> {
    app.key(KeyEvent::new(code, KeyModifiers::NONE))
}

// ---- SetupTUITests ----

/// Port of `test_keyboard_parent_states_filter_scope_review_back_and_cancel`.
#[test]
fn keyboard_parent_states_filter_scope_review_back_and_cancel() {
    let Boxed { mut setup, _home } = default_box();
    assert!(setup.selected.is_empty());
    assert!(!setup.target_label("").contains("Hidden selected:"));
    setup.show_section(section(&setup, "env"));
    setup.highlight("bash");
    key(&mut setup, KeyCode::Char(' '));
    assert_eq!(selected(&setup), set_of(&["bash"]));
    assert_eq!(setup.checkbox("env"), "[-]");
    setup.highlight("env");
    key(&mut setup, KeyCode::Char('0'));
    assert_eq!(setup.checkbox("env"), "[x]");
    key(&mut setup, KeyCode::Char(' '));
    assert!(setup.selected.is_empty());
    set_filter(&mut setup, "bash");
    setup.highlight("env");
    key(&mut setup, KeyCode::Char(' '));
    assert_eq!(selected(&setup), set_of(&["bash"]));
    set_filter(&mut setup, "codex");
    setup.show_section(section(&setup, "app"));
    setup.highlight("codex");
    key(&mut setup, KeyCode::Char(' '));
    let mut expected = setup.descendant_ids("codex", false);
    expected.push("bash".to_owned());
    expected.sort();
    assert_eq!(selected(&setup), expected);
    assert!(setup.target_label("").contains("Hidden selected: 1"));
    setup.review();
    assert!(review_text(&setup).contains("[hidden by filter]"));
    key(&mut setup, KeyCode::Esc);
    assert!(matches!(setup.stage, Stage::Tree));
    assert_eq!(selected(&setup), expected);
    assert!(!setup.busy);
    key(&mut setup, KeyCode::Char('/'));
    key(&mut setup, KeyCode::Esc);
    assert!(setup.filter.is_empty());
    setup.show_section(section(&setup, "env"));
    setup.highlight("bash");
    key(&mut setup, KeyCode::Left);
    assert_eq!(setup.cursor, "env");
    key(&mut setup, KeyCode::Enter);
    assert!(matches!(setup.stage, Stage::Review { .. }));
    key(&mut setup, KeyCode::Esc);
    assert!(!setup.busy);
    assert_eq!(key(&mut setup, KeyCode::Esc), Some(Ask::Close));
}

/// Port of `test_refresh_preserves_removed_selected_identity_and_allows_deselection`.
#[test]
fn refresh_preserves_removed_selected_identity_and_allows_deselection() {
    let Boxed { mut setup, _home } = default_box();
    setup.selected.insert("bash".to_owned());
    let mut refreshed = load_catalog(&setup.settings, None);
    refreshed.entries.retain(|e| e.id != "bash");
    setup.replace_catalog(refreshed, None);
    assert!(setup.selected.contains("bash"));
    assert_eq!(setup.catalog.availability["bash"], "removed");
    setup.toggle("bash", false);
    assert!(!setup.selected.contains("bash"));
}

/// Port of `test_unavailable_files_visible_dont_disable_apps_and_no_execution_on_enter`.
#[test]
fn unavailable_files_visible_dont_disable_apps_and_no_execution_on_enter() {
    let Boxed { mut setup, _home } = with_settings(|home| settings_with_file(home, "~/missing-setup-fixture"), None);
    let file = setup
        .catalog
        .entries
        .iter()
        .find(|e| e.handler == "file")
        .unwrap()
        .id
        .clone();
    assert!(setup.nodes.contains_key(&file));
    setup.toggle(&file, false);
    assert!(!setup.selected.contains(&file));
    setup.toggle("codex", false);
    assert!(setup.selected.contains("codex"));
    setup.show_section(section(&setup, "app"));
    setup.highlight("codex");
    assert_eq!(key(&mut setup, KeyCode::Enter), None);
    assert!(!setup.busy);
    assert!(matches!(setup.stage, Stage::Review { .. }));
}

/// A TUI that records what happens to the terminal it lends.
fn lending(log: &Arc<Mutex<Vec<String>>>) -> Tell {
    let log = Arc::clone(log);
    Arc::new(move |event| {
        if let Event::Terminal(handoff) = event {
            let log = Arc::clone(&log);
            std::thread::spawn(move || {
                for line in &handoff.banner {
                    log.lock().unwrap().push(format!("banner {line}"));
                }
                log.lock().unwrap().push("suspend".to_owned());
                handoff.go.send(()).unwrap();
                let _ = handoff.done.recv();
                log.lock().unwrap().push("restore".to_owned());
            });
        }
    })
}

/// Port of `test_suspend_restored_even_if_body_raises`: the screen comes
/// back after an operation that fails.
#[test]
fn suspend_restored_even_if_body_raises() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let tell = lending(&log);
    let result = with_terminal(&tell, Vec::new(), || -> Result<(), AppError> {
        Err(AppError::new("Cancelled; remaining actions were not run"))
    })
    .unwrap();
    assert!(result.unwrap_err().0.contains("Cancelled"));
    for _ in 0..100 {
        if log.lock().unwrap().len() == 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(*log.lock().unwrap(), ["suspend", "restore"]);
}

/// Port of `test_worker_terminal_handoff_runs_driver_on_main_thread`: the
/// worker runs only once the TUI has left the screen, which it takes back
/// only when the worker is done.
#[test]
fn worker_terminal_handoff_runs_driver_on_main_thread() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let tell = lending(&log);
    let inner = Arc::clone(&log);
    let value = with_terminal(&tell, vec!["SSH: authenticate once".to_owned()], move || {
        inner.lock().unwrap().push("operation".to_owned());
        7
    });
    assert_eq!(value, Some(7));
    for _ in 0..100 {
        if log.lock().unwrap().len() == 4 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        *log.lock().unwrap(),
        ["banner SSH: authenticate once", "suspend", "operation", "restore"]
    );
    // A TUI that is gone lends nothing.
    let gone: Tell = Arc::new(|_| {});
    assert_eq!(with_terminal(&gone, Vec::new(), || 1), None);
}

// ---- DesktopConnectionTests ----

/// Port of `test_desktop_authentication_keeps_modal_visible_until_success`:
/// the box shows the touch while `herdr machine add` has the terminal.
#[test]
fn desktop_authentication_keeps_modal_visible_until_success() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut watcher = Watcher {
        tell: lending(&log),
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        sounds: Sounds::disabled(),
    };
    let result = watcher.register_desktop(&["true".to_owned()]).unwrap();
    assert_eq!(result.code, 0);
    let Boxed { mut setup, _home } = default_box();
    setup.selected.insert("herdr".to_owned());
    setup.review();
    key(&mut setup, KeyCode::Char('a'));
    setup.activity(Some("herdr"), AUTH_NOTICE);
    assert!(matches!(setup.stage, Stage::Running { auth_wait: true, .. }));
    let (mut fixture, _) = tui(setup);
    let buf = render(&fixture.app, 120, 36);
    assert!(screen(&buf).contains("Touch your YubiKey/security key if it flashes."));
    open_mut(&mut fixture.app).finished(
        json!({"ok": true, "results": [{"id": "herdr", "label": "Herdr", "status": "succeeded", "detail": "Desktop connected"}], "snapshot": {"created": false}}),
        None,
    );
    assert!(matches!(
        open(&fixture.app).stage,
        Stage::Running {
            finished: true,
            auth_wait: false,
            ..
        }
    ));
}

/// Port of `test_escape_cancels_local_registration_without_suspending_tui`.
#[test]
fn escape_cancels_local_registration_without_suspending_tui() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut watcher = Watcher {
        tell: lending(&log),
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        sounds: Sounds::disabled(),
    };
    let error = watcher.register_desktop(&["true".to_owned()]).unwrap_err();
    assert!(
        error.0.contains("Cancelled before desktop Herdr registration"),
        "{}",
        error.0
    );
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(log.lock().unwrap().is_empty());
}

// ---- SetupInteractionTests ----

/// Port of `test_mouse_checkbox_label_arrow_and_right_click_are_distinct`.
#[test]
fn mouse_checkbox_label_arrow_and_right_click_are_distinct() {
    let Boxed { mut setup, _home } = with_settings(example_settings, None);
    setup.show_section(section(&setup, "env"));
    let (mut fixture, _keep) = tui(setup);
    let app = &mut fixture.app;
    let buf = render(app, 140, 42);
    let (checkbox, _) = row_parts(&buf, "[ ] 1. Bash");
    click(app, checkbox);
    assert_eq!(selected(open(app)), set_of(&["bash"]));
    assert_eq!(open(app).cursor, "bash");
    assert_eq!(open(app).checkbox("env"), "[-]");
    let buf = render(app, 140, 42);
    let (_, name) = row_parts(&buf, "[ ] 2. Zsh");
    click(app, name);
    assert_eq!(selected(open(app)), set_of(&["bash"]));
    assert_eq!(open(app).cursor, "zsh");
    let buf = render(app, 140, 42);
    assert!(screen(&buf).contains("Zsh"));
    let (checkbox, _) = row_parts(&buf, "[x] 1. Bash");
    right_click(app, checkbox);
    assert_eq!(selected(open(app)), set_of(&["bash"]));
    click(app, checkbox);
    assert!(open(app).selected.is_empty());
    let buf = render(app, 140, 42);
    let (x, y) = position(&buf, "[ ] Environment");
    click(app, (x + 1, y));
    assert_eq!(selected(open(app)), set_of(&["bash", "fish", "nu", "zsh"]));
    // The arrow of an application with configuration opens and closes it,
    // without touching the selection.
    let section_app = section(open(app), "app");
    open_mut(app).show_section(section_app);
    let buf = render(app, 140, 42);
    let (x, y) = position(&buf, "▸ [ ] 1. Codex");
    click(app, (x, y));
    assert!(open(app).rows().iter().any(|row| row.id == "codex:config"));
    let buf = render(app, 140, 42);
    let (x, y) = position(&buf, "▾ [ ] 1. Codex");
    click(app, (x, y));
    assert!(!open(app).rows().iter().any(|row| row.id == "codex:config"));
    assert_eq!(selected(open(app)), set_of(&["bash", "fish", "nu", "zsh"]));
    assert!(!open(app).busy);
}

/// Port of `test_mouse_respects_filter_unavailable_items_and_busy_guard`.
#[test]
fn mouse_respects_filter_unavailable_items_and_busy_guard() {
    let Boxed { mut setup, _home } = with_settings(|home| settings_with_file(home, "~/missing-setup-fixture"), None);
    setup.show_section(section(&setup, "files"));
    let (mut fixture, _keep) = tui(setup);
    let app = &mut fixture.app;
    let buf = render(app, 120, 40);
    let (x, y) = position(&buf, "Fixture file");
    click(app, (x.saturating_sub(7), y));
    assert!(open(app).selected.is_empty());
    open_mut(app).selected.insert("codex".to_owned());
    set_filter(open_mut(app), "bash");
    let env = section(open(app), "env");
    open_mut(app).show_section(env);
    let buf = render(app, 120, 40);
    let (x, y) = position(&buf, "Environment  ");
    click(app, (x.saturating_sub(4), y));
    assert_eq!(selected(open(app)), set_of(&["bash", "codex"]));
    assert!(open(app).target_label("").contains("Hidden selected: 1"));
    open_mut(app).busy = true;
    let buf = render(app, 120, 40);
    let (checkbox, _) = row_parts(&buf, "[x] 1. Bash");
    click(app, checkbox);
    app_key(app, KeyCode::Char(' '));
    assert_eq!(selected(open(app)), set_of(&["bash", "codex"]));
    open_mut(app).busy = false;
    let buf = render(app, 120, 40);
    let (x, y) = position(&buf, "Environment  ");
    click(app, (x.saturating_sub(4), y));
    assert_eq!(selected(open(app)), set_of(&["codex"]));
}

/// Port of `test_details_tab_page_keys_and_wheel_do_not_change_selection`.
#[test]
fn details_tab_page_keys_and_wheel_do_not_change_selection() {
    let Boxed { mut setup, _home } = with_settings(|home| settings_with_file(home, "~/short-fixture"), None);
    let short = setup
        .catalog
        .entries
        .iter()
        .find(|e| e.handler == "file")
        .unwrap()
        .id
        .clone();
    let long: Vec<String> = (0..100).map(|i| format!("Detail line {i}")).collect();
    for entry in &mut setup.catalog.entries {
        if entry.id == "bash" {
            entry.description = long.join("\n");
        }
    }
    setup.rebuild();
    setup.show_section(section(&setup, "env"));
    setup.highlight("bash");
    let (mut fixture, _keep) = tui(setup);
    let app = &mut fixture.app;
    let _ = render(app, 120, 32);
    app_key(app, KeyCode::PageDown);
    assert!(open(app).details_scroll > 0);
    assert_eq!(open(app).cursor, "bash");
    assert!(open(app).selected.is_empty());
    let before = open(app).details_scroll;
    let buf = render(app, 120, 32);
    let (x, y) = position(&buf, "Detail line");
    mouse(app, MouseEventKind::ScrollDown, (x, y));
    assert!(open(app).details_scroll > before);
    assert_eq!(open(app).cursor, "bash");
    assert!(open(app).selected.is_empty());
    // Another item's details start at their top.
    let files = section(open(app), "files");
    open_mut(app).show_section(files);
    open_mut(app).highlight(&short);
    assert_eq!(open(app).details_scroll, 0);
}

/// Port of `test_layout_resizes_without_losing_selection_or_fixed_controls`.
#[test]
fn layout_resizes_without_losing_selection_or_fixed_controls() {
    let Boxed { mut setup, _home } = default_box();
    setup.show_section(section(&setup, "env"));
    let (mut fixture, _keep) = tui(setup);
    let app = &mut fixture.app;
    let buf = render(app, 140, 42);
    let (tree_x, _) = position(&buf, "[ ] 1. Bash");
    let (details_x, _) = position(&buf, "State registry:");
    assert!(details_x > tree_x + 20);
    let (checkbox, _) = row_parts(&buf, "[ ] 1. Bash");
    click(app, checkbox);
    for (width, height) in [(80, 24), (140, 42)] {
        let buf = render(app, width, height);
        let text = screen(&buf);
        assert!(text.contains("[x] 1. Bash"), "{text}");
        assert!(text.contains("review & apply"), "{text}");
        assert_eq!(selected(open(app)), set_of(&["bash"]));
        assert_eq!(open(app).cursor, "bash");
    }
    assert!(open(app).target_label("").contains("192.0.2.200"));
}

/// Port of `test_repository_initial_collapse_short_labels_and_full_identity`
/// (each group is a section: there is no group to collapse).
#[test]
fn repository_initial_collapse_short_labels_and_full_identity() {
    let Boxed { mut setup, _home } = with_settings(
        |home| {
            let mut cfg = settings(home);
            cfg.repo.owner = Some("example-owner".to_owned());
            cfg
        },
        None,
    );
    let repository = "example-owner/project".to_owned();
    setup.catalog.entries.push(Entry {
        id: repository.clone(),
        group: "repo".to_owned(),
        handler: "repository".to_owned(),
        label: repository.clone(),
        description: "Repository description".to_owned(),
        params: Params::Repository {
            repository: repository.clone(),
        },
        depends_on: Vec::new(),
    });
    setup
        .catalog
        .availability
        .insert(repository.clone(), "available".to_owned());
    let mut times = serde_json::Map::new();
    times.insert("created_at".to_owned(), json!("2026-09-01T00:00:00Z"));
    times.insert("pushed_at".to_owned(), json!("2026-09-10T00:00:00Z"));
    setup.catalog.timestamps.insert(repository.clone(), times);
    setup.rebuild();
    assert!(label(&setup, &repository).contains("project"));
    assert!(!label(&setup, &repository).contains("example-owner/"));
    setup.toggle(&repository, false);
    assert_eq!(selected(&setup), set_of(&[&repository]));
    let details = setup.details(&repository);
    assert!(details.contains("~/DEV/project"), "{details}");
    assert!(details.contains("example-owner/project"));
    assert!(details.contains("Last push: 2026-09-10T00:00:00Z"));
    assert!(setup.details("repo").contains("Recent first"));
}

/// Port of `test_mouse_hit_regions_survive_horizontal_and_vertical_scrolling`.
#[test]
fn mouse_hit_regions_survive_horizontal_and_vertical_scrolling() {
    let Boxed { mut setup, _home } = default_box();
    for i in 0..40 {
        setup.catalog.entries.push(Entry {
            id: format!("extra-{i}"),
            group: "app".to_owned(),
            handler: "application".to_owned(),
            label: format!("Extra {i} {}", "wide-label-".repeat(15)),
            description: "Test-only installer".to_owned(),
            params: Params::Application(Box::new(Application::new("true"))),
            depends_on: Vec::new(),
        });
    }
    setup.rebuild();
    setup.show_section(section(&setup, "app"));
    let (mut fixture, _keep) = tui(setup);
    let app = &mut fixture.app;
    app_key(app, KeyCode::End);
    assert_eq!(open(app).cursor, "extra-39");
    let buf = render(app, 120, 28);
    assert!(!screen(&buf).contains("1. Codex"));
    let (x, y) = position(&buf, ". Extra 39");
    let x = x + 2;
    click(app, (x, y));
    assert_eq!(open(app).cursor, "extra-39");
    click(app, (x.saturating_sub(6), y));
    assert_eq!(selected(open(app)), set_of(&["extra-39"]));
    let details = open(app).details_scroll;
    let buf = render(app, 120, 28);
    let (x, y) = position(&buf, "Extra 3");
    mouse(app, MouseEventKind::ScrollUp, (x, y));
    assert_eq!(open(app).details_scroll, details);
    assert_eq!(selected(open(app)), set_of(&["extra-39"]));
}

// ---- SetupActionTests ----

fn reviewed(setup: &mut SetupBox, id: &str) {
    setup.toggle(id, false);
    setup.review();
}

/// Port of `test_enter_opens_review_and_requires_a_second_confirmation`.
#[test]
fn enter_opens_review_and_requires_a_second_confirmation() {
    let Boxed { mut setup, _home } = default_box();
    setup.show_section(section(&setup, "app"));
    setup.highlight("codex");
    assert_eq!(key(&mut setup, KeyCode::Char(' ')), None);
    assert_eq!(key(&mut setup, KeyCode::Enter), None);
    assert!(matches!(setup.stage, Stage::Review { apply: true, .. }));
    assert!(!setup.busy);
    assert_eq!(key(&mut setup, KeyCode::Enter), Some(Ask::Execute));
    assert!(setup.busy);
    assert!(setup.pending_plan.is_some());
    setup.review();
    assert!(matches!(setup.stage, Stage::Running { .. }));
    setup.finished(json!({"ok": true, "results": [], "snapshot": {"created": false}}), None);
    assert!(!setup.busy);
    assert_eq!(key(&mut setup, KeyCode::Enter), None);
    assert!(matches!(setup.stage, Stage::Tree));
}

/// Port of `test_empty_selection_notifies_and_filter_enter_reviews`.
#[test]
fn empty_selection_notifies_and_filter_enter_reviews() {
    let Boxed { mut setup, _home } = default_box();
    key(&mut setup, KeyCode::Enter);
    assert_eq!(setup.notice.as_ref().unwrap().text, "No setup actions selected.");
    assert!(matches!(setup.stage, Stage::Tree));
    setup.selected.insert("codex".to_owned());
    key(&mut setup, KeyCode::Char('/'));
    key(&mut setup, KeyCode::Char('c'));
    key(&mut setup, KeyCode::Enter);
    assert_eq!(setup.filter, "c");
    assert!(matches!(setup.stage, Stage::Review { .. }));
}

/// Port of `test_compact_mouse_actions_back_focus_and_escape`: the buttons
/// in the box's border.
#[test]
fn compact_mouse_actions_back_focus_and_escape() {
    let Boxed { mut setup, _home } = default_box();
    setup.selected.extend(setup.descendant_ids("codex", false));
    let (mut fixture, _keep) = tui(setup);
    let app = &mut fixture.app;
    let buf = render(app, 120, 40);
    click(app, position(&buf, "review & apply"));
    assert!(matches!(open(app).stage, Stage::Review { .. }));
    app_key(app, KeyCode::BackTab);
    assert!(matches!(open(app).stage, Stage::Review { apply: false, .. }));
    app_key(app, KeyCode::Enter);
    assert!(matches!(open(app).stage, Stage::Tree));
    let buf = render(app, 120, 40);
    click(app, position(&buf, "review & apply"));
    app_key(app, KeyCode::Esc);
    assert!(matches!(open(app).stage, Stage::Tree));
    let buf = render(app, 120, 40);
    click(app, position(&buf, "review & apply"));
    let buf = render(app, 120, 40);
    click(app, position(&buf, "back esc"));
    assert!(matches!(open(app).stage, Stage::Tree));
    let buf = render(app, 120, 40);
    click(app, position(&buf, "review & apply"));
    let buf = render(app, 120, 40);
    let request = click(app, position(&buf, "apply this plan"));
    assert_eq!(request, Some(Request::SetupAsk(Ask::Execute)));
    open_mut(app).finished(json!({"ok": true, "results": [], "snapshot": {"created": false}}), None);
    let buf = render(app, 120, 40);
    click(app, position(&buf, "OK ↵"));
    assert!(matches!(open(app).stage, Stage::Tree));
}

/// Port of `test_execution_modal_shows_activity_and_closes_with_ok_or_escape`.
#[test]
fn execution_modal_shows_activity_and_closes_with_ok_or_escape() {
    let Boxed { mut setup, _home } = default_box();
    reviewed(&mut setup, "codex");
    assert_eq!(key(&mut setup, KeyCode::Char('a')), Some(Ask::Execute));
    setup.activity(None, "Verify workspace connection");
    setup.activity(Some("codex"), "Run application installer");
    setup.finished(
        json!({"ok": true, "results": [{"id": "codex", "label": "Codex", "status": "succeeded", "detail": "done"}],
               "snapshot": {"created": false}}),
        None,
    );
    let lines = run_lines(&setup).join("\n");
    assert!(lines.contains("Verify workspace connection"));
    assert!(lines.contains("codex: Run application installer"));
    assert!(lines.contains("Setup finished"));
    let (mut fixture, _keep) = tui(setup);
    let buf = render(&fixture.app, 120, 40);
    assert!(screen(&buf).contains("OK ↵"));
    app_key(&mut fixture.app, KeyCode::Esc);
    assert!(matches!(open(&fixture.app).stage, Stage::Tree));
}

/// Port of `test_execution_escape_requests_cancel_then_mouse_ok_closes_result`.
#[test]
fn execution_escape_requests_cancel_then_mouse_ok_closes_result() {
    let Boxed { mut setup, _home } = default_box();
    reviewed(&mut setup, "codex");
    key(&mut setup, KeyCode::Char('a'));
    assert!(setup.busy);
    let (mut fixture, _keep) = tui(setup);
    let app = &mut fixture.app;
    let buf = render(app, 120, 40);
    assert!(!screen(&buf).contains("OK ↵"));
    assert_eq!(app_key(app, KeyCode::Esc), Some(Request::SetupAsk(Ask::Cancel)));
    assert!(open(app).cancel_requested);
    assert!(run_lines(open(app)).contains(&"Cancellation requested".to_owned()));
    assert!(matches!(open(app).stage, Stage::Running { finished: false, .. }));
    open_mut(app).finished(
        json!({"ok": false, "results": [{"id": "codex", "label": "Codex", "status": "failed",
               "detail": "Cancelled; remaining actions were not run"}], "snapshot": {"created": false}}),
        None,
    );
    assert!(run_lines(open(app)).contains(&"Setup stopped".to_owned()));
    let buf = render(app, 120, 40);
    click(app, position(&buf, "OK ↵"));
    assert!(matches!(open(app).stage, Stage::Tree));
}

/// Port of `test_execution_activity_removes_control_characters_and_limits_lines`.
#[test]
fn execution_activity_removes_control_characters_and_limits_lines() {
    assert_eq!(clean_line(Some("id"), "one\n\x1b[31msecret\x1b[0m"), "id: one secret");
    assert_eq!(clean_line(None, &"x".repeat(600)).chars().count(), 500);
    assert_eq!(clean_line(Some("id"), " \n\t "), "");
}

/// Port of `test_execution_log_keeps_wrapped_final_result_visible`.
#[test]
fn execution_log_keeps_wrapped_final_result_visible() {
    let Boxed { mut setup, _home } = default_box();
    reviewed(&mut setup, "codex");
    key(&mut setup, KeyCode::Char('a'));
    for index in 0..30 {
        setup.activity(Some("repo"), &format!("{index}: {}", "long-path-".repeat(30)));
    }
    setup.finished(
        json!({"ok": true, "results": [{"id": "repo", "label": "Repository", "status": "succeeded",
               "detail": "Repository Git access verified"}], "snapshot": {"created": false}}),
        None,
    );
    let (fixture, _keep) = tui(setup);
    let text = screen(&render(&fixture.app, 80, 24));
    assert!(
        text.contains("Repository: succeeded — Repository Git access verified"),
        "{text}"
    );
}

/// Port of `test_details_enter_and_tab_order`: Tab walks the groups, Enter
/// reviews from any of them.
#[test]
fn details_enter_and_tab_order() {
    let Boxed { mut setup, _home } = default_box();
    let app_section = section(&setup, "app");
    setup.show_section(app_section);
    setup.highlight("codex");
    key(&mut setup, KeyCode::Char(' '));
    key(&mut setup, KeyCode::Tab);
    assert_eq!(setup.section, app_section + 1);
    key(&mut setup, KeyCode::Enter);
    assert!(matches!(setup.stage, Stage::Review { .. }));
    key(&mut setup, KeyCode::Esc);
    key(&mut setup, KeyCode::BackTab);
    assert_eq!(setup.section, app_section);
    assert_eq!(setup.cursor, "app");
}

/// The background of the review button: the accent bar when chosen.
fn review_button_chosen(app: &App) -> bool {
    let buf = render(app, 120, 40);
    let (x, y) = position(&buf, "review & apply");
    buf[(x, y)].bg == app.palette.focus
}

/// Port of `test_pending_key_transitions_include_hidden_selections`.
#[test]
fn pending_key_transitions_include_hidden_selections() {
    let Boxed { setup, _home } = default_box();
    let (mut fixture, _keep) = tui(setup);
    let app = &mut fixture.app;
    assert!(!review_button_chosen(app));
    open_mut(app).selected.insert("codex".to_owned());
    assert!(review_button_chosen(app));
    set_filter(open_mut(app), "bash");
    assert!(review_button_chosen(app));
    assert!(!open(app).nodes.contains_key("codex"));
    open_mut(app).selected.clear();
    assert!(!review_button_chosen(app));
}

/// Port of `test_success_clears_selection_and_restores_ready_green`.
#[test]
fn success_clears_selection_and_restores_ready_green() {
    let state = json!({"items": {"codex": {"ready": true, "state": "installed"}}});
    let Boxed { mut setup, _home } = state_box(state.clone());
    let codex = entry(&setup, "codex").clone();
    assert_eq!(setup.entry_style(&codex), Tone::Green);
    setup.toggle("codex", false);
    assert_eq!(setup.entry_style(&codex), Tone::Red);
    setup.busy = true;
    let results: Vec<Value> = setup
        .descendant_ids("codex", false)
        .iter()
        .map(|id| json!({"id": id, "label": id, "status": "succeeded", "detail": "done"}))
        .collect();
    setup.finished(json!({"ok": true, "results": results}), Some(state));
    assert!(setup.selected.is_empty());
    assert_eq!(setup.entry_style(&codex), Tone::Green);
    assert!(!setup.busy);
}

/// Port of `test_complete_details_fold_at_narrow_and_wide_widths`.
#[test]
fn complete_details_fold_at_narrow_and_wide_widths() {
    let Boxed { mut setup, _home } = default_box();
    let description = format!("Long description {}", "unbroken-path-".repeat(60));
    for entry in &mut setup.catalog.entries {
        if entry.id == "codex" {
            entry.description.clone_from(&description);
        }
    }
    setup.rebuild();
    setup.show_section(section(&setup, "app"));
    setup.highlight("codex");
    let details = setup.details("codex");
    let command = entry(&setup, "codex").application().unwrap().command.clone();
    assert!(details.contains("ID: codex"));
    assert!(details.contains(&format!("Command: {command}")));
    assert!(details.contains(&description));
    let (fixture, _keep) = tui(setup);
    for (width, height) in [(120, 40), (80, 24), (140, 42)] {
        let text = screen(&render(&fixture.app, width, height));
        assert!(text.contains("Status: UNKNOWN"), "{text}");
        assert!(text.contains("unbroken-path-unbroken"), "{text}");
        assert!(text.contains("review & apply"), "{text}");
    }
    assert!(open(&fixture.app).details("app").contains("State registry:"));
}

/// Port of `test_narrow_confirmation_actions_keep_complete_labels`.
#[test]
fn narrow_confirmation_actions_keep_complete_labels() {
    let Boxed { mut setup, _home } = default_box();
    reviewed(&mut setup, "codex");
    if let Stage::Review { lines, .. } = &mut setup.stage {
        *lines = (0..100).map(|i| format!("Action {i}")).collect();
    }
    let (mut fixture, _keep) = tui(setup);
    let text = screen(&render(&fixture.app, 80, 24));
    assert!(text.contains("back esc"), "{text}");
    assert!(text.contains("apply this plan ↵"), "{text}");
    app_key(&mut fixture.app, KeyCode::PageDown);
    assert!(matches!(open(&fixture.app).stage, Stage::Review { scroll: 10, .. }));
    app_key(&mut fixture.app, KeyCode::Esc);
}

/// Port of `test_custom_commands_stay_redacted_and_no_button_or_details_action_remains`.
#[test]
fn custom_commands_stay_redacted_and_no_button_or_details_action_remains() {
    let Boxed { mut setup, _home } = default_box();
    for entry in &mut setup.catalog.entries {
        if entry.id == "codex"
            && let Params::Application(application) = &mut entry.params
        {
            application.command = "SECRET_PAYLOAD".to_owned();
        }
    }
    setup.rebuild();
    assert!(!setup.details("codex").contains("SECRET_PAYLOAD"));
    assert!(setup.details("codex").contains("Custom payload withheld"));
}

/// Port of `test_complete_details_preserve_state_metadata_and_overwrite_colors`.
#[test]
fn complete_details_preserve_state_metadata_and_overwrite_colors() {
    let Boxed { mut setup, _home } = state_box(json!({
        "checked_at": "2026-09-11T01:00:00Z",
        "items": {"bash": {
            "ready": false, "will_overwrite": true, "state": "needs update",
            "first_managed_at": "first-managed", "installed_at": "installed",
            "last_applied_at": "last-applied", "last_snapshot": "~/snapshot/example",
            "files": [{"path": ".bashrc", "exists": true, "size": 123, "sha256": "abcdef0123456789",
                       "mtime_ns": 1_000_000_000, "birthtime_ns": 1_000_000_000}],
        }},
    }));
    let content = setup.details("bash");
    for value in [
        "ID: bash",
        "Status: UPDATE",
        "first-managed",
        "last-applied",
        "~/snapshot/example",
        "Managed path: ~/.bashrc",
        "SHA-256: abcdef0123456789",
        "Size: 123 bytes",
        "Created: 1970-01-01T00:00:01Z",
        "Modified: 1970-01-01T00:00:01Z",
        "State checked: 2026-09-11T01:00:00Z",
        "Managed paths:",
    ] {
        assert!(content.contains(value), "{value}: {content}");
    }
    let bash = entry(&setup, "bash").clone();
    assert_eq!(setup.entry_style(&bash), Tone::Plain);
    setup.selected.insert("bash".to_owned());
    assert_eq!(setup.entry_style(&bash), Tone::Red);
}

/// Port of `test_ready_environment_review_does_not_claim_an_update`.
#[test]
fn ready_environment_review_does_not_claim_an_update() {
    let Boxed { mut setup, _home } = state_box(json!({"items": {"bash": {"ready": true, "state": "configured"}}}));
    assert!(setup.details("bash").starts_with("Status: NO CHANGES"));
    setup.toggle("bash", false);
    let bash = entry(&setup, "bash").clone();
    assert_eq!(setup.entry_style(&bash), Tone::Green);
    setup.review();
    let text = review_text(&setup);
    assert!(text.contains("Verify shell configuration; write only if changes are needed"));
    assert!(!text.contains("Update / reapply existing state"));
}

/// Port of `test_managed_environment_with_missing_shell_is_not_green`.
#[test]
fn managed_environment_with_missing_shell_is_not_green() {
    let Boxed { setup, _home } = state_box(json!({
        "items": {"bash": {"ready": false, "managed": true, "state": "shell missing"}},
    }));
    let bash = entry(&setup, "bash").clone();
    assert_eq!(setup.entry_style(&bash), Tone::Plain);
}

/// Port of `test_modified_shell_file_shows_backup_and_overwrite`.
#[test]
fn modified_shell_file_shows_backup_and_overwrite() {
    let Boxed { mut setup, _home } = state_box(json!({
        "items": {"bash": {"ready": false, "managed": true, "state": "needs update",
                           "will_overwrite": true, "changed_since_apply": [".profile"]}},
    }));
    let content = setup.details("bash");
    assert!(content.starts_with("Status: UPDATE"));
    assert!(content.contains("Changed since last apply:\n~/.profile"));
    assert!(content.contains("backed up before replacement"));
    let bash = entry(&setup, "bash").clone();
    assert_eq!(setup.entry_style(&bash), Tone::Green);
    setup.toggle("bash", false);
    assert_eq!(setup.entry_style(&bash), Tone::Red);
    setup.toggle("bash", false);
    assert_eq!(setup.entry_style(&bash), Tone::Green);
    setup.toggle("bash", false);
    setup.review();
    let text = review_text(&setup);
    assert!(text.contains("Changed since last apply: ~/.profile"));
    assert!(text.contains("Back up modified shell files; overwrite configuration"));
    assert!(!text.contains("Overwrite existing configuration"));
}

/// Port of `test_stale_rebuild_restore_cannot_replace_newer_highlight`: a
/// rebuild keeps the highlight the user moved to.
#[test]
fn stale_rebuild_restore_cannot_replace_newer_highlight() {
    let Boxed { mut setup, _home } = default_box();
    setup.show_section(section(&setup, "app"));
    setup.rebuild();
    setup.highlight("codex");
    setup.rebuild();
    assert_eq!(setup.cursor, "codex");
    assert!(setup.details(&setup.cursor).contains("ID: codex"));
}

// ---- StructuredCatalogTests ----

/// Port of `test_runtime_nested_values_are_cataloged_from_explicit_toml`.
#[test]
fn runtime_nested_values_are_cataloged_from_explicit_toml() {
    let home = tempfile::tempdir().unwrap();
    let cfg = runtime_example_settings(home.path());
    let catalog = Catalog {
        entries: cfg.setup.items.clone(),
        ..Catalog::default()
    };
    let rows = catalog.rows(&cfg);
    let row = rows.iter().find(|row| row["id"] == "codex").unwrap();
    let config = &row["config_files"][0];
    assert_eq!(config["values"]["agents"]["enabled"], true);
    assert_eq!(config["values"]["agents"]["max_concurrent_threads_per_session"], 12);
    let selectors: Vec<&str> = config["selectors"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(selectors.contains(&leaf(&["agents", "enabled"]).as_str()));
    assert!(selectors.contains(&leaf(&["agents", "max_concurrent_threads_per_session"]).as_str()));
}

/// Port of `test_rows_keep_application_index_and_expose_nested_values_and_selectors`.
#[test]
fn rows_keep_application_index_and_expose_nested_values_and_selectors() {
    let home = tempfile::tempdir().unwrap();
    let cfg = fixture_settings(home.path());
    let catalog = Catalog {
        entries: cfg.setup.items.clone(),
        ..Catalog::default()
    };
    let rows = catalog.rows(&cfg);
    let row = rows.iter().find(|row| row["id"] == "codex").unwrap();
    let config = &row["config_files"][0];
    assert_eq!(row["index"], 1);
    assert_eq!(config["path"], "~/.fixture/config.toml");
    assert_eq!(config["values"]["root.key"], "literal");
    let selectors: Vec<&str> = config["selectors"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(selectors.contains(&leaf(&["features", "multi_agent"]).as_str()));
    assert!(config.get("validation").is_none());
}

/// Port of `test_stable_leaf_selector_selects_only_that_declared_value`.
#[test]
fn stable_leaf_selector_selects_only_that_declared_value() {
    let home = tempfile::tempdir().unwrap();
    let cfg = fixture_settings(home.path());
    let leaf_id = leaf(&["root.key"]);
    let identity = || -> Result<Value, AppError> { panic!("unexpected GitHub") };
    let catalog = || -> Catalog { panic!("unexpected GitHub") };
    let github = Github {
        identity: &identity,
        catalog: &catalog,
    };
    let (selected, snapshot) = select_entries(&cfg, &[format!("app={leaf_id}")], None, &github).unwrap();
    assert_eq!(snapshot, None);
    assert_eq!(
        selected.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        [leaf_id.as_str()]
    );
    assert!(matches!(&selected[0].params, Params::Structured { key, .. } if key == &["root.key".to_owned()]));
}

// ---- StructuredTUITests ----

/// Port of `test_absent_codex_config_declaration_has_no_config_nodes`.
#[test]
fn absent_codex_config_declaration_has_no_config_nodes() {
    let Boxed { setup, _home } = default_box();
    assert!(setup.nodes.contains_key("codex"));
    assert!(!setup.nodes.contains_key("codex:config"));
    assert!(!setup.nodes.keys().any(|id| id.starts_with("codex:config:")));
}

/// Port of `test_runtime_nested_values_appear_in_tui_without_python_catalog_changes`.
#[test]
fn runtime_nested_values_appear_in_tui_without_python_catalog_changes() {
    let Boxed { setup, _home } = with_settings(runtime_example_settings, None);
    let agents = leaf(&["agents", "enabled"]);
    let threads = leaf(&["agents", "max_concurrent_threads_per_session"]);
    assert!(label(&setup, &agents).contains("enabled = true"));
    assert!(label(&setup, &threads).contains("max_concurrent_threads_per_session = 12"));
}

/// Port of `test_hierarchical_counters_use_visible_semantic_levels`.
#[test]
fn hierarchical_counters_use_visible_semantic_levels() {
    let Boxed { mut setup, _home } = with_settings(example_settings, None);
    assert!(
        label(&setup, "app").contains("Applications  0/8"),
        "{}",
        label(&setup, "app")
    );
    assert!(label(&setup, "codex").contains("0/8"));
    let digits = |text: &str| {
        text.split_whitespace()
            .any(|w| w.contains('/') && w.chars().all(|c| c.is_ascii_digit() || c == '/'))
    };
    assert!(!digits(&label(&setup, "opencode")));
    assert!(!digits(&label(&setup, "hermes")));
    assert!(!setup.rows().iter().any(|row| row.id == "codex:config"));
    setup.toggle(&leaf(&["approvals_reviewer"]), false);
    assert!(label(&setup, "app").contains("Applications  1/8"));
    assert!(label(&setup, "codex").contains("Codex  1/8"));
    setup.toggle("opencode", false);
    assert!(label(&setup, "app").contains("Applications  2/8"));
}

fn example_codex(setup: &SetupBox) -> Entry {
    setup
        .settings
        .setup
        .items
        .iter()
        .find(|e| e.id == "codex")
        .unwrap()
        .clone()
}

/// Port of `test_config_file_and_section_counters_count_only_leaf_descendants`.
#[test]
fn config_file_and_section_counters_count_only_leaf_descendants() {
    let Boxed { mut setup, _home } = with_settings(example_settings, None);
    setup.toggle("codex", false);
    let mut expected: Vec<String> = config_entries(&example_codex(&setup))
        .into_iter()
        .map(|e| e.id)
        .collect();
    expected.push("codex".to_owned());
    expected.sort();
    assert_eq!(selected(&setup), expected);
    let tui_id = leaf(&["tui"]);
    let colors = leaf(&["tui", "status_line_use_colors"]);
    let features = leaf(&["features"]);
    let context = leaf(&["features", "context_management"]);
    assert!(label(&setup, "codex").contains("Codex  8/8"));
    assert!(label(&setup, "codex:config").contains("~/.codex/config.toml  8/8"));
    assert!(label(&setup, &tui_id).contains("tui  2/2"));
    assert!(label(&setup, &features).contains("features  3/3"));
    assert!(label(&setup, &context).contains("context_management  1/1"));
    setup.toggle(&colors, false);
    assert_eq!(setup.checkbox("codex"), "[-]");
    assert!(label(&setup, "codex").contains("Codex  7/8"));
    assert!(label(&setup, "codex:config").contains("~/.codex/config.toml  7/8"));
    assert!(label(&setup, &tui_id).contains("tui  1/2"));
    assert!(label(&setup, &features).contains("features  3/3"));
    assert!(label(&setup, &context).contains("context_management  1/1"));
}

/// Port of `test_section_bulk_selection_only_selects_managed_leaf_descendants`.
#[test]
fn section_bulk_selection_only_selects_managed_leaf_descendants() {
    let Boxed { mut setup, _home } = with_settings(example_settings, None);
    let section_id = leaf(&["tui"]);
    let mut tui_leaves: Vec<String> = config_entries(&example_codex(&setup))
        .into_iter()
        .filter(|e| matches!(&e.params, Params::Structured { key, .. } if key[0] == "tui"))
        .map(|e| e.id)
        .collect();
    tui_leaves.sort();
    setup.toggle(&section_id, false);
    assert_eq!(selected(&setup), tui_leaves);
    assert_eq!(setup.checkbox(&section_id), "[x]");
    assert!(label(&setup, &section_id).contains("tui  2/2"));
    setup.toggle(&section_id, false);
    assert!(setup.selected.is_empty());
    assert_eq!(setup.checkbox(&section_id), "[ ]");
}

/// Port of `test_installer_only_application_is_partial_and_group_counts_applications`.
#[test]
fn installer_only_application_is_partial_and_group_counts_applications() {
    let Boxed { mut setup, _home } = with_settings(example_settings, None);
    setup.toggle("codex", false);
    setup.toggle("codex:config", false);
    assert_eq!(selected(&setup), set_of(&["codex"]));
    assert_eq!(setup.checkbox("codex"), "[-]");
    assert!(label(&setup, "codex").contains("Codex  0/8"));
    assert!(label(&setup, "app").contains("Applications  1/8"));
}

/// Port of `test_application_group_tri_state_tracks_involved_visible_applications`.
#[test]
fn application_group_tri_state_tracks_involved_visible_applications() {
    let Boxed { mut setup, _home } = with_settings(example_settings, None);
    assert_eq!(setup.checkbox("app"), "[ ]");
    let reviewer = leaf(&["approvals_reviewer"]);
    setup.toggle(&reviewer, false);
    assert_eq!(setup.checkbox("app"), "[-]");
    assert!(label(&setup, "app").contains("Applications  1/8"));
    setup.toggle("codex", false);
    setup.toggle("opencode", false);
    assert_eq!(setup.checkbox("app"), "[-]");
    assert!(label(&setup, "app").contains("Applications  2/8"));
    for id in ["herdr", "hermes", "pi", "opencode-v2", "rust", "claude"] {
        setup.toggle(id, false);
    }
    assert_eq!(setup.checkbox("app"), "[x]");
    assert!(label(&setup, "app").contains("Applications  8/8"));
    setup.toggle(&reviewer, false);
    assert_eq!(setup.checkbox("app"), "[-]");
    assert!(label(&setup, "app").contains("Applications  8/8"));
}

/// Port of `test_application_and_config_details_distinguish_state_and_counts`.
#[test]
fn application_and_config_details_distinguish_state_and_counts() {
    let home = tempfile::tempdir().unwrap();
    let cfg = example_settings(home.path());
    let codex = cfg.setup.items.iter().find(|e| e.id == "codex").unwrap().clone();
    let leaves = config_entries(&codex);
    let mut items = serde_json::Map::new();
    items.insert("codex".to_owned(), json!({"state": "installed"}));
    for (index, leaf_entry) in leaves.iter().enumerate() {
        items.insert(
            leaf_entry.id.clone(),
            json!({"state": if index == 0 { "different" } else { "matching" }}),
        );
    }
    let catalog = load_catalog(&cfg, None);
    let mut setup = SetupBox::new(cfg, target(), catalog, Some(json!({"items": items})));
    setup.selected = leaves[1..].iter().map(|e| e.id.clone()).collect();
    let application = setup.details("codex");
    assert!(application.contains("Application state: installed"));
    assert!(application.contains("Install/update: not selected"));
    assert!(application.contains("Configuration: selected 7/8 managed options"));
    let file = setup.details("codex:config");
    assert!(file.contains("Configuration: selected 7/8 managed options"));
    assert!(file.contains("Workspace state: 1 different, 7 matching"), "{file}");
    setup.selected = [leaf(&["tui", "status_line"])].into_iter().collect();
    let section_details = setup.details(&leaf(&["tui"]));
    assert!(section_details.contains("Configuration: selected 1/2 managed options"));
    assert!(section_details.contains("Workspace state: 2 matching"));
}

/// Port of `test_details_show_workspace_value_beside_desired_value`.
#[test]
fn details_show_workspace_value_beside_desired_value() {
    let policy = leaf(&["approval_policy"]);
    let sandbox = leaf(&["sandbox_mode"]);
    let reviewer = leaf(&["approvals_reviewer"]);
    let mut items = serde_json::Map::new();
    items.insert(policy.clone(), json!({"state": "matching", "current": "never"}));
    items.insert(
        sandbox.clone(),
        json!({"state": "different", "current": "workspace-write"}),
    );
    items.insert(reviewer.clone(), json!({"state": "missing"}));
    let Boxed { setup, _home } = with_settings(example_settings, Some(json!({"items": items})));
    let sandbox_details = setup.details(&sandbox);
    assert!(
        sandbox_details.contains("Workspace value:\n\"workspace-write\""),
        "{sandbox_details}"
    );
    assert!(sandbox_details.contains("Desired value:\n\"danger-full-access\""));
    assert!(
        setup
            .details(&policy)
            .contains("Workspace value (matches desired):\n\"never\"")
    );
    assert!(setup.details(&reviewer).contains("Workspace value:\n(not set)"));
    let file = setup.details("codex:config");
    assert!(file.contains("sandbox_mode  [different]"));
    assert!(file.contains("\"workspace-write\""));
    assert!(file.contains("\"danger-full-access\""));
}

/// Port of `test_filtered_bulk_selection_preserves_hidden_selected_counts`.
#[test]
fn filtered_bulk_selection_preserves_hidden_selected_counts() {
    let Boxed { mut setup, _home } = with_settings(example_settings, None);
    setup.toggle("codex", false);
    set_filter(&mut setup, "approvals_reviewer");
    assert!(label(&setup, "app").contains("Applications  1/8"));
    assert!(label(&setup, "codex").contains("Codex  8/8"));
    assert!(label(&setup, "codex:config").contains("~/.codex/config.toml  8/8"));
    setup.toggle("codex:config", false);
    assert!(label(&setup, "app").contains("Applications  1/8"));
    assert!(label(&setup, "codex").contains("Codex  7/8"));
    assert!(label(&setup, "codex:config").contains("~/.codex/config.toml  7/8"));
    assert!(setup.selected.contains(&leaf(&["tui", "status_line"])));
}

/// Port of `test_config_branches_start_collapsed_and_preserve_manual_expansion`.
#[test]
fn config_branches_start_collapsed_and_preserve_manual_expansion() {
    let Boxed { mut setup, _home } = with_settings(fixture_settings, None);
    setup.show_section(section(&setup, "app"));
    let branches = ["codex".to_owned(), "codex:config".to_owned(), leaf(&["tui"])];
    let shown = |setup: &SetupBox, id: &str| setup.rows().iter().any(|row| row.id == id);
    assert!(!shown(&setup, "codex:config"));
    for id in &branches {
        setup.set_open(id, true);
    }
    setup.rebuild();
    assert!(shown(&setup, &leaf(&["tui", "status_line"])));
    setup.set_open("codex", false);
    setup.rebuild();
    assert!(!shown(&setup, "codex:config"));
    setup.set_open("codex", true);
    assert!(shown(&setup, &leaf(&["tui", "status_line"])));
}

/// Port of `test_config_tree_has_file_sections_and_leaves_without_root_group`.
#[test]
fn config_tree_has_file_sections_and_leaves_without_root_group() {
    let Boxed { setup, _home } = with_settings(fixture_settings, None);
    let tui_id = leaf(&["tui"]);
    let colors = leaf(&["tui", "status_line_use_colors"]);
    let context = leaf(&["features", "context_management"]);
    for id in ["codex:config", tui_id.as_str(), colors.as_str(), context.as_str()] {
        assert!(setup.nodes.contains_key(id), "{id}");
    }
    assert_eq!(setup.nodes["codex:config"].parent.as_deref(), Some("codex"));
    assert_eq!(setup.nodes[&tui_id].parent.as_deref(), Some("codex:config"));
    assert_eq!(setup.nodes[&colors].parent.as_deref(), Some(tui_id.as_str()));
    assert!(!setup.settings.setup.groups.iter().any(|g| g.id == "config"));
    let text = label(&setup, &colors);
    assert!(text.contains("status_line_use_colors"));
    assert!(text.contains("true"));
}

/// Port of `test_application_file_section_and_leaf_selection_are_tri_state`.
#[test]
fn application_file_section_and_leaf_selection_are_tri_state() {
    let Boxed { mut setup, _home } = with_settings(fixture_settings, None);
    let tui_id = leaf(&["tui"]);
    let colors = leaf(&["tui", "status_line_use_colors"]);
    setup.toggle("codex:config", false);
    let mut expected = setup.descendant_ids("codex:config", false);
    expected.sort();
    assert_eq!(selected(&setup), expected);
    assert_eq!(setup.checkbox("codex:config"), "[x]");
    assert_eq!(setup.checkbox("codex"), "[-]");
    setup.toggle(&colors, false);
    assert!(!setup.selected.contains(&colors));
    assert_eq!(setup.checkbox(&tui_id), "[-]");
    assert_eq!(setup.checkbox("codex:config"), "[-]");
    setup.rebuild();
    assert!(label(&setup, "codex").contains("[-] 1. Codex"));
    assert!(label(&setup, "codex:config").contains("[-] ~/.fixture/config.toml"));
    assert!(label(&setup, &tui_id).contains("[-] tui"));
    setup.toggle("codex", false);
    assert!(setup.selected.contains("codex"));
    assert_eq!(setup.checkbox("codex"), "[x]");
    setup.toggle(&colors, false);
    assert_eq!(setup.checkbox(&tui_id), "[-]");
    assert_eq!(setup.checkbox("codex"), "[-]");
}

/// Port of `test_leaf_details_show_complete_declared_array_and_review_uses_explicit_leaves`.
#[test]
fn leaf_details_show_complete_declared_array_and_review_uses_explicit_leaves() {
    let Boxed { mut setup, _home } = with_settings(fixture_settings, None);
    let leaf_id = leaf(&["tui", "status_line"]);
    let details = setup.details(&leaf_id);
    for value in ["model", "current-dir", "git-branch"] {
        assert!(details.contains(value), "{details}");
    }
    setup.toggle(&leaf_id, false);
    setup.review();
    let Stage::Review { plan, .. } = &setup.stage else {
        panic!("no review");
    };
    assert_eq!(
        plan.entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        [leaf_id.as_str()]
    );
}

// ---- StructuredPlanRenderingTests ----

/// Port of `test_text_plan_includes_structured_key_and_desired_value`.
#[test]
fn text_plan_includes_structured_key_and_desired_value() {
    let home = tempfile::tempdir().unwrap();
    let cfg = fixture_settings(home.path());
    let codex = cfg.setup.items.iter().find(|e| e.id == "codex").unwrap().clone();
    let root = config_entries(&codex)
        .into_iter()
        .find(|e| e.id == leaf(&["root.key"]))
        .unwrap();
    let plan = Plan {
        target: target(),
        entries: vec![root],
        catalog_id: None,
        unattended: false,
    };
    let style = hs_cli::view::Style {
        color: false,
        width: 120,
    };
    let rendered = hs_cli::setup::show::plan(&plan, &style);
    assert!(rendered.contains("root.key"), "{rendered}");
    assert!(rendered.contains("\"literal\""), "{rendered}");
}

// ---- the TUI tests of the groups ----

/// Port of `test_setup_state.py::SetupTUIStateTests::test_ready_item_is_green_and_selected_ready_item_is_red`.
#[test]
fn ready_item_is_green_and_selected_ready_item_is_red() {
    let Boxed { mut setup, _home } = state_box(json!({
        "items": {"codex": {"state": "installed", "ready": true, "will_overwrite": true}},
        "checked_at": "2026-09-11T00:00:00Z", "registry_present": false,
    }));
    let codex = entry(&setup, "codex").clone();
    assert_eq!(setup.entry_style(&codex), Tone::Green);
    setup.toggle("codex", false);
    assert_eq!(setup.entry_style(&codex), Tone::Red);
}

/// Port of `test_setup_app_files.py::AppFileTUITests::test_files_are_children_of_their_application`.
#[test]
fn files_are_children_of_their_application() {
    let Boxed { mut setup, _home } = with_settings(
        |home| {
            std::fs::create_dir_all(home.join(".claude")).unwrap();
            std::fs::write(home.join(".claude/.credentials.json"), "{}").unwrap();
            std::fs::write(home.join(".claude/statusline.sh"), "#!/bin/sh\n").unwrap();
            let mut cfg = settings(home);
            let raw: toml::Table = toml::from_str(
                "[[items]]\nid = \"claude\"\nfiles = [\"~/.claude/.credentials.json\", \"~/.claude/statusline.sh\"]\n",
            )
            .unwrap();
            cfg.setup = parse_setup(&raw).unwrap();
            cfg
        },
        None,
    );
    let credentials = "claude:file:~/.claude/.credentials.json";
    assert_eq!(setup.nodes[credentials].parent.as_deref(), Some("claude"));
    assert!(
        label(&setup, "claude").contains("Claude Code  0/2"),
        "{}",
        label(&setup, "claude")
    );
    setup.toggle("claude", false);
    assert!(setup.selected.contains(credentials));
    assert!(label(&setup, "claude").contains("Claude Code  2/2"));
    assert!(setup.details(credentials).contains("Application: claude"));
    assert!(setup.details("claude").contains("Desktop file copies:"));
}

/// Port of `test_setup_backup.py::BackupTUITests::test_backup_group_and_item_are_rendered_by_the_generic_tree`.
#[test]
fn backup_group_and_item_are_rendered_by_the_generic_tree() {
    let Boxed { mut setup, _home } = default_box();
    assert!(label(&setup, "backup").contains("Backup"));
    assert!(label(&setup, "bk").contains("BK"));
    setup.show_section(section(&setup, "backup"));
    let (fixture, _keep) = tui(setup);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("‹ Backup ›"), "{text}");
    assert!(text.contains("1. BK"), "{text}");
}

/// Port of `test_setup_herdr.py::HerdrSetupTests::test_tui_details_explain_linger_and_separate_authentication`.
#[test]
fn tui_details_explain_linger_and_separate_authentication() {
    let Boxed { setup, _home } = default_box();
    let details = setup.details("herdr");
    assert!(details.contains("without a user login"));
    assert!(details.contains("YubiKey"));
}

// ---- StartupProgressTests (the worker's start) ----

/// A connection that says when it is closed.
struct Recorded(Arc<Mutex<Vec<String>>>);

impl Workspace for Recorded {
    fn run(&mut self, _command: &str, _input: Option<&str>, _interactive: bool) -> Result<Output, AppError> {
        Ok(Output::default())
    }

    fn transfer(&mut self, _source: &str, _destination: &str) -> Result<(), AppError> {
        Ok(())
    }
}

impl Drop for Recorded {
    fn drop(&mut self) {
        self.0.lock().unwrap().push("closed".to_owned());
    }
}

/// Run the start with a connection and an inspection that may fail; the
/// steps told, and what happened.
fn run_startup(fail_connection: bool, fail_inspection: bool) -> (Vec<String>, Result<Value, String>) {
    let home = tempfile::tempdir().unwrap();
    let cfg = settings(home.path());
    let catalog = load_catalog(&cfg, None);
    let log = Arc::new(Mutex::new(Vec::new()));
    let told = Arc::clone(&log);
    let tell: Tell = Arc::new(move |event| {
        if let Event::Loading(message) = event {
            told.lock().unwrap().push(message);
        }
    });
    let connected = Arc::clone(&log);
    let inspected = Arc::clone(&log);
    let result = startup(
        &tell,
        cfg,
        target(),
        &catalog,
        "Connecting/authenticating SSH (touch your security key if prompted)…",
        move |_, _| {
            if fail_connection {
                return Err(AppError::new("Authentication failed"));
            }
            connected.lock().unwrap().push("connected".to_owned());
            Ok(Box::new(Recorded(Arc::clone(&connected))) as Box<dyn Workspace>)
        },
        move |_session| {
            let last = inspected.lock().unwrap().last().cloned().unwrap_or_default();
            assert!(last.contains("SSH connected"), "{last}");
            if fail_inspection {
                return Err(AppError::new("Inspection failed"));
            }
            Ok(json!({"items": {}}))
        },
    );
    let outcome = result.map(|(session, state)| {
        drop(session);
        state
    });
    let phases = log.lock().unwrap().clone();
    (phases, outcome.map_err(|error| error.0))
}

/// Port of `test_connected_spinner_during_inspection_and_stopped_before_tui`.
#[test]
fn connected_spinner_during_inspection_and_stopped_before_tui() {
    let (phases, outcome) = run_startup(false, false);
    assert_eq!(outcome.unwrap(), json!({"items": {}}));
    assert_eq!(
        phases,
        [
            "Connecting/authenticating SSH (touch your security key if prompted)…",
            "connected",
            "SSH connected; inspecting workspace…",
            "closed"
        ]
    );
}

/// Port of `test_inspection_error_stops_spinner_and_closes_connection`.
#[test]
fn inspection_error_stops_spinner_and_closes_connection() {
    let (phases, outcome) = run_startup(false, true);
    assert_eq!(outcome.unwrap_err(), "Inspection failed");
    assert_eq!(phases.last().map(String::as_str), Some("closed"));
}

/// Port of `test_failed_authentication_stops_spinner_without_connected_status`.
#[test]
fn failed_authentication_stops_spinner_without_connected_status() {
    let (phases, outcome) = run_startup(true, false);
    assert_eq!(outcome.unwrap_err(), "Authentication failed");
    assert!(!phases.iter().any(|phase| phase.contains("SSH connected")));
    assert!(!phases.iter().any(|phase| phase == "connected"));
}

/// `e` opens the box on a running workspace only, and the TUI asks its
/// worker to start; Esc while it connects closes it.
#[test]
fn key_e_opens_the_setup_of_a_running_workspace() {
    let mut fixture = tui_fixture::app();
    let app = &mut fixture.app;
    app_key(app, KeyCode::Down);
    let running = app.selected_entry().map(|entry| entry.status == "running").unwrap();
    let request = app_key(app, KeyCode::Char('e'));
    if running {
        let vmid = app.selected.unwrap();
        assert_eq!(request, Some(Request::Setup(Some(vmid))));
        assert!(matches!(open(app).stage, Stage::Loading { .. }));
        assert!(screen(&render(app, 120, 36)).contains(&format!("setup {vmid}")));
        assert_eq!(app_key(app, KeyCode::Esc), Some(Request::SetupAsk(Ask::Close)));
        assert!(app.setup.is_none());
    } else {
        assert_eq!(request, None);
    }
    // A stopped workspace has no setup.
    let stopped = app
        .rows()
        .iter()
        .find(|entry| entry.status != "running")
        .map(|entry| entry.vmid)
        .unwrap();
    app.selected = Some(stopped);
    assert_eq!(app_key(app, KeyCode::Char('e')), None);
    assert!(app.setup.is_none());
}

/// Default groups are the sections, walked with Tab; each shows its items.
#[test]
fn every_group_is_a_section() {
    let Boxed { mut setup, _home } = default_box();
    let groups: Vec<String> = setup.settings.setup.groups.iter().map(|g| g.id.clone()).collect();
    for (index, group) in groups.iter().enumerate() {
        assert_eq!(setup.section, index);
        assert_eq!(setup.cursor, *group);
        let rows = setup.rows();
        assert_eq!(rows[0].id, *group);
        assert!(rows.iter().skip(1).all(|row| {
            setup
                .catalog
                .entries
                .iter()
                .any(|e| e.id == row.id && e.group == *group)
        }));
        key(&mut setup, KeyCode::Tab);
    }
    assert_eq!(setup.section, 0);
}

/// Every group is in the bar with its count; a click shows that group. The
/// details of a group carry no key help (it is in Menu › Help).
#[test]
fn the_group_bar_shows_every_group_and_a_click_opens_it() {
    let Boxed { mut setup, _home } = default_box();
    setup.toggle("codex", false);
    assert!(!setup.details("env").contains("Space or a click"));
    let (mut fixture, _keep) = tui(setup);
    let app = &mut fixture.app;
    let buf = render(app, 120, 36);
    let text = screen(&buf);
    for group in [
        " Files 0/",
        " Environment 0/",
        " Applications 1/",
        " Credentials 0/",
        " Backup 0/",
        " Repositories 0/",
    ] {
        assert!(text.contains(group), "{group}: {text}");
    }
    click(app, position(&buf, " Backup 0/"));
    assert_eq!(open(app).section, section(open(app), "backup"));
    assert_eq!(open(app).cursor, "backup");
}

/// The box takes the width of the storage and log panels while it is open;
/// they come back when it closes. Each item shows its state, and what
/// applying it does once it is selected.
#[test]
fn the_box_is_wide_and_every_item_shows_its_state() {
    let mut fixture = tui_fixture::app();
    let app = &mut fixture.app;
    app_key(app, KeyCode::Down);
    let vmid = app.rows().iter().find(|entry| entry.status == "running").unwrap().vmid;
    app.selected = Some(vmid);
    let before = app.shown;
    assert!(before.storage && before.log);
    app_key(app, KeyCode::Char('e'));
    assert!(!app.shown.storage && !app.shown.log);
    app_key(app, KeyCode::Esc);
    assert_eq!(app.shown, before);
    let Boxed { mut setup, _home } = state_box(json!({"items": {
        "bash": {"ready": true, "state": "configured"},
        "zsh": {"ready": false, "will_overwrite": true, "state": "needs update"},
        "fish": {"ready": false, "state": "not configured"},
    }}));
    assert_eq!(setup.row_state("bash"), Some(("✓ configured".to_owned(), Tone::Green)));
    assert_eq!(
        setup.row_state("zsh"),
        Some(("! needs update".to_owned(), Tone::Yellow))
    );
    assert_eq!(
        setup.row_state("fish"),
        Some(("○ not configured".to_owned(), Tone::Dim))
    );
    assert_eq!(setup.row_state("env"), None);
    setup.toggle("fish", false);
    setup.toggle("zsh", false);
    assert_eq!(setup.row_state("fish").unwrap().0, "○ not configured → install");
    assert_eq!(
        setup.row_state("zsh"),
        Some(("! needs update → update".to_owned(), Tone::Red))
    );
    setup.show_section(section(&setup, "env"));
    let (fixture, _keep) = tui(setup);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("STATE"), "{text}");
    assert!(text.contains("✓ configured"), "{text}");
}
