//! The status, details and app the TUI tests draw (phase 13b).
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hs_cli::tui::state::{App, Request};
use hs_cli::tui::theme::Preferences;
use ratatui::buffer::Buffer;
use serde_json::{Value, json};

const GIB: u64 = 1 << 30;

pub fn workspace(vmid: u32, name: &str, node: &str, running: bool, home_gib: u64, free: f64) -> Value {
    json!({
        "vmid": vmid, "role": "WS", "name": name,
        "pool": "homestack",
        "status": if running { "running" } else { "stopped" },
        "node": node, "node_status": "online",
        "ip": if running { json!(format!("10.0.0.{}", vmid - 180)) } else { Value::Null },
        "root_storage": "local-lvm",
        "home": {"size_bytes": home_gib * GIB, "used_bytes": null, "free_bytes": null,
                 "free_percent": if running { json!(free) } else { Value::Null }},
        "role_tag_ok": true, "warning": null
    })
}

pub fn status() -> Value {
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
                 "role_tag_ok": true, "warning": null, "pool": "homestack-gold"},
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

pub fn details() -> Value {
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

pub struct Fixture {
    pub app: App,
    pub _dir: tempfile::TempDir,
}

pub fn app() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let preferences = Preferences::load(&dir.path().join("tui.toml")).unwrap();
    let mut app = App::new(preferences, false).unwrap();
    app.set_status(&status());
    app.pool = Some("homestack".to_owned());
    app.gold_pool = Some("homestack-gold".to_owned());
    app.set_servers(vec![
        ("https://api1.hs.example.net/".to_owned(), None),
        ("https://api2.hs.example.net/".to_owned(), None),
        ("https://api3.hs.example.net/".to_owned(), None),
    ]);
    Fixture { app, _dir: dir }
}

pub fn press(app: &mut App, code: KeyCode) -> Option<Request> {
    app.key(KeyEvent::new(code, KeyModifiers::NONE))
}

pub fn row(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_owned()).collect()
}

pub fn screen(buf: &Buffer) -> String {
    (0..buf.area.height).map(|y| row(buf, y)).collect::<Vec<_>>().join("\n")
}

pub fn position(buf: &Buffer, text: &str) -> (u16, u16) {
    for y in 0..buf.area.height {
        let line = row(buf, y);
        if let Some(at) = line.find(text) {
            return (u16::try_from(line[..at].chars().count()).unwrap(), y);
        }
    }
    panic!("{text:?} not on the screen:\n{}", screen(buf));
}
