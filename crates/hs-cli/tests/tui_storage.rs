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
    let storages = json!([
        {"storage": "NFS1", "storage_type": "nfs", "content": "images,backup,iso,snippets", "shared": true,
         "nodes": ["pve1", "pve2", "pve3"], "available": true,
         "capacity": {"name": "NFS1", "total_bytes": 100, "used_bytes": 85, "available_bytes": 15, "percent": 85.0}}
    ]);
    status["summary"]["homes"]["missing_count"] = json!(2);
    status["summary"]["orphaned_volumes"] = json!([
        {"volid": "local:vm-299-hs-home-user", "vmid": 299, "role": "home", "name": "user", "size_bytes": 1073741824, "storage": "local", "node": "pve1"}
    ]);
    fixture.app.storage_overview = Some(Ok(serde_json::from_value(json!({
        "storages": storages,
        "latest_backups": [{"vmid": 201, "created_at": 1, "storage": "NFS1"}],
        "backup_scan_complete": true, "warnings": []
    }))
    .unwrap()));
    fixture.app.status = Some(serde_json::from_value(status).unwrap());
    let compact = screen(&render(&fixture.app, 120, 36));
    assert!(!compact.contains("STORAGE  1 total"));
    for view in ['2', '3'] {
        press(&mut fixture.app, KeyCode::Char(view));
        let buf = render(&fixture.app, 160, 60);
        let text = screen(&buf);
        assert_eq!(text.matches("pve1,pve2,pve3").count(), 1, "{text}");
        assert!(text.contains("SHARED"), "{text}");
        for expected in [
            "backup, images, iso, snippets",
            "missing 2",
            "local:vm-299-hs-home-user",
            "BACKUPS  1/13 guests",
            "dev-main",
            "No backup found:",
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
    fixture.app.storage_overview = Some(Ok(serde_json::from_value(json!({
        "storages": [], "latest_backups": [], "backup_scan_complete": true, "warnings": []
    }))
    .unwrap()));
    let mut status = tui_fixture::status();
    status["summary"]["orphaned_volumes"] = json!((280..300).map(|vmid| json!({
        "volid": format!("local:vm-{vmid}-hs-home-user"), "vmid": vmid, "role": "home", "name": "user", "size_bytes": 1073741824_u64, "storage": "local", "node": "pve1"
    })).collect::<Vec<_>>());
    fixture.app.status = Some(serde_json::from_value(status).unwrap());
    press(&mut fixture.app, KeyCode::Char('2'));
    let _ = render(&fixture.app, 80, 24);
    press(&mut fixture.app, KeyCode::End);
    assert!(fixture.app.storage_scroll.get() > 0);
    let text = screen(&render(&fixture.app, 80, 24));
    assert!(text.contains("No backup found"), "{text}");
    press(&mut fixture.app, KeyCode::Home);
    assert_eq!(fixture.app.storage_scroll.get(), 0);
    assert!(screen(&render(&fixture.app, 80, 24)).contains("STORAGE"));
}

#[test]
fn basic_status_starts_without_extended_storage_and_storage_failure_is_local() {
    let mut fixture = app();
    fixture.app.status = None;
    let status = tui_fixture::status();
    assert!(status["summary"].get("storages").is_none());
    fixture.app.set_status(&status);
    assert!(fixture.app.status.is_some());
    fixture.app.storage_overview = Some(Err("HTTP 404: endpoint unavailable".to_owned()));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("dev-main"), "{text}");
    assert!(!text.contains("endpoint unavailable"), "{text}");
    press(&mut fixture.app, KeyCode::Char('3'));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("Storage inventory unavailable"), "{text}");
    assert!(text.contains("/HOME"), "{text}");
    assert!(!text.contains("No backup found"), "{text}");
}

#[test]
fn reordering_storage_content_nodes_and_rows_does_not_change_the_view() {
    let mut fixture = app();
    let mut first = json!({"storages": [
        {"storage": "NFS1", "storage_type": "nfs", "content": "images,backup,iso,snippets", "shared": true,
         "nodes": ["pve1", "pve2"], "available": true,
         "capacity": {"name": "NFS1", "total_bytes": 100, "used_bytes": 85, "available_bytes": 15, "percent": 85.0}},
        {"storage": "local", "storage_type": "dir", "content": "vztmpl,rootdir,iso", "shared": false,
         "nodes": ["pve1"], "available": true,
         "capacity": {"name": "local", "total_bytes": 100, "used_bytes": 50, "available_bytes": 50, "percent": 50.0}}
    ], "latest_backups": [], "backup_scan_complete": true, "warnings": []});
    assert!(
        fixture
            .app
            .set_storage_overview(Ok(serde_json::from_value(first.clone()).unwrap()))
    );
    press(&mut fixture.app, KeyCode::Char('3'));
    let before = render(&fixture.app, 80, 36);
    first["storages"][0]["content"] = json!("snippets,iso,backup,images");
    first["storages"][0]["nodes"] = json!(["pve2", "pve1"]);
    first["storages"].as_array_mut().unwrap().reverse();
    assert!(
        !fixture
            .app
            .set_storage_overview(Ok(serde_json::from_value(first).unwrap()))
    );
    assert_eq!(before, render(&fixture.app, 80, 36));
}

#[test]
fn twelve_storages_fit_on_one_row_each_and_missing_backups_are_grouped() {
    let mut fixture = app();
    let storages: Vec<_> = (0..12)
        .map(|index| {
            let name = if index == 0 {
                "backup-goodram120".to_owned()
            } else {
                format!("storage-{index:02}")
            };
            json!({"storage": name, "storage_type": "dir", "content": "vztmpl,snippets,backup,iso,rootdir,images",
            "shared": false, "nodes": ["pve1"], "available": true,
            "capacity": {"name": name, "total_bytes": 100, "used_bytes": 90, "available_bytes": 10, "percent": 90.0}})
        })
        .collect();
    fixture
        .app
        .set_storage_overview(Ok(serde_json::from_value(json!({"storages": storages,
        "latest_backups": [], "backup_scan_complete": true, "warnings": []}))
        .unwrap()));
    press(&mut fixture.app, KeyCode::Char('3'));
    let buf = render(&fixture.app, 80, 36);
    let text = screen(&buf);
    assert_eq!(text.matches("No backup found").count(), 1, "{text}");
    let (_, first_y) = position(&buf, "backup-goodram120");
    let (type_x, _) = position(&buf, "TYPE");
    for index in 1..12 {
        let (_, y) = position(&buf, &format!("storage-{index:02}"));
        assert_eq!(y, first_y + index);
        assert_eq!(buf[(type_x, y)].symbol(), "d");
    }
    assert!(text.contains("BACKUPS  0/13 guests"), "{text}");
    assert_eq!(fixture.app.storage_scroll_max.get(), 0);
}
