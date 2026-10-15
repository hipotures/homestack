//! The main screen of the TUI (phase 13b, step 1; mockups 09–11, 15–17):
//! panels from the server's status, selection, details, filter, menu,
//! themes and the size notice. Drawn into a ratatui buffer, as MyPowers
//! tests its screens.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod tui_fixture;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hs_cli::tui::draw::render;
use hs_cli::tui::state::Request;
use hs_cli::tui::theme::Palette;
use tui_fixture::{app, details, position, press, row, screen};

#[test]
fn a_terminal_below_80_by_24_shows_only_the_size_notice() {
    let fixture = app();
    let buf = render(&fixture.app, 70, 20);
    let text = screen(&buf);
    assert!(text.contains("Terminal too small"), "{text}");
    assert!(text.contains("70") && text.contains("20"), "{text}");
    assert!(text.contains("80") && text.contains("24"), "{text}");
    assert!(!text.contains("workspaces"), "{text}");
}

#[test]
fn at_80_by_24_the_cluster_and_the_workspaces_are_drawn_in_btop_borders() {
    let fixture = app();
    let buf = render(&fixture.app, 80, 24);
    let text = screen(&buf);
    assert!(row(&buf, 0).starts_with("╭─┐²cluster┌┐menu┌┐view auto┌"), "{text}");
    assert!(row(&buf, 3).starts_with("╭─┐¹workspaces┌┐filter┌"), "{text}");
    assert!(!text.contains("³storage") && !text.contains("⁴log"), "{text}");
    for name in ["dev-main", "rust-builder", "notes", "pve1", "10.0.0.21"] {
        assert!(text.contains(name), "{name}: {text}");
    }
    // The first workspace is selected from the start: the counter says so
    // and the actions are lit (the owner, 2026-10-15).
    assert!(row(&buf, 23).contains("┘1/12└"), "{text}");
    let palette = Palette::named("HomeStack", false).unwrap();
    let (x, y) = position(&buf, "refresh");
    assert_eq!(buf[(x + 1, y)].fg, palette.foreground);
}

#[test]
fn the_cluster_line_names_nodes_servers_gold_and_free_storage() {
    let fixture = app();
    let buf = render(&fixture.app, 80, 24);
    let line = row(&buf, 1);
    assert!(line.contains("pve1") && line.contains("pve3"), "{line}");
    assert!(line.contains("servers ●●●"), "{line}");
    assert!(
        line.contains("gold ○ 112"),
        "the dot before, as for the servers: {line}"
    );
    assert!(line.contains("free"), "{line}");
}

#[test]
fn the_first_workspace_is_selected_and_down_moves_the_selection() {
    let mut fixture = app();
    let buf = render(&fixture.app, 80, 24);
    assert!(row(&buf, 23).contains("┘1/12└"), "{}", screen(&buf));
    let palette = Palette::named("HomeStack", false).unwrap();
    let (x, y) = position(&buf, "dev-main");
    assert_eq!(buf[(x, y)].bg, palette.focus);
    assert_eq!(press(&mut fixture.app, KeyCode::Down), Some(Request::Details(202)));
    assert_eq!(press(&mut fixture.app, KeyCode::Up), Some(Request::Details(201)));
    let buf = render(&fixture.app, 80, 24);
    assert!(row(&buf, 23).contains("┘1/12└"), "{}", screen(&buf));
    let palette = Palette::named("HomeStack", false).unwrap();
    let (x, y) = position(&buf, "dev-main");
    assert_eq!(buf[(x, y)].bg, palette.focus);
    let (x, y) = position(&buf, "rust-builder");
    assert_ne!(buf[(x, y)].bg, palette.focus);
}

#[test]
fn info_at_80_columns_splits_the_list_and_shows_its_scrollbar() {
    let mut fixture = app();
    fixture.app.set_details(201, &details());
    press(&mut fixture.app, KeyCode::Char('i'));
    let buf = render(&fixture.app, 80, 24);
    let text = screen(&buf);
    assert!(text.contains("┐201 dev-main┌"), "{text}");
    assert!(text.contains("ssh dev-main"), "{text}");
    let right: String = (0..24).map(|y| buf[(79, y)].symbol().to_owned()).collect();
    assert!(
        right.contains('↑') && right.contains('↓') && right.contains('█'),
        "{right}\n{text}"
    );
    press(&mut fixture.app, KeyCode::Char('i'));
    let text = screen(&render(&fixture.app, 80, 24));
    assert!(!text.contains("┐201 dev-main┌"), "{text}");
}

#[test]
fn at_120_by_36_all_panels_and_the_details_box_are_drawn() {
    let mut fixture = app();
    fixture.app.set_details(201, &details());
    // The details box opens on i only.
    assert!(!screen(&render(&fixture.app, 120, 36)).contains("┐201 dev-main┌"));
    press(&mut fixture.app, KeyCode::Char('i'));
    let buf = render(&fixture.app, 120, 36);
    let text = screen(&buf);
    for part in ["┐³storage┌", "┐⁴log┌", "┐201 dev-main┌", "local-zfs", "ADDRESS", "api2"] {
        assert!(text.contains(part), "{part}: {text}");
    }
}

#[test]
fn at_160_by_45_the_list_gains_the_root_column() {
    let fixture = app();
    let text = screen(&render(&fixture.app, 160, 45));
    assert!(text.contains("ROOT"), "{text}");
}

#[test]
fn a_digit_hides_its_panel_and_the_menu_moves_to_the_first_panel_shown() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Char('2'));
    let buf = render(&fixture.app, 120, 36);
    let top = row(&buf, 0);
    assert!(top.starts_with("╭─┐¹workspaces┌┐menu┌┐view auto┌┐filter┌"), "{top}");
    press(&mut fixture.app, KeyCode::Char('2'));
    let buf = render(&fixture.app, 120, 36);
    assert!(row(&buf, 0).starts_with("╭─┐²cluster┌"));
}

#[test]
fn v_cycles_the_views() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Char('v'));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("┐view 1┌") && !text.contains("³storage"), "{text}");
    press(&mut fixture.app, KeyCode::Char('v'));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("┐view 2┌") && !text.contains("²cluster"), "{text}");
    press(&mut fixture.app, KeyCode::Char('v'));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("┐view auto┌") && text.contains("⁴log"), "{text}");
}

#[test]
fn the_filter_opens_in_the_border_and_keeps_matching_rows() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Char('f'));
    for c in "lab".chars() {
        press(&mut fixture.app, KeyCode::Char(c));
    }
    let text = screen(&render(&fixture.app, 80, 24));
    assert!(text.contains("┐filter: lab"), "{text}");
    press(&mut fixture.app, KeyCode::Enter);
    let text = screen(&render(&fixture.app, 80, 24));
    assert!(text.contains("web-lab") && text.contains("ml-lab"), "{text}");
    assert!(!text.contains("dev-main"), "{text}");
    assert!(text.contains("┘1/2└"), "{text}");
    press(&mut fixture.app, KeyCode::Esc);
    let text = screen(&render(&fixture.app, 80, 24));
    assert!(text.contains("dev-main") && text.contains("/12└"), "{text}");
}

#[test]
fn the_node_carousel_keeps_the_rows_of_one_node() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Right);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("‹ node: pve1 ›"), "{text}");
    assert!(text.contains("web-lab") && !text.contains("dev-main"), "{text}");
    press(&mut fixture.app, KeyCode::Left);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("‹ node: all ›") && text.contains("dev-main"), "{text}");
}

#[test]
fn servers_that_do_not_answer_are_listed_with_their_reasons() {
    let mut fixture = app();
    fixture.app.set_unreachable(vec![
        (
            "https://api1.hs.example.net/".to_owned(),
            "connection refused".to_owned(),
        ),
        ("https://api2.hs.example.net/".to_owned(), "timed out".to_owned()),
    ]);
    let text = screen(&render(&fixture.app, 80, 24));
    assert!(text.contains("No server answers"), "{text}");
    assert!(
        text.contains("api1.hs.example.net") && text.contains("connection refused"),
        "{text}"
    );
    assert!(text.contains("timed out"), "{text}");
}

#[test]
fn the_menu_opens_with_m_and_closes_with_esc() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Char('m'));
    let text = screen(&render(&fixture.app, 120, 36));
    for item in ["Check the servers", "Check this desktop", "Theme", "Help", "Quit"] {
        assert!(text.contains(item), "{item}: {text}");
    }
    press(&mut fixture.app, KeyCode::Esc);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(!text.contains("Check the servers"), "{text}");
}

#[test]
fn the_menu_asks_for_the_server_and_desktop_checks() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Char('m'));
    assert_eq!(press(&mut fixture.app, KeyCode::Enter), Some(Request::Transport));
    press(&mut fixture.app, KeyCode::Char('m'));
    press(&mut fixture.app, KeyCode::Down);
    assert_eq!(press(&mut fixture.app, KeyCode::Enter), Some(Request::Discover));
}

#[test]
fn a_theme_chosen_in_the_menu_is_drawn_and_saved() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Char('m'));
    for _ in 0..10 {
        if screen(&render(&fixture.app, 120, 36)).contains("▶ Theme") {
            break;
        }
        press(&mut fixture.app, KeyCode::Down);
    }
    press(&mut fixture.app, KeyCode::Enter);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("Tokyo Night") && text.contains("Gruvbox"), "{text}");
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Down);
    let buf = render(&fixture.app, 120, 36);
    assert_eq!(
        buf[(5, 30)].bg,
        ratcn::Theme::nord().background,
        "the theme is previewed"
    );
    press(&mut fixture.app, KeyCode::Enter);
    let saved = std::fs::read_to_string(fixture._dir.path().join("tui.toml")).unwrap();
    assert!(saved.contains("Nord"), "{saved}");
}

#[test]
fn esc_in_the_theme_list_puts_the_old_theme_back() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Char('m'));
    for _ in 0..10 {
        if screen(&render(&fixture.app, 120, 36)).contains("▶ Theme") {
            break;
        }
        press(&mut fixture.app, KeyCode::Down);
    }
    press(&mut fixture.app, KeyCode::Enter);
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Esc);
    let buf = render(&fixture.app, 120, 36);
    assert_eq!(buf[(5, 30)].bg, Palette::named("HomeStack", false).unwrap().background);
    assert!(!fixture._dir.path().join("tui.toml").exists());
}

#[test]
fn q_quits() {
    let mut fixture = app();
    assert_eq!(press(&mut fixture.app, KeyCode::Char('q')), Some(Request::Quit));
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(fixture.app.key(ctrl_c), Some(Request::Quit));
}

#[test]
fn the_log_panel_shows_the_newest_line_first() {
    let mut fixture = app();
    fixture.app.log("10:41:58", "status read from api1");
    fixture.app.log("10:47:01", "status read from api2");
    let text = screen(&render(&fixture.app, 120, 36));
    let newer = text.find("status read from api2").unwrap();
    let older = text.find("status read from api1").unwrap();
    assert!(newer < older, "{text}");
}

#[test]
fn plain_homestack_opens_the_tui_only_on_a_terminal() {
    use hs_cli::tui::wants_tui;
    assert!(wants_tui(&[], true, true));
    assert!(!wants_tui(&[], false, true));
    assert!(!wants_tui(&[], true, false));
    assert!(!wants_tui(&["--help".to_owned()], true, true));
    assert!(!wants_tui(&["status".to_owned()], true, true));
}

#[test]
fn zero_brings_back_the_standard_view() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Char('v'));
    press(&mut fixture.app, KeyCode::Char('2'));
    press(&mut fixture.app, KeyCode::Right);
    press(&mut fixture.app, KeyCode::Char('f'));
    press(&mut fixture.app, KeyCode::Char('x'));
    press(&mut fixture.app, KeyCode::Enter);
    press(&mut fixture.app, KeyCode::Char('0'));
    let text = screen(&render(&fixture.app, 120, 36));
    for part in [
        "┐²cluster┌",
        "┐³storage┌",
        "┐⁴log┌",
        "┐view auto┌",
        "‹ node: all ›",
        "┐filter┌",
        "dev-main",
    ] {
        assert!(text.contains(part), "{part}: {text}");
    }
    let saved = std::fs::read_to_string(fixture._dir.path().join("tui.toml")).unwrap();
    assert!(saved.contains("view = 0"), "{saved}");
}

#[test]
fn the_node_table_lines_up_under_its_headings() {
    let fixture = app();
    let buf = render(&fixture.app, 120, 36);
    let (state, heading_y) = position(&buf, "STATE      WORKSPACES");
    let (workspaces, _) = position(&buf, "WORKSPACES");
    let line = row(&buf, heading_y + 1);
    let dot = line.chars().position(|c| c == '●').unwrap();
    // The second dot is the running workspaces', under WORKSPACES.
    let count = line
        .chars()
        .enumerate()
        .filter(|(_, c)| *c == '●')
        .nth(1)
        .map(|(at, _)| at)
        .unwrap();
    assert_eq!(dot, usize::from(state), "{line}");
    assert_eq!(count, usize::from(workspaces), "{line}");
}

#[test]
fn a_newline_or_a_tab_in_a_text_is_drawn_as_a_space() {
    let mut fixture = app();
    fixture.app.log("10:00:00", "first line\nsecond\tline\r");
    fixture.app.set_unreachable(vec![(
        "https://api1.example.net/".to_owned(),
        "error sending request\ncaused by: refused".to_owned(),
    )]);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("first line second line"), "{text}");
}

#[test]
fn the_gold_line_of_the_servers_box_has_its_dot_first_and_fits() {
    let fixture = app();
    let text = screen(&render(&fixture.app, 120, 36));
    let line = text
        .lines()
        .find(|line| line.contains(" gold ○ 112 hs-gold@pve1"))
        .unwrap_or_else(|| panic!("no gold line:\n{text}"));
    assert!(line.contains("pve1"), "{line}");
}

#[test]
fn a_six_digit_vmid_is_shown_whole() {
    let mut fixture = app();
    let mut status = tui_fixture::status();
    status["workspaces"][0]["vmid"] = serde_json::json!(101_010);
    fixture.app.set_status(&status);
    for (width, height) in [(80, 24), (120, 36)] {
        let text = screen(&render(&fixture.app, width, height));
        assert!(text.contains("101010"), "{width}x{height}:\n{text}");
    }
}

#[test]
fn an_address_of_the_new_numbering_is_shown_whole() {
    let mut fixture = app();
    let mut status = tui_fixture::status();
    status["workspaces"][0]["ip"] = serde_json::json!("192.168.101.10");
    fixture.app.set_status(&status);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("192.168.101.10 "), "{text}");
}

#[test]
fn before_the_first_status_the_list_says_it_is_reading() {
    let dir = tempfile::tempdir().unwrap();
    let preferences = hs_cli::tui::theme::Preferences::load(&dir.path().join("tui.toml")).unwrap();
    let app = hs_cli::tui::state::App::new(preferences, false).unwrap();
    let text = screen(&render(&app, 120, 36));
    assert!(text.contains("Reading the status from the servers…  0.0 s"), "{text}");
    let mut later = app.clone();
    later.waiting = 12;
    let text = screen(&render(&later, 120, 36));
    assert!(
        text.contains("1.2 s") && text.contains("██████"),
        "a moving bar: {text}"
    );
}

#[test]
fn a_click_on_a_workspace_selects_it_and_the_wheel_moves_the_selection() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut fixture = app();
    let buf = render(&fixture.app, 120, 36);
    let (x, y) = position(&buf, "rust-builder");
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(fixture.app.mouse(click), Some(Request::Details(202)));
    let wheel = MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(fixture.app.mouse(wheel), Some(Request::Details(203)));
}

#[test]
fn the_node_table_counts_running_and_stopped_workspaces_with_dots() {
    let fixture = app();
    let text = screen(&render(&fixture.app, 120, 36));
    // pve2 holds dev-main, data-play (running), api-proto, game-dev (stopped).
    let line = text.lines().find(|line| line.contains(" pve2  ")).unwrap();
    assert!(line.contains("● 2") && line.contains("○ 2"), "{line}");
    assert!(!line.contains("running"), "{line}");
}

fn click(app: &mut hs_cli::tui::state::App, x: u16, y: u16) -> Option<Request> {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    app.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    })
}

#[test]
fn the_buttons_in_the_borders_and_the_menu_s_items_are_clicked() {
    let mut fixture = app();
    // ┐menu┌ in the border opens the menu.
    let buf = render(&fixture.app, 120, 36);
    let (x, y) = position(&buf, "menu");
    assert_eq!(click(&mut fixture.app, x, y), None);
    assert!(matches!(
        fixture.app.overlay,
        Some(hs_cli::tui::state::Overlay::Menu { .. })
    ));
    // Its item "Check the servers" asks the servers.
    let buf = render(&fixture.app, 120, 36);
    let (x, y) = position(&buf, "Check the servers");
    assert_eq!(click(&mut fixture.app, x, y), Some(Request::Transport));
    // A panel's digit hides it.
    fixture.app.overlay = None;
    let buf = render(&fixture.app, 120, 36);
    let (x, y) = position(&buf, "³storage");
    click(&mut fixture.app, x + 1, y);
    assert!(!fixture.app.shown.storage, "the storage panel is hidden");
}

#[test]
fn i_shows_and_hides_the_details_of_the_selected_workspace() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Down);
    assert!(!fixture.app.info);
    press(&mut fixture.app, KeyCode::Char('i'));
    assert!(fixture.app.info);
    press(&mut fixture.app, KeyCode::Char('i'));
    assert!(!fixture.app.info);
}

#[test]
fn a_view_saved_as_preset_before_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tui.toml");
    std::fs::write(&path, "preset = 2\n").unwrap();
    let preferences = hs_cli::tui::theme::Preferences::load(&path).unwrap();
    let app = hs_cli::tui::state::App::new(preferences, false).unwrap();
    assert_eq!(app.view, 2);
}

#[test]
fn storage_and_the_log_lie_side_by_side_under_the_whole_width_list() {
    let fixture = app();
    let buf = render(&fixture.app, 150, 40);
    let (list_x, list_y) = position(&buf, "¹workspaces");
    let (log_x, log_y) = position(&buf, "⁴log");
    let (storage_x, storage_y) = position(&buf, "³storage");
    assert_eq!(log_y, storage_y, "{}", screen(&buf));
    assert!(log_y > list_y && log_x < storage_x, "{}", screen(&buf));
    assert_eq!(log_x, list_x, "{}", screen(&buf));
    // The list's border runs the whole width.
    assert!(row(&buf, list_y).trim_end().ends_with('╮'), "{}", row(&buf, list_y));
    assert_eq!(row(&buf, list_y).trim_end().chars().count(), 150);
}

#[test]
fn the_log_scrolls_back_by_its_scrollbar_and_the_wheel() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut fixture = app();
    for minute in 0..30 {
        fixture
            .app
            .log(&format!("10:{minute:02}:00"), &format!("line {minute}"));
    }
    let event = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let buf = render(&fixture.app, 150, 40);
    let text = screen(&buf);
    assert!(text.contains("line 29") && !text.contains("line 0 "), "{text}");
    // The wheel over the log goes back three lines.
    let (x, y) = position(&buf, "line 29");
    assert_eq!(fixture.app.mouse(event(MouseEventKind::ScrollDown, x, y)), None);
    assert_eq!(fixture.app.log_scroll, 3);
    let text = screen(&render(&fixture.app, 150, 40));
    assert!(!text.contains("line 29") && text.contains("line 26"), "{text}");
    // A new line keeps the lines read in place.
    fixture.app.log("10:30:00", "line 30");
    assert_eq!(fixture.app.log_scroll, 4);
    // ↓ of the scrollbar goes back a line.
    let buf = render(&fixture.app, 150, 40);
    let (bar_x, down_y) = position(&buf, "↓");
    fixture
        .app
        .mouse(event(MouseEventKind::Down(MouseButton::Left), bar_x, down_y));
    assert_eq!(fixture.app.log_scroll, 5);
    // The thumb held and dragged down, the pointer off the bar, to the oldest.
    let buf = render(&fixture.app, 150, 40);
    let thumb_y = (0..40).find(|y| buf[(bar_x, *y)].symbol() == "█").unwrap();
    fixture
        .app
        .mouse(event(MouseEventKind::Down(MouseButton::Left), bar_x, thumb_y));
    assert_eq!(fixture.app.log_scroll, 5, "holding the thumb does not move it");
    fixture
        .app
        .mouse(event(MouseEventKind::Drag(MouseButton::Left), bar_x - 20, 39));
    let text = screen(&render(&fixture.app, 150, 40));
    assert!(text.contains("line 0 "), "{text}");
    // Let go, a drag moves nothing.
    fixture
        .app
        .mouse(event(MouseEventKind::Up(MouseButton::Left), bar_x - 20, 39));
    let held = fixture.app.log_scroll;
    fixture
        .app
        .mouse(event(MouseEventKind::Drag(MouseButton::Left), bar_x, down_y - 3));
    assert_eq!(fixture.app.log_scroll, held);
    // The wheel back up stops at the newest line.
    for _ in 0..20 {
        fixture.app.mouse(event(MouseEventKind::ScrollUp, x, y));
    }
    assert_eq!(fixture.app.log_scroll, 0);
}

#[test]
fn the_list_s_scrollbar_is_held_and_dragged_and_the_selection_follows() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut fixture = app();
    fixture.app.set_details(201, &details());
    press(&mut fixture.app, KeyCode::Char('i'));
    let event = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let buf = render(&fixture.app, 80, 24);
    let thumb_y = (0..24).find(|y| buf[(79, *y)].symbol() == "█").unwrap();
    let (_, down_y) = (0..24)
        .find(|y| buf[(79, *y)].symbol() == "↓")
        .map(|y| (79, y))
        .unwrap();
    fixture
        .app
        .mouse(event(MouseEventKind::Down(MouseButton::Left), 79, thumb_y));
    let last = fixture.app.rows().last().unwrap().vmid;
    let request = fixture
        .app
        .mouse(event(MouseEventKind::Drag(MouseButton::Left), 70, down_y));
    assert_eq!(request, Some(Request::Details(last)));
    assert_eq!(fixture.app.selected, Some(last));
    // Back up to the top: the selection moves to the last row then shown.
    fixture.app.mouse(event(MouseEventKind::Drag(MouseButton::Left), 70, 0));
    let text = screen(&render(&fixture.app, 80, 24));
    let first = fixture.app.rows().first().unwrap().vmid.to_string();
    assert!(text.contains(&format!(" {first} ")), "{text}");
    assert_ne!(fixture.app.selected, Some(last));
    fixture.app.mouse(event(MouseEventKind::Up(MouseButton::Left), 70, 0));
    assert!(fixture.app.dragging.is_none());
}

fn mouse_at(
    app: &mut hs_cli::tui::state::App,
    kind: crossterm::event::MouseEventKind,
    column: u16,
    row: u16,
) -> Option<Request> {
    app.mouse(crossterm::event::MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

#[test]
fn a_log_line_is_highlighted_by_a_click_and_the_wheel_moves_the_highlight() {
    use crossterm::event::{MouseButton, MouseEventKind};
    let mut fixture = app();
    for minute in 0..30 {
        fixture
            .app
            .log(&format!("10:{minute:02}:00"), &format!("line {minute}"));
    }
    let buf = render(&fixture.app, 150, 40);
    let (x, y) = position(&buf, "line 27");
    mouse_at(&mut fixture.app, MouseEventKind::Down(MouseButton::Left), x, y);
    assert_eq!(fixture.app.log_cursor, Some(2));
    let buf = render(&fixture.app, 150, 40);
    assert!(screen(&buf).contains("3/30"), "{}", screen(&buf));
    let (_, highlighted) = position(&buf, "line 27");
    assert_eq!(buf[(x, highlighted)].bg, fixture.app.palette.focus);
    // The wheel moves the highlight a line; the view follows it down.
    for _ in 0..10 {
        mouse_at(&mut fixture.app, MouseEventKind::ScrollDown, x, y);
    }
    assert_eq!(fixture.app.log_cursor, Some(12));
    let buf = render(&fixture.app, 150, 40);
    let (_, line_17) = position(&buf, "line 17 ");
    assert_eq!(buf[(x, line_17)].bg, fixture.app.palette.focus);
    assert!(fixture.app.log_scroll > 0);
    // The scrollbar dragged to the newest takes the highlight with the view.
    let (bar_x, up_y) = position(&buf, "↑");
    let thumb_y = (0..40).find(|row| buf[(bar_x, *row)].symbol() == "█").unwrap();
    mouse_at(
        &mut fixture.app,
        MouseEventKind::Down(MouseButton::Left),
        bar_x,
        thumb_y,
    );
    mouse_at(
        &mut fixture.app,
        MouseEventKind::Drag(MouseButton::Left),
        bar_x,
        up_y - 5,
    );
    mouse_at(&mut fixture.app, MouseEventKind::Up(MouseButton::Left), bar_x, up_y - 5);
    assert_eq!(fixture.app.log_scroll, 0);
    let cursor = fixture.app.log_cursor.unwrap();
    assert!(cursor < 6, "the highlight is on a line shown: {cursor}");
    // A new line keeps the highlight on its line.
    fixture.app.log("10:30:00", "line 30");
    assert_eq!(fixture.app.log_cursor, Some(cursor + 1));
}

#[test]
fn new_lines_above_a_log_scrolled_back_star_its_top_right_corner_until_read() {
    use crossterm::event::MouseEventKind;
    let mut fixture = app();
    for minute in 0..30 {
        fixture
            .app
            .log(&format!("10:{minute:02}:00"), &format!("line {minute}"));
    }
    let star = |app: &hs_cli::tui::state::App| {
        let buf = render(app, 150, 40);
        let (_, log_y) = position(&buf, "⁴log");
        row(&buf, log_y).contains("┐*┌─╮")
    };
    // At the newest line a new line is seen: no star.
    fixture.app.log("10:30:00", "line 30");
    assert!(!star(&fixture.app));
    // Scrolled back, a new line does not move the view and stars the corner.
    let buf = render(&fixture.app, 150, 40);
    let (x, y) = position(&buf, "line 30");
    mouse_at(&mut fixture.app, MouseEventKind::ScrollDown, x, y);
    assert!(!star(&fixture.app));
    let before = screen(&render(&fixture.app, 150, 40));
    fixture.app.log("10:31:00", "line 31");
    let text = screen(&render(&fixture.app, 150, 40));
    assert!(!text.contains("line 31") && text.contains("line 27"), "{text}");
    assert_eq!(before.contains("line 28"), text.contains("line 28"));
    assert!(star(&fixture.app), "{text}");
    // Scrolled part of the way up it stays; at the newest line it goes.
    mouse_at(&mut fixture.app, MouseEventKind::ScrollUp, x, y);
    assert_eq!(fixture.app.log_scroll, 1);
    assert!(star(&fixture.app));
    for _ in 0..3 {
        mouse_at(&mut fixture.app, MouseEventKind::ScrollUp, x, y);
    }
    assert_eq!(fixture.app.log_scroll, 0);
    assert!(!star(&fixture.app));
    // Scrolled back again, the lines read do not star it.
    mouse_at(&mut fixture.app, MouseEventKind::ScrollDown, x, y);
    assert!(!star(&fixture.app));
}
