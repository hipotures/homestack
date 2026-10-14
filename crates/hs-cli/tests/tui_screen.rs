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
    assert!(row(&buf, 0).starts_with("╭─┐²cluster┌┐menu┌┐preset auto┌"), "{text}");
    assert!(row(&buf, 3).starts_with("╭─┐¹workspaces┌┐filter┌"), "{text}");
    assert!(!text.contains("³storage") && !text.contains("⁴log"), "{text}");
    for name in ["dev-main", "rust-builder", "notes", "pve1", "10.0.0.21"] {
        assert!(text.contains(name), "{name}: {text}");
    }
    // Nothing is selected yet: the counter says so and the actions are muted.
    assert!(row(&buf, 23).contains("┘0/12└"), "{text}");
    let palette = Palette::named("HomeStack", false).unwrap();
    let (x, y) = position(&buf, "refresh");
    assert_eq!(buf[(x, y)].fg, palette.muted);
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
fn moving_down_selects_the_first_workspace_and_lights_the_counter() {
    let mut fixture = app();
    let request = press(&mut fixture.app, KeyCode::Down);
    assert_eq!(request, Some(Request::Details(201)));
    let buf = render(&fixture.app, 80, 24);
    assert!(row(&buf, 23).contains("┘1/12└"), "{}", screen(&buf));
    let palette = Palette::named("HomeStack", false).unwrap();
    let (x, y) = position(&buf, "dev-main");
    assert_eq!(buf[(x, y)].bg, palette.selection);
    let (x, y) = position(&buf, "rust-builder");
    assert_ne!(buf[(x, y)].bg, palette.selection);
}

#[test]
fn info_at_80_columns_splits_the_list_and_shows_its_scrollbar() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Down);
    fixture.app.set_details(201, &details());
    press(&mut fixture.app, KeyCode::Enter);
    let buf = render(&fixture.app, 80, 24);
    let text = screen(&buf);
    assert!(text.contains("┐201 dev-main┌"), "{text}");
    assert!(text.contains("ssh dev-main"), "{text}");
    let right: String = (0..24).map(|y| buf[(79, y)].symbol().to_owned()).collect();
    assert!(
        right.contains('↑') && right.contains('↓') && right.contains('█'),
        "{right}\n{text}"
    );
    press(&mut fixture.app, KeyCode::Enter);
    let text = screen(&render(&fixture.app, 80, 24));
    assert!(!text.contains("┐201 dev-main┌"), "{text}");
}

#[test]
fn at_120_by_36_all_panels_and_the_details_box_are_drawn() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Down);
    fixture.app.set_details(201, &details());
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
    assert!(top.starts_with("╭─┐¹workspaces┌┐menu┌┐preset auto┌┐filter┌"), "{top}");
    press(&mut fixture.app, KeyCode::Char('2'));
    let buf = render(&fixture.app, 120, 36);
    assert!(row(&buf, 0).starts_with("╭─┐²cluster┌"));
}

#[test]
fn p_cycles_the_presets() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Char('p'));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("┐preset 1┌") && !text.contains("³storage"), "{text}");
    press(&mut fixture.app, KeyCode::Char('p'));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("┐preset 2┌") && !text.contains("²cluster"), "{text}");
    press(&mut fixture.app, KeyCode::Char('p'));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("┐preset auto┌") && text.contains("⁴log"), "{text}");
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
    assert!(text.contains("┘0/2└"), "{text}");
    press(&mut fixture.app, KeyCode::Esc);
    let text = screen(&render(&fixture.app, 80, 24));
    assert!(text.contains("dev-main") && text.contains("┘0/12└"), "{text}");
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
    press(&mut fixture.app, KeyCode::Char('p'));
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
        "┐preset auto┌",
        "‹ node: all ›",
        "┐filter┌",
        "dev-main",
    ] {
        assert!(text.contains(part), "{part}: {text}");
    }
    let saved = std::fs::read_to_string(fixture._dir.path().join("tui.toml")).unwrap();
    assert!(saved.contains("preset = 0"), "{saved}");
}

#[test]
fn the_node_table_lines_up_under_its_headings() {
    let fixture = app();
    let buf = render(&fixture.app, 120, 36);
    let (state, heading_y) = position(&buf, "STATE      WORKSPACES");
    let (workspaces, _) = position(&buf, "WORKSPACES");
    let line = row(&buf, heading_y + 1);
    let dot = line.chars().position(|c| c == '●').unwrap();
    let count = line.chars().position(|c| c == '4').unwrap();
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
        .find(|line| line.contains(" gold ○ 112 hs-gold"))
        .unwrap_or_else(|| panic!("no gold line:\n{text}"));
    assert!(line.contains("pve1"), "{line}");
}
