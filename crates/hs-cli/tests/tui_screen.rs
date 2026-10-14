//! The main screen of the TUI (phase 13b, step 1; mockups 09–11, 15–17):
//! panels from the server's status, selection, details, filter, menu,
//! themes and the size notice. Drawn into a ratatui buffer, as MyPowers
//! tests its screens.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hs_cli::tui::draw::render;
use hs_cli::tui::state::{App, Request};
use hs_cli::tui::theme::{Palette, Preferences};
use ratatui::buffer::Buffer;
use serde_json::{Value, json};

const GIB: u64 = 1 << 30;

fn workspace(vmid: u32, name: &str, node: &str, running: bool, home_gib: u64, free: f64) -> Value {
    json!({
        "vmid": vmid, "role": "WS", "name": name,
        "status": if running { "running" } else { "stopped" },
        "node": node, "node_status": "online",
        "ip": if running { json!(format!("10.0.0.{}", vmid - 180)) } else { Value::Null },
        "root_storage": "local-lvm",
        "home": {"size_bytes": home_gib * GIB, "used_bytes": null, "free_bytes": null,
                 "free_percent": if running { json!(free) } else { Value::Null }},
        "role_tag_ok": true, "warning": null
    })
}

fn status() -> Value {
    let rows = [
        (201, "dev-main", "pve2", true, 64, 41.0),
        (202, "rust-builder", "pve3", true, 128, 77.0),
        (203, "web-lab", "pve1", false, 32, 0.0),
        (204, "data-play", "pve2", true, 64, 12.0),
        (205, "scratch", "pve1", false, 16, 0.0),
        (206, "ml-lab", "pve3", true, 256, 63.0),
        (207, "docs-site", "pve1", true, 16, 88.0),
        (208, "api-proto", "pve2", false, 32, 0.0),
        (209, "db-test", "pve3", true, 64, 35.0),
        (210, "ci-runner", "pve1", true, 32, 54.0),
        (211, "game-dev", "pve2", false, 128, 0.0),
        (212, "notes", "pve3", true, 8, 90.0),
    ];
    let workspaces: Vec<Value> = rows
        .iter()
        .map(|(vmid, name, node, running, home, free)| workspace(*vmid, name, node, *running, *home, *free))
        .collect();
    let layout = |node: &str, storage: &str, used: u64, total: u64| {
        json!({"layout": format!("homestack-storage-{node}"), "node": node, "storage": storage,
               "node_status": "online", "ok": true, "name": storage,
               "total_bytes": total * GIB, "used_bytes": used * GIB,
               "available_bytes": (total - used) * GIB, "percent": 100.0 * used as f64 / total as f64})
    };
    json!({
        "gold": {"vmid": 112, "role": "GOLD", "name": "hs-gold", "status": "stopped", "node": "pve1",
                 "node_status": "online", "ip": null, "root_storage": "local-lvm", "home": null,
                 "role_tag_ok": true, "warning": null},
        "workspaces": workspaces,
        "summary": {
            "workspace_count": 12, "running": 8, "stopped": 4,
            "root_storage": {"name": "local-lvm", "total_bytes": null, "used_bytes": null,
                             "available_bytes": null, "percent": null},
            "storage_layouts": [
                layout("pve1", "local-zfs", 380, 2100),
                layout("pve2", "local-lvm", 546, 1754),
                layout("pve3", "local-lvm", 650, 3573),
            ],
            "nodes": [
                {"node": "pve1", "status": "online", "online": true},
                {"node": "pve2", "status": "online", "online": true},
                {"node": "pve3", "status": "online", "online": true},
            ],
            "homes": {"used_bytes": null, "quota_bytes": 0, "free_bytes": null, "free_percent": null,
                      "physical_available_bytes": null, "missing_count": 0},
            "orphaned_volumes": [], "orphan_scan_complete": false
        },
        "warnings": []
    })
}

fn details() -> Value {
    json!({
        "vmid": 201, "name": "dev-main", "role": "WS", "tags": ["homestack-ws"], "role_tag_ok": true,
        "role_warning": null, "status": "running", "node": "pve2", "configured_ip": "10.0.0.21",
        "actual_hostname": "dev-main", "actual_ip": "10.0.0.21/24", "qga": true,
        "root_storage": "local-lvm", "volumes": [], "home_disk": "local-lvm:vm-201-disk-1",
        "home_storage": "local-lvm", "home_size_gb": 64.0, "home_label": "HS_HOME_201",
        "home_serial": "HS_HOME_201", "home_identity_ok": true,
        "home_mount": "/dev/sdb ext4 /home/user", "home_mount_ok": true,
        "ssh": {"root": "ssh root@dev-main", "user": "ssh dev-main"}
    })
}

struct Fixture {
    app: App,
    _dir: tempfile::TempDir,
}

fn app() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let preferences = Preferences::load(&dir.path().join("tui.toml")).unwrap();
    let mut app = App::new(preferences, false).unwrap();
    app.set_status(&status());
    app.set_servers(vec![
        ("https://api1.hs.example.net/".to_owned(), None),
        ("https://api2.hs.example.net/".to_owned(), None),
        ("https://api3.hs.example.net/".to_owned(), None),
    ]);
    Fixture { app, _dir: dir }
}

fn press(app: &mut App, code: KeyCode) -> Option<Request> {
    app.key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn row(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_owned()).collect()
}

fn screen(buf: &Buffer) -> String {
    (0..buf.area.height).map(|y| row(buf, y)).collect::<Vec<_>>().join("\n")
}

fn position(buf: &Buffer, text: &str) -> (u16, u16) {
    for y in 0..buf.area.height {
        let line = row(buf, y);
        if let Some(at) = line.find(text) {
            return (u16::try_from(line[..at].chars().count()).unwrap(), y);
        }
    }
    panic!("{text:?} not on the screen:\n{}", screen(buf));
}

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
    assert!(line.contains("gold 112"), "{line}");
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
