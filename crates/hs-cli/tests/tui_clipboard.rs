#![allow(clippy::unwrap_used)]

mod tui_fixture;

use crossterm::event::KeyCode;
use hs_cli::tui::state::Request;
use ratatui::layout::Rect;
use serde_json::json;
use tui_fixture::{app, details, press, status};

#[test]
fn f3_copies_cached_original_responses_instead_of_serializing_display_models() {
    let mut fixture = app();
    let area = Rect::new(0, 0, 80, 24);
    let mut original = status();
    original["unknown_field"] = json!({"preserve": [null, "żółć", 42]});
    fixture.app.set_status(&original);
    assert_eq!(press(&mut fixture.app, KeyCode::F(3)), Some(Request::CopyJson));
    assert_eq!(fixture.app.displayed_json(area), Some(original.clone()));

    let mut detail = details();
    detail["unknown_detail"] = json!(true);
    fixture.app.selected = Some(201);
    fixture.app.set_details(201, Ok(detail.clone()));
    fixture.app.info = true;
    let copied = fixture.app.displayed_json(area).unwrap();
    assert_eq!(copied["/v1/status"], original);
    assert_eq!(copied["/v1/workspaces/201"], detail);

    fixture.app.info = false;
    assert_eq!(fixture.app.displayed_json(area), Some(original.clone()));
    original["unknown_field"] = json!("updated response");
    fixture.app.set_status(&original);
    assert_eq!(fixture.app.displayed_json(area), Some(original));
    fixture.app.after_installed(true);
    assert_eq!(fixture.app.displayed_json(area), None);
}

#[test]
fn storage_json_keeps_unknown_fields_and_original_inventory_order() {
    let mut fixture = app();
    let area = Rect::new(0, 0, 120, 36);
    let original = json!({
        "storages": [], "latest_backups": [], "backup_scan_complete": true,
        "warnings": ["z", "a", "z"], "unknown_storage_field": 123
    });
    fixture.app.set_storage_json(Ok(original.clone()));
    press(&mut fixture.app, KeyCode::Char('3'));
    let copied = fixture.app.displayed_json(area).unwrap();
    assert_eq!(copied["/v1/storage/overview"], original);
    assert_eq!(
        fixture
            .app
            .storage_overview
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .warnings,
        ["a", "z"]
    );
    fixture.app.set_storage_json(Err("unavailable".to_owned()));
    assert!(
        fixture
            .app
            .displayed_json(area)
            .unwrap()
            .get("/v1/storage/overview")
            .is_none()
    );
}

#[test]
fn logs_are_included_only_when_the_panel_fits_and_are_limited_to_the_latest_100() {
    let mut fixture = app();
    for index in 0..150 {
        fixture.app.log("12:00:00", &format!("entry {index}"));
    }
    let small = Rect::new(0, 0, 80, 24);
    assert!(fixture.app.displayed_json(small).unwrap().get("logs").is_none());
    let large = Rect::new(0, 0, 160, 45);
    let copied = fixture.app.displayed_json(large).unwrap();
    let logs = copied["logs"].as_array().unwrap();
    assert_eq!(logs.len(), 100);
    assert_eq!(logs[0]["text"], "entry 50");
    assert_eq!(logs[99]["text"], "entry 149");
    assert_eq!(fixture.app.lines.len(), 150);
    press(&mut fixture.app, KeyCode::Char('4'));
    let copied = fixture.app.displayed_json(small).unwrap();
    assert!(copied.get("/v1/status").is_none());
    assert_eq!(copied["logs"].as_array().unwrap().len(), 100);
    press(&mut fixture.app, KeyCode::Char('m'));
    assert_eq!(press(&mut fixture.app, KeyCode::F(3)), Some(Request::CopyJson));
}
