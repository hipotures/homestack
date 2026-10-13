//! Ports of the terminal views of `python/src/homestack/ui.py`: tests of
//! `test_status.py`, `test_lifecycle.py` and `test_cli.py` about what the
//! human output shows.

use hs_cli::view::{self, Color, Style};
use serde_json::json;

const GIB: u64 = 1024 * 1024 * 1024;

fn plain() -> Style {
    Style {
        color: false,
        width: 120,
    }
}

/// Port of `FreePercentDisplayTests::test_free_percent_uses_capacity_risk_thresholds`.
#[test]
fn free_percent_uses_capacity_risk_thresholds() {
    assert_eq!(view::free_percent(Some(100), Some(40)), Some(40.0));
    assert_eq!(view::free_percent_color(Some(30.0)), Color::Green);
    assert_eq!(view::free_percent_color(Some(29.9)), Color::Yellow);
    assert_eq!(view::free_percent_color(Some(10.0)), Color::Yellow);
    assert_eq!(view::free_percent_color(Some(9.9)), Color::Red);
}

/// Port of `FreePercentDisplayTests::test_free_percent_text_formats_value`.
#[test]
fn free_percent_text_formats_value() {
    assert_eq!(view::free_percent_text(Some(99.0), 1).text, "99.0%");
    assert_eq!(view::free_percent_text(None, 1).text, "—");
}

/// Port of `StorageDisplayFormatTests::test_fixed_storage_unit_and_precision`.
#[test]
fn fixed_storage_unit_and_precision() {
    let value = json!(7 * 1024 * GIB / 4);
    assert_eq!(view::byte_value_in_unit(&value, "GiB", 0), "1792");
    assert_eq!(view::byte_value_in_unit(&value, "TiB", 2), "1.75");
    assert_eq!(view::byte_value_in_unit(&value, "TiB", 3), "1.750");
}

/// Port of `GlobalStorageTableTests::test_storage_table_replaces_total_with_free_percent`.
#[test]
fn storage_table_replaces_total_with_free_percent() {
    let result = json!({"summary": {"storage_unit": "GiB", "storage_decimals": 0, "storage_layouts": [
        {"layout": "homestack-storage-2", "node": "example-node-2", "storage": "example-storage-b",
         "total_bytes": 100 * GIB, "used_bytes": 75 * GIB, "available_bytes": 25 * GIB}]}});
    let table = view::build_global_storage_table(&result, false);
    assert_eq!(
        table.map(|t| t.headers),
        Some(
            ["LAYOUT", "NODE", "STATE", "STORAGE", "USED GiB", "FREE GiB", "FREE %"]
                .map(str::to_owned)
                .to_vec()
        )
    );
}

/// Port of `GlobalStorageTableTests::test_offline_node_state_is_visible`.
#[test]
fn offline_node_state_is_visible() {
    let result = json!({"summary": {"storage_unit": "GiB", "storage_decimals": 0, "storage_layouts": [
        {"layout": "homestack-storage-3", "node": "example-node-3", "node_status": "offline",
         "storage": "example-storage-b", "total_bytes": null, "used_bytes": null, "available_bytes": null}]}});
    let rendered = view::build_global_storage_table(&result, false).map(|t| t.render(&plain()));
    assert!(rendered.is_some_and(|text| text.contains("offline")));
}

fn summary_text(result: &serde_json::Value) -> String {
    view::global_summary_lines(result, false).join("\n")
}

/// Port of `GlobalSummaryTests::test_cluster_node_states_are_summarized`.
#[test]
fn cluster_node_states_are_summarized() {
    let result = json!({"summary": {"workspace_count": 0, "running": 0, "stopped": 0,
        "nodes": [{"node": "example-node-1", "status": "online"}, {"node": "example-node-3", "status": "offline"}],
        "homes": {"used_bytes": null, "quota_bytes": 0, "free_percent": null, "missing_count": 0}}});
    let text = summary_text(&result);
    assert!(text.contains("Cluster nodes"));
    assert!(text.contains("example-node-3 offline"));
}

/// Port of `GlobalSummaryTests::test_zero_workspaces_keeps_storage_in_separate_table_data`.
#[test]
fn zero_workspaces_keeps_storage_in_separate_table_data() {
    let result = json!({"summary": {"workspace_count": 0, "running": 0, "stopped": 0,
        "storage_layouts": [{"layout": "homestack-storage-2", "node": "example-node-2", "storage": "example-storage-b",
            "total_bytes": 2048 * GIB, "used_bytes": 512 * GIB, "available_bytes": 1536 * GIB}],
        "homes": {"used_bytes": null, "quota_bytes": 0, "free_percent": null, "missing_count": 0}}});
    assert!(!summary_text(&result).contains("homestack-storage-2"));
    let rows = view::global_storage_rows(&result);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["layout"], "homestack-storage-2");
    assert_eq!(rows[0]["node"], "example-node-2");
    assert_eq!(rows[0]["storage"], "example-storage-b");
    assert_eq!(rows[0]["used_bytes"], 512 * GIB);
    assert_eq!(rows[0]["available_bytes"], 1536 * GIB);
}

/// Port of `GlobalSummaryTests::test_zero_workspaces_hides_home_metrics`.
#[test]
fn zero_workspaces_hides_home_metrics() {
    let result = json!({"summary": {"workspace_count": 0, "running": 0, "stopped": 0,
        "homes": {"used_bytes": null, "quota_bytes": 0, "free_percent": null, "missing_count": 0}}});
    let text = summary_text(&result);
    assert!(text.contains("Workspaces"));
    assert!(text.contains("0 (0 running / 0 stopped)"));
    assert!(!text.contains("Persistent homes"));
    assert!(!text.contains("Home filesystem free"));
}

/// Port of `GlobalSummaryTests::test_zero_workspaces_shows_detached_volume_count_when_present`.
#[test]
fn zero_workspaces_shows_detached_volume_count_when_present() {
    let result = json!({"summary": {"workspace_count": 0, "running": 0, "stopped": 0,
        "homes": {"used_bytes": null, "quota_bytes": 0, "free_percent": null, "missing_count": 0},
        "orphaned_volumes": [{"volid": "example-storage-a:vm-200-hs-home-user"}]}});
    let text = summary_text(&result);
    assert!(text.contains("Workspaces"));
    assert!(text.contains("Detached HS volumes"));
    assert!(text.contains('1'));
}

/// Port of `MigrationPlanDisplayTests::test_format_volume_lines_accepts_migration_size_bytes`.
#[test]
fn format_volume_lines_accepts_migration_size_bytes() {
    let lines = view::format_volume_lines(&[
        json!({"slot": "scsi0", "role": "root", "volume": "example-storage-b:vm-200-hs-root-default", "size_bytes": 16 * GIB}),
        json!({"slot": "scsi1", "role": "home", "volume": "example-storage-b:vm-200-hs-home-user", "size_bytes": 20 * GIB}),
    ]);
    assert_eq!(lines.len(), 3);
    assert!(lines[1].contains("16.0 GiB"));
    assert!(lines[2].contains("20.0 GiB"));
}

/// Port of `DestroyPlanPresentationTests::test_gib_value_is_compact` (`test_lifecycle.py`).
#[test]
fn gib_value_is_compact() {
    assert_eq!(view::gib_value(&json!(16.0)), "16 GiB");
    assert_eq!(view::gib_value(&json!(20.5)), "20.5 GiB");
    assert_eq!(view::gib_value(&json!(null)), "—");
}

/// Port of `DestroyPlanPresentationTests::test_destroy_result_shows_local_ssh_config_and_known_hosts_cleanup`.
#[test]
fn destroy_result_shows_local_ssh_config_and_known_hosts_cleanup() {
    let result = json!({"vmid": 210, "name": "test210", "vm_deleted": true, "home_deleted": true,
        "ssh_config_removed": ["/tmp/vm210-test210.conf", "/tmp/vm210-oldname.conf"],
        "ssh_known_hosts_removed": ["192.0.2.210"]});
    let rendered = view::destroy_result(&result, &plain());
    assert!(rendered.contains("Local SSH config"));
    assert!(rendered.contains("2 removed"));
    assert!(rendered.contains("Local known_hosts"));
    assert!(rendered.contains("1 removed"));
}

/// Port of `CreatePlanRenderingTests::test_create_plan_shows_target_node` (`test_cli.py`).
#[test]
fn create_plan_shows_target_node() {
    let plan = json!({"vmid": 210, "name": "test210", "node": "example-node-2", "source_node": "example-node-1",
        "transfer_method": "clone-migrate", "gold_vmid": 101, "root_disk_gb": 16.0, "ip": "192.0.2.210",
        "cidr": 24, "gateway": "192.0.2.1", "root_storage": "example-storage-b", "home_disk": "scsi1",
        "home_storage": "example-storage-b", "home_size": "20G", "home_label": "HS_HOME_210",
        "home_volume_name": "vm-210-hs-home-user", "ssh_public_keys": ["example hardware-backed key"],
        "ssh_key_source": "trusted desktop", "user": "user", "uid": 1000, "gid": 1000, "pool": "homestack"});
    let rendered = view::create_plan(&plan, &plain());
    assert!(rendered.contains("Node"));
    assert!(rendered.contains("example-node-2"));
    assert!(rendered.contains("example-node-1"));
    assert!(rendered.contains("Clone on Gold's node, then migrate"));
}

/// Port of `ParserTests::test_error_panel_preserves_bracketed_config_section` (`test_cli.py`).
#[test]
fn error_panel_preserves_bracketed_config_section() {
    let rendered = view::error("Missing configuration value [transport] type", &plain());
    assert!(rendered.contains("[transport]"));
    assert!(rendered.contains("ERROR"));
}
