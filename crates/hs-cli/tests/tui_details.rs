//! Detail reads follow visible info or an explicit action, never navigation alone.
#![allow(clippy::unwrap_used)]

mod tui_fixture;

use crossterm::event::KeyCode;
use hs_cli::tui::{
    draw::render,
    shell,
    state::{Overlay, Request},
};
use serde_json::json;
use tui_fixture::{app, details, press, screen};

#[test]
fn info_reuses_details_until_they_are_invalidated() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Down);
    assert_eq!(fixture.app.details_target(), None);
    press(&mut fixture.app, KeyCode::Char('i'));
    assert_eq!(fixture.app.details_target(), Some((201, true)));
    fixture.app.set_details(201, Ok(details()));
    assert_eq!(fixture.app.details_target(), None);
    press(&mut fixture.app, KeyCode::Char('i'));
    press(&mut fixture.app, KeyCode::Char('i'));
    assert_eq!(fixture.app.details_target(), None);
    fixture.app.clear_details();
    assert_eq!(fixture.app.details_target(), Some((201, true)));
}

#[test]
fn navigation_with_info_open_requests_only_the_current_vm() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Char('i'));
    press(&mut fixture.app, KeyCode::Down);
    assert_eq!(fixture.app.details_target(), Some((202, true)));
    fixture.app.set_details(201, Err("obsolete error".to_owned()));
    assert!(fixture.app.details_error().is_none());
    assert!(fixture.app.selected_details().is_none());
    assert!(fixture.app.lines.is_empty());
    press(&mut fixture.app, KeyCode::Char('i'));
    assert_eq!(fixture.app.details_target(), None);
}

#[test]
fn info_reads_pause_under_an_action_and_resume_when_it_is_hidden() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Char('i'));
    press(&mut fixture.app, KeyCode::Char('t'));
    assert_eq!(fixture.app.details_target(), None);
    fixture.app.progress(hs_cli::tui::action::Progress::Steps(Vec::new()));
    press(&mut fixture.app, KeyCode::Esc);
    assert_eq!(fixture.app.details_target(), Some((201, true)));
}

#[test]
fn outside_pools_and_containers_show_basic_info_without_a_detail_read() {
    for (pool, role) in [(None, "WS"), (Some("homestack-test"), "WS"), (Some("homestack"), "CT")] {
        let mut fixture = app();
        let mut status = tui_fixture::status();
        status["workspaces"][0]["pool"] = json!(pool);
        status["workspaces"][0]["role"] = json!(role);
        fixture.app.set_status(&status);
        press(&mut fixture.app, KeyCode::Down);
        press(&mut fixture.app, KeyCode::Char('i'));
        assert_eq!(fixture.app.details_target(), None);
        let text = screen(&render(&fixture.app, 160, 45));
        assert!(text.contains("10.0.0.21") && text.contains("unavailable"), "{text}");
        assert!(!text.contains("reading the details"), "{text}");
        assert!(fixture.app.lines.is_empty());
    }
}

#[test]
fn pool_names_come_from_the_server_and_gold_can_also_be_read() {
    let mut fixture = app();
    fixture.app.pool = Some("team".to_owned());
    fixture.app.gold_pool = Some("base-images".to_owned());
    let mut status = tui_fixture::status();
    status["gold"]["pool"] = json!("base-images");
    status["workspaces"][0]["pool"] = json!("team");
    fixture.app.set_status(&status);
    press(&mut fixture.app, KeyCode::Char('i'));
    assert_eq!(fixture.app.details_target(), Some((112, true)));
    press(&mut fixture.app, KeyCode::Down);
    assert_eq!(fixture.app.details_target(), Some((201, true)));
}

#[test]
fn a_detail_error_is_shown_in_info_and_cached_without_log_spam() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Char('i'));
    fixture.app.set_details(201, Err("guest agent timed out".to_owned()));
    assert_eq!(fixture.app.details_target(), None);
    assert!(screen(&render(&fixture.app, 160, 45)).contains("guest agent timed out"));
    assert!(fixture.app.lines.is_empty());
}

#[test]
fn resize_loads_current_disk_sizes_without_opening_info() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Char('z'));
    assert!(!fixture.app.info);
    assert_eq!(fixture.app.details_target(), Some((201, false)));
    assert!(fixture.app.action.is_none());
    let mut details = details();
    details["volumes"] = json!([
        {"slot": "scsi0", "role": "root", "volume": "local-lvm:vm-201-disk-0", "storage": "local-lvm", "size": "40G"},
        {"slot": "scsi1", "role": "home", "volume": "local-lvm:vm-201-disk-1", "storage": "local-lvm", "size": "80G"}
    ]);
    fixture.app.set_details(201, Ok(details));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("‹ 80G ›"), "{text}");
    press(&mut fixture.app, KeyCode::Right);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("‹ root ›") && text.contains("‹ 40G ›"), "{text}");
}

#[test]
fn cancelling_a_detail_read_cannot_open_a_resize_form_later() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Char('z'));
    assert_eq!(press(&mut fixture.app, KeyCode::Esc), None);
    assert_eq!(fixture.app.details_target(), None);
    assert_eq!(fixture.app.set_details(201, Ok(details())), None);
    assert!(fixture.app.action.is_none());
    assert!(fixture.app.overlay.is_none());
}

#[test]
fn resize_read_failure_shows_the_error_instead_of_a_default_size() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Char('z'));
    fixture.app.set_details(201, Err("cannot read disks".to_owned()));
    assert!(matches!(fixture.app.overlay, Some(Overlay::Report { .. })));
    assert!(fixture.app.action.is_none());
    assert!(screen(&render(&fixture.app, 120, 36)).contains("cannot read disks"));
}

#[test]
fn ssh_loads_its_target_without_opening_info() {
    let mut fixture = app();
    let home = tempfile::tempdir().unwrap();
    press(&mut fixture.app, KeyCode::Down);
    assert_eq!(shell::prepare(&mut fixture.app, home.path(), 201), None);
    assert_eq!(fixture.app.details_target(), Some((201, false)));
    assert_eq!(fixture.app.set_details(201, Ok(details())), Some(Request::Ssh(201)));
    assert_eq!(
        shell::prepare(&mut fixture.app, home.path(), 201).as_deref(),
        Some("dev-main")
    );
    assert!(!fixture.app.info);
    assert_eq!(fixture.app.details_target(), None);
}

#[test]
fn a_local_ssh_entry_needs_no_detail_read() {
    let mut fixture = app();
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join(".ssh/config.d/homestack");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("vm201-dev.conf"), "Host dev vm201\n").unwrap();
    press(&mut fixture.app, KeyCode::Down);
    assert_eq!(
        shell::prepare(&mut fixture.app, home.path(), 201).as_deref(),
        Some("vm201")
    );
    assert_eq!(fixture.app.details_target(), None);
    assert!(fixture.app.overlay.is_none());
}

#[test]
fn an_action_waits_for_capabilities_before_reading_details() {
    let mut fixture = app();
    fixture.app.pool = None;
    fixture.app.gold_pool = None;
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Char('z'));
    assert_eq!(fixture.app.details_target(), None);
    fixture.app.pool = Some("homestack".to_owned());
    fixture.app.gold_pool = Some("homestack-gold".to_owned());
    assert_eq!(fixture.app.details_target(), Some((201, false)));
}
