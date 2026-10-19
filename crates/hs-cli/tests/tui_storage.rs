#![allow(clippy::unwrap_used, clippy::expect_used)]

mod tui_fixture;

use crossterm::event::KeyCode;
use hs_cli::tui::draw::render;
use serde_json::json;
use tui_fixture::{app, position, press, screen};

#[test]
fn dedicated_views_show_storage_details_and_default_stays_compact() {
    let mut fixture = app();
    let mut status = tui_fixture::status();
    status["summary"]["storages"] = json!([
        {"storage": "NFS1", "storage_type": "nfs", "content": "images,backup,iso,snippets", "shared": true,
         "nodes": ["pve1", "pve2", "pve3"], "available": true,
         "capacity": {"name": "NFS1", "total_bytes": 100, "used_bytes": 85, "available_bytes": 15, "percent": 85.0}}
    ]);
    status["summary"]["homes"]["missing_count"] = json!(2);
    status["summary"]["orphaned_volumes"] = json!([
        {"volid": "local:vm-299-hs-home-user", "vmid": 299, "role": "home", "name": "user", "size_bytes": 1073741824, "storage": "local", "node": "pve1"}
    ]);
    status["summary"]["latest_backups"] = json!([{"vmid": 201, "created_at": 1, "storage": "NFS1"}]);
    fixture.app.status = Some(serde_json::from_value(status).unwrap());
    let compact = screen(&render(&fixture.app, 120, 36));
    assert!(!compact.contains("[shared]"));
    for view in ['2', '3'] {
        press(&mut fixture.app, KeyCode::Char(view));
        let buf = render(&fixture.app, 160, 60);
        let text = screen(&buf);
        assert_eq!(text.matches("[shared]").count(), 1, "{text}");
        for expected in [
            "images,backup,iso,snippets",
            "missing 2",
            "local:vm-299-hs-home-user",
            "Latest guest backups",
            "ago  NFS1",
            "no backup found",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
        let (x, y) = position(&buf, "85%");
        assert_eq!(buf[(x, y)].fg, fixture.app.palette.error);
    }
}

#[test]
fn storage_scrolling_reaches_last_guest_and_home_returns_to_inventory() {
    let mut fixture = app();
    let mut status = tui_fixture::status();
    status["summary"]["orphaned_volumes"] = json!([
        {"volid": "local:vm-299-hs-home-user", "vmid": 299, "role": "home", "name": "user", "size_bytes": 1073741824, "storage": "local", "node": "pve1"}
    ]);
    fixture.app.status = Some(serde_json::from_value(status).unwrap());
    press(&mut fixture.app, KeyCode::Char('2'));
    let _ = render(&fixture.app, 80, 24);
    press(&mut fixture.app, KeyCode::End);
    assert!(fixture.app.storage_scroll.get() > 0);
    let text = screen(&render(&fixture.app, 80, 24));
    assert!(text.contains("no backup found"), "{text}");
    press(&mut fixture.app, KeyCode::Home);
    assert_eq!(fixture.app.storage_scroll.get(), 0);
    assert!(screen(&render(&fixture.app, 80, 24)).contains("Storage inventory"));
}
