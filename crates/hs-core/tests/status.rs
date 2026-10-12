//! Ports of the status tests in `python/tests/test_status.py`,
//! `test_alien_workspace.py` and `test_lifecycle.py` (target resolution).

mod common;

use std::collections::BTreeSet;

use common::{FakePve, block_on, test_config};
use hs_core::status::{
    StatusResource, global_status, orphaned_volumes, resolve_existing_workspace, resolve_workspace_target,
    status_resources, storage_capacities, workspace_status,
};
use hs_model::status::NodeState;
use serde_json::{Value, json};

fn node(name: &str, online: bool) -> NodeState {
    NodeState {
        node: name.into(),
        status: if online { "online" } else { "offline" }.into(),
        online,
    }
}

fn volids(volumes: &[hs_model::status::OrphanVolume]) -> Vec<&str> {
    volumes.iter().map(|v| v.volid.as_str()).collect()
}

/// `OrphanedVolumeTests::test_orphan_scan_reports_only_unattached_managed_volumes`
#[test]
fn orphan_scan_reports_only_unattached_managed_volumes() {
    let pve = FakePve::new()
        .with(
            "GET /nodes/example-node-1/storage",
            json!([
                {"storage": "example-storage-a", "content": "images", "enabled": 1, "active": 1},
                {"storage": "example-storage-b", "content": "images", "enabled": 1, "active": 1},
                {"storage": "local", "content": "iso,vztmpl", "enabled": 1, "active": 1},
            ]),
        )
        .with(
            "GET /nodes/example-node-1/storage/example-storage-a/content?content=images",
            json!([
                {"volid": "example-storage-a:vm-200-hs-root-default", "size": 16},
                {"volid": "example-storage-a:vm-200-hs-home-user", "size": 20},
                {"volid": "example-storage-a:vm-201-hs-home-user", "size": 20},
                {"volid": "example-storage-a:vm-201-disk-0", "size": 16},
            ]),
        );
    let attached: BTreeSet<String> = [
        "example-storage-a:vm-200-hs-root-default",
        "example-storage-a:vm-200-hs-home-user",
    ]
    .iter()
    .map(|v| (*v).to_owned())
    .collect();
    let (orphans, warnings, complete) = block_on(orphaned_volumes(
        &pve,
        &test_config(),
        &attached,
        &[node("example-node-1", true)],
    ));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(complete);
    assert_eq!(volids(&orphans), ["example-storage-a:vm-201-hs-home-user"]);
    assert_eq!(orphans[0].size_bytes, Some(20));
    assert_eq!(
        (orphans[0].storage.as_str(), orphans[0].node.as_str()),
        ("example-storage-a", "example-node-1")
    );
}

/// `OrphanedVolumeTests::test_orphan_scan_ignores_unassigned_image_storages`
#[test]
fn orphan_scan_ignores_unassigned_image_storages() {
    let pve = FakePve::new()
        .with(
            "GET /nodes/example-node-1/storage",
            json!([
                {"storage": "example-storage-b", "content": "images", "enabled": 1, "active": 1},
                {"storage": "example-storage-a", "content": "images", "enabled": 1, "active": 1},
            ]),
        )
        .with(
            "GET /nodes/example-node-1/storage/example-storage-a/content?content=images",
            json!([]),
        );
    let (orphans, warnings, complete) = block_on(orphaned_volumes(
        &pve,
        &test_config(),
        &BTreeSet::new(),
        &[node("example-node-1", true)],
    ));
    assert!(orphans.is_empty() && warnings.is_empty() && complete);
    assert!(
        !pve.calls()
            .iter()
            .any(|call| call.contains("example-storage-b/content"))
    );
}

/// `OrphanedVolumeTests::test_orphan_scan_warns_when_assigned_storage_is_missing`
#[test]
fn orphan_scan_warns_when_assigned_storage_is_missing() {
    let pve = FakePve::new().with(
        "GET /nodes/example-node-1/storage",
        json!([{"storage": "example-storage-b", "content": "images", "enabled": 1, "active": 1}]),
    );
    let (orphans, warnings, complete) = block_on(orphaned_volumes(
        &pve,
        &test_config(),
        &BTreeSet::new(),
        &[node("example-node-1", true)],
    ));
    assert!(orphans.is_empty());
    assert_eq!(
        warnings,
        ["Assigned HomeStack storage 'example-storage-a' is unavailable for image scanning on example-node-1"]
    );
    assert!(!complete);
}

/// `OrphanedVolumeTests::test_orphan_scan_skips_offline_nodes_without_warning`
#[test]
fn orphan_scan_skips_offline_nodes_without_warning() {
    let pve = FakePve::new()
        .with(
            "GET /nodes/example-node-1/storage",
            json!([{"storage": "example-storage-a", "content": "images", "enabled": 1, "active": 1}]),
        )
        .with(
            "GET /nodes/example-node-1/storage/example-storage-a/content?content=images",
            json!([]),
        );
    let (orphans, warnings, complete) = block_on(orphaned_volumes(
        &pve,
        &test_config(),
        &BTreeSet::new(),
        &[node("example-node-1", true), node("example-node-3", false)],
    ));
    assert!(orphans.is_empty() && warnings.is_empty());
    assert!(!complete);
    assert!(
        !pve.calls()
            .iter()
            .any(|call| call.contains("/nodes/example-node-3/storage"))
    );
}

/// `ManagedStatusResourceTests::test_global_status_inspects_only_gold_and_tagged_workspaces`
#[test]
fn global_status_inspects_only_gold_and_tagged_workspaces() {
    let resource = |vmid: u32, tags: &str| StatusResource {
        vmid,
        node: "example-node-1".into(),
        name: None,
        status: None,
        tags: Some(tags.into()),
    };
    let resources = [
        resource(100, ""),
        resource(101, "homestack-gold"),
        resource(102, "other"),
        resource(200, "homestack-ws"),
        resource(201, "backup;homestack-ws"),
    ];
    let managed: Vec<u32> = status_resources(&resources, &test_config())
        .iter()
        .map(|r| r.vmid)
        .collect();
    assert_eq!(managed, [101, 200, 201]);
}

/// `ManagedStatusResourceTests::test_global_status_marks_orphan_scan_incomplete_for_offline_storage_node`
#[test]
fn global_status_marks_orphan_scan_incomplete_for_offline_storage_node() {
    let pve = FakePve::new()
        .with("GET /cluster/resources?type=vm", json!([]))
        .with(
            "GET /nodes",
            json!([{"node": "example-node-1", "status": "online"}, {"node": "example-node-3", "status": "offline"}]),
        )
        .with(
            "GET /nodes/example-node-1/storage",
            json!([{"storage": "example-storage-a", "content": "images", "enabled": 1, "active": 1}]),
        )
        .with(
            "GET /nodes/example-node-1/storage/example-storage-a/content?content=images",
            json!([]),
        )
        .with(
            "GET /nodes/example-node-1/storage/example-storage-a/status",
            json!({"total": 1000, "used": 250, "avail": 750}),
        );
    let status = block_on(global_status(&pve, &test_config())).ok();
    let status = status.as_ref();
    assert_eq!(status.map(|s| s.summary.orphan_scan_complete), Some(false));
    assert!(
        !pve.calls()
            .iter()
            .any(|call| call.contains("/nodes/example-node-3/storage"))
    );
    assert!(status.is_some_and(|s| {
        !s.warnings
            .iter()
            .any(|w| w.to_lowercase().contains("orphan scan incomplete"))
    }));
}

/// `StorageCapacityStatusTests::test_reads_capacity_for_only_assigned_homestack_storages`
#[test]
fn reads_capacity_for_only_assigned_homestack_storages() {
    let pve = FakePve::new()
        .with(
            "GET /nodes/example-node-1/storage/example-storage-a/status",
            json!({"total": 1000, "used": 250, "avail": 750}),
        )
        .with(
            "GET /nodes/example-node-2/storage/example-storage-b/status",
            json!({"total": 2000, "used": 500, "avail": 1500}),
        )
        .with(
            "GET /nodes/example-node-3/storage/example-storage-b/status",
            json!({"total": 3000, "used": 600, "avail": 2400}),
        );
    let nodes = [
        node("example-node-1", true),
        node("example-node-2", true),
        node("example-node-3", true),
    ];
    let (entries, warnings) = block_on(storage_capacities(&pve, &test_config(), &nodes));
    assert!(warnings.is_empty());
    let keys: Vec<(&str, &str, &str)> = entries
        .iter()
        .map(|e| (e.layout.as_str(), e.node.as_str(), e.storage.as_str()))
        .collect();
    assert_eq!(
        keys,
        [
            ("homestack-storage-1", "example-node-1", "example-storage-a"),
            ("homestack-storage-2", "example-node-2", "example-storage-b"),
            ("homestack-storage-3", "example-node-3", "example-storage-b"),
        ]
    );
    assert_eq!(
        (entries[1].used_bytes, entries[1].available_bytes),
        (Some(500), Some(1500))
    );
    assert_eq!(entries[0].percent, Some(25.0));
}

/// `StorageCapacityStatusTests::test_offline_node_capacity_is_reported_without_remote_call`
#[test]
fn offline_node_capacity_is_reported_without_remote_call() {
    let pve = FakePve::new().with(
        "GET /nodes/example-node-1/storage/example-storage-a/status",
        json!({"total": 1000, "used": 250, "avail": 750}),
    );
    let nodes = [node("example-node-1", true), node("example-node-3", false)];
    let (entries, warnings) = block_on(storage_capacities(&pve, &test_config(), &nodes));
    assert!(warnings.is_empty());
    let offline = entries.iter().find(|e| e.node == "example-node-3");
    assert_eq!(
        offline.map(|e| (e.node_status.as_str(), e.ok, e.total_bytes)),
        Some(("offline", false, None))
    );
    assert!(
        !pve.calls()
            .iter()
            .any(|call| call.contains("/nodes/example-node-3/storage"))
    );
}

fn existing_workspace_pve(config: Value) -> FakePve {
    FakePve::new()
        .with(
            "GET /cluster/resources?type=vm",
            json!([{"type": "qemu", "vmid": 200, "node": "example-node-1", "status": "stopped"}]),
        )
        .with("GET /nodes/example-node-1/qemu/200/config", config)
}

/// `VmVolumeInventoryTests::test_workspace_rejects_root_and_home_pointing_to_same_volume`
#[test]
fn workspace_rejects_root_and_home_pointing_to_same_volume() {
    let shared = "example-storage-a:vm-200-hs-home-user";
    let pve = existing_workspace_pve(json!({
        "name": "test1",
        "tags": "homestack-ws",
        "scsi0": format!("{shared},size=20G"),
        "scsi1": format!("{shared},serial=HS_HOME_200,size=20G"),
    }));
    let error = block_on(resolve_existing_workspace(&pve, &test_config(), 200))
        .err()
        .map(|e| e.to_string());
    assert!(error.as_deref().is_some_and(|e| e.contains("same volume")), "{error:?}");
    assert!(
        !pve.calls().iter().any(|call| call.contains("/agent/")),
        "home usage is not read"
    );
}

/// `VmVolumeInventoryTests::test_workspace_allows_virtiofs_devices`
#[test]
fn workspace_allows_virtiofs_devices() {
    let pve = existing_workspace_pve(json!({
        "name": "test1",
        "tags": "homestack-ws",
        "scsi0": "example-storage-a:vm-200-hs-root-default,size=16G",
        "scsi1": "example-storage-a:vm-200-hs-home-user,serial=HS_HOME_200,size=20G",
        "virtiofs0": "user-managed-mapping",
    }));
    let info = block_on(resolve_existing_workspace(&pve, &test_config(), 200)).ok();
    assert_eq!(
        info.map(|i| i.vm_config["virtiofs0"].clone()).as_deref(),
        Some("user-managed-mapping")
    );
}

/// `WorkspaceTargetTests::test_numeric_target_is_vmid`
#[test]
fn numeric_target_is_vmid() {
    let pve = FakePve::new();
    assert_eq!(
        block_on(resolve_workspace_target(&pve, &test_config(), "200")).ok(),
        Some(200)
    );
    assert!(pve.calls().is_empty());
}

/// `WorkspaceTargetTests::test_workspace_name_resolves_to_tagged_vmid`
#[test]
fn workspace_name_resolves_to_tagged_vmid() {
    let pve = FakePve::new()
        .with(
            "GET /cluster/resources?type=vm",
            json!([{"type": "qemu", "vmid": 200, "node": "example-node-1", "name": "test1"}]),
        )
        .with(
            "GET /nodes/example-node-1/qemu/200/config",
            json!({"name": "test1", "tags": "homestack-ws"}),
        );
    assert_eq!(
        block_on(resolve_workspace_target(&pve, &test_config(), "test1")).ok(),
        Some(200)
    );
}

/// `RoleTagTests::test_name_resolution_discovers_alien_workspace`
#[test]
fn name_resolution_discovers_alien_workspace() {
    let pve = FakePve::new()
        .with(
            "GET /cluster/resources?type=vm",
            json!([{"type": "qemu", "vmid": 200, "node": "example-node-1", "name": "alien-workspace"}]),
        )
        .with(
            "GET /nodes/example-node-1/qemu/200/config",
            json!({"name": "alien-workspace", "tags": "homestack-alien"}),
        );
    assert_eq!(
        block_on(resolve_workspace_target(&pve, &test_config(), "alien-workspace")).ok(),
        Some(200)
    );
}

#[test]
fn target_errors_match_python() {
    let pve = FakePve::new()
        .with(
            "GET /cluster/resources?type=vm",
            json!([
                {"type": "qemu", "vmid": 200, "node": "n1", "name": "dup"},
                {"type": "qemu", "vmid": 201, "node": "n1", "name": "dup"},
                {"type": "qemu", "vmid": 202, "node": "n1", "name": "plain"},
            ]),
        )
        .with("GET /nodes/n1/qemu/200/config", json!({"tags": "homestack-ws"}))
        .with("GET /nodes/n1/qemu/201/config", json!({"tags": "homestack-ws"}))
        .with("GET /nodes/n1/qemu/202/config", json!({}));
    let error = |target: &str| {
        block_on(resolve_workspace_target(&pve, &test_config(), target))
            .err()
            .map(|e| e.to_string())
    };
    assert_eq!(
        error("dup").as_deref(),
        Some("Workspace name 'dup' is ambiguous; matching VMIDs: 200, 201")
    );
    assert_eq!(
        error("plain").as_deref(),
        Some("VM name 'plain' exists but is not a HomeStack workspace (VMID 202).")
    );
    assert_eq!(
        error("none").as_deref(),
        Some("No HomeStack workspace named 'none' exists.")
    );
    assert!(error("1bad").is_some_and(|e| e.starts_with("Invalid workspace target")));
}

/// `StatusAlienTests::test_global_status_counts_only_native_homes_and_skips_alien_disk_warnings`
#[test]
fn global_status_counts_only_native_homes_and_skips_alien_disk_warnings() {
    let pve = FakePve::new()
        .with(
            "GET /cluster/resources?type=vm",
            json!([
                {"vmid": 101, "type": "qemu", "node": "example-node-1", "status": "stopped", "name": "gold"},
                {"vmid": 200, "type": "qemu", "node": "example-node-1", "status": "running", "name": "native", "tags": "homestack-ws"},
                {"vmid": 201, "type": "qemu", "node": "example-node-1", "status": "running", "name": "alien", "tags": "homestack-alien"},
            ]),
        )
        .with("GET /nodes", json!([{"node": "example-node-1", "status": "online"}]))
        .with("GET /nodes/example-node-1/qemu/101/config", json!({"name": "gold", "tags": "homestack-gold", "scsi0": "example-storage-a:vm-101-root,size=16G"}))
        .with(
            "GET /nodes/example-node-1/qemu/200/config",
            json!({"name": "native", "tags": "homestack-ws", "scsi0": "example-storage-a:vm-200-root,size=16G", "scsi1": "example-storage-a:vm-200-home,serial=HS_HOME_200,size=20G"}),
        )
        .with(
            "GET /nodes/example-node-1/qemu/201/config",
            json!({"name": "alien", "tags": "homestack-alien", "scsi0": "example-storage-a:vm-201-root,size=16G", "scsi1": "example-storage-a:vm-201-home,size=20G"}),
        )
        .with("POST /nodes/example-node-1/qemu/200/agent/ping", json!({"result": {}}))
        .with(
            "GET /nodes/example-node-1/qemu/200/agent/get-fsinfo",
            json!({"result": [{"mountpoint": "/home/user", "type": "ext4", "name": "sdb", "used-bytes": 40, "total-bytes": 100, "total-bytes-privileged": 100,
                "disk": [{"serial": "0QEMU_QEMU_HARDDISK_drive-scsi1", "dev": "/dev/sdb"}]}]}),
        )
        .with("GET /nodes/example-node-1/storage", json!([]))
        .with("GET /nodes/example-node-1/storage/example-storage-a/status", json!({"total": 1000, "used": 250, "avail": 750}));
    let status = block_on(global_status(&pve, &test_config()));
    let status = match status {
        Ok(status) => status,
        Err(error) => panic!("{error}"),
    };
    let roles: Vec<&str> = status.workspaces.iter().map(|w| w.role.as_str()).collect();
    assert_eq!(roles, ["WS", "ALIEN"]);
    assert!(status.workspaces[1].home.is_none());
    let homes = &status.summary.homes;
    assert_eq!(
        (homes.quota_bytes, homes.used_bytes, homes.free_bytes),
        (100, Some(40), Some(60))
    );
    assert_eq!((homes.free_percent, homes.missing_count), (Some(60.0), 0));
    let fsinfo_calls = pve
        .calls()
        .iter()
        .filter(|call| call.ends_with("/agent/get-fsinfo"))
        .count();
    assert_eq!(fsinfo_calls, 1, "only the native workspace's home is read");
    assert!(!status.warnings.iter().any(|w| w.contains("scsi1")));
}

/// `StatusAlienTests::test_per_vm_alien_status_has_no_home_identity_or_home_volume_role`
#[test]
fn per_vm_alien_status_has_no_home_identity_or_home_volume_role() {
    let pve = FakePve::new()
        .with(
            "GET /cluster/resources?type=vm",
            json!([{"vmid": 201, "type": "qemu", "node": "example-node-1", "status": "running"}]),
        )
        .with(
            "GET /nodes/example-node-1/qemu/201/config",
            json!({"name": "alien", "tags": "homestack-alien", "scsi0": "example-storage-a:vm-201-root,size=16G", "scsi1": "example-storage-a:vm-201-home,size=20G"}),
        )
        .failing("POST /nodes/example-node-1/qemu/201/agent/ping", 500, "QEMU guest agent is not running\n");
    let status = match block_on(workspace_status(&pve, &test_config(), 201)) {
        Ok(Some(status)) => status,
        Ok(None) => panic!("VM 201 not found"),
        Err(error) => panic!("{error}"),
    };
    assert_eq!(status.role.as_deref(), Some("ALIEN"));
    assert_eq!(
        (
            status.home_label,
            status.home_identity_ok,
            status.home_mount_ok,
            status.home_disk
        ),
        (None, None, None, None)
    );
    assert!(status.volumes.iter().all(|v| v.role == "disk"));
    assert!(!status.qga);
}
