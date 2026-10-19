//! Ports of the read helpers' tests in `python/tests/test_proxmox.py` and
//! `test_status.py`.

mod common;

use std::collections::BTreeSet;

use common::{FakePve, block_on, test_config};
use hs_core::inventory::{
    VmConfig, attached_disk_volumes, cluster_node_statuses, has_tag, layout_name_for_node, node_dns, node_network,
    parse_tags, resolve_homestack_storage, storage_ids_for_node, vm_config, vm_status, vm_volume_inventory,
    volume_identity, workspace_tags,
};
use hs_model::status::{NodeState, VolumeEntry};
use serde_json::json;

fn vm(pairs: &[(&str, &str)]) -> VmConfig {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
}

/// `ControlNodeTests::test_cluster_node_statuses_preserve_online_state`
#[test]
fn cluster_node_statuses_preserve_online_state() {
    let pve = FakePve::new().with(
        "GET /nodes",
        json!([{"node": "example-node-2", "status": "offline"}, {"node": "example-node-1", "status": "online"}]),
    );
    let nodes = block_on(cluster_node_statuses(&pve)).ok();
    assert_eq!(
        nodes,
        Some(vec![
            NodeState {
                node: "example-node-1".into(),
                status: "online".into(),
                online: true,
                ..NodeState::default()
            },
            NodeState {
                node: "example-node-2".into(),
                status: "offline".into(),
                online: false,
                ..NodeState::default()
            },
        ])
    );
    assert_eq!(pve.calls(), ["GET /nodes"]);
}

/// `NetworkInventoryTests::test_node_network_and_dns_use_read_only_pvesh_get`
#[test]
fn node_network_and_dns_use_read_only_get() {
    let pve = FakePve::new()
        .with(
            "GET /nodes/example-node-1/network",
            json!([{"iface": "vmbr0", "type": "bridge"}, {"type": "eth"}]),
        )
        .with("GET /nodes/example-node-1/dns", json!({"dns1": "192.0.2.1"}));
    let network = block_on(node_network(&pve, "example-node-1")).unwrap_or_default();
    assert_eq!(network.len(), 1, "entries without iface are dropped");
    assert_eq!(network[0]["iface"], "vmbr0");
    let dns = block_on(node_dns(&pve, "example-node-1")).unwrap_or_default();
    assert_eq!(dns["dns1"], "192.0.2.1");
    assert!(pve.calls().iter().all(|call| call.starts_with("GET ")));
}

/// `StorageLayoutTests::test_first_storage_is_default_and_override_must_be_assigned`
#[test]
fn first_storage_is_default_and_override_must_be_assigned() {
    let cfg = test_config();
    assert_eq!(
        resolve_homestack_storage(&cfg, "example-node-1", None).ok().as_deref(),
        Some("example-storage-a")
    );
    assert_eq!(
        resolve_homestack_storage(&cfg, "example-node-2", None).ok().as_deref(),
        Some("example-storage-b")
    );
    assert_eq!(
        resolve_homestack_storage(&cfg, "example-node-2", Some("example-storage-b"))
            .ok()
            .as_deref(),
        Some("example-storage-b")
    );
    assert!(resolve_homestack_storage(&cfg, "example-node-2", Some("disallowed-storage")).is_err());
    assert!(resolve_homestack_storage(&cfg, "other", None).is_err());
}

/// `TagTests::test_exact_semicolon_separated_tags`
#[test]
fn exact_semicolon_separated_tags() {
    let expected: BTreeSet<String> = ["homestack-gold", "backup", "homestack-ws"]
        .iter()
        .map(|t| (*t).to_owned())
        .collect();
    assert_eq!(parse_tags(Some(" homestack-gold ; backup ; homestack-ws ")), expected);
    assert!(has_tag(Some("backup;homestack-gold"), "homestack-gold"));
    assert!(has_tag(Some("backup;homestack-ws"), "homestack-ws"));
    assert!(!has_tag(Some("prefix-homestack-ws-suffix"), "homestack-ws"));
    assert!(parse_tags(None).is_empty());
}

/// The test instance marks its workspaces, so the installer puts them back in
/// its pool after an uninstall.
#[test]
fn a_workspace_of_the_test_pool_is_tagged_homestack_test() {
    assert_eq!(workspace_tags("homestack"), "homestack-ws");
    assert_eq!(workspace_tags("homestack-test"), "homestack-ws;homestack-test");
}

/// `VmConfigTests::test_qm_config_uses_proxmox_json_api_not_terminal_text`
#[test]
fn vm_config_uses_the_json_api_and_reads_values_as_text() {
    let pve = FakePve::new().with(
        "GET /nodes/example-node-1/qemu/200/config",
        json!({"name": "test1", "tags": "homestack-ws", "scsi1": "example-storage-a:vm-200-disk-1,discard=on,iothread=1,serial=HS_HOME_200,size=20G,ssd=1", "memory": 4096, "agent": null}),
    );
    let config = block_on(vm_config(&pve, "example-node-1", 200)).unwrap_or_default();
    assert_eq!(config["name"], "test1");
    assert_eq!(config["tags"], "homestack-ws");
    assert_eq!(config["memory"], "4096");
    assert_eq!(config["agent"], "");
    assert_eq!(pve.calls(), ["GET /nodes/example-node-1/qemu/200/config"]);
}

/// `VmStatusTests::test_qm_status_uses_proxmox_json_api`
#[test]
fn vm_status_uses_the_json_api() {
    let pve = FakePve::new()
        .with(
            "GET /nodes/example-node-1/qemu/200/status/current",
            json!({"status": "running"}),
        )
        .failing(
            "GET /nodes/example-node-1/qemu/201/status/current",
            500,
            "does not exist",
        )
        .with("GET /nodes/example-node-1/qemu/202/status/current", json!([]));
    assert_eq!(block_on(vm_status(&pve, "example-node-1", 200)), "running");
    assert_eq!(block_on(vm_status(&pve, "example-node-1", 201)), "absent");
    assert_eq!(block_on(vm_status(&pve, "example-node-1", 202)), "unknown");
}

/// `VmVolumeInventoryTests::test_lists_root_home_cloudinit_and_unused_volume_refs`
#[test]
fn lists_root_home_cloudinit_and_unused_volume_refs() {
    let config = vm(&[
        ("ide0", "example-storage-a:vm-200-cloudinit,media=cdrom,size=4M"),
        ("ide2", "none,media=cdrom"),
        ("scsi0", "example-storage-a:vm-200-hs-root-default,iothread=1,size=16G"),
        (
            "scsi1",
            "example-storage-a:vm-200-hs-home-user,discard=on,iothread=1,serial=HS_HOME_200,size=20G,ssd=1",
        ),
        ("unused0", "example-storage-a:vm-200-disk-0"),
        ("name", "test1"),
    ]);
    let entry = |slot: &str, role: &str, volume: &str, size: Option<&str>| VolumeEntry {
        slot: slot.into(),
        role: role.into(),
        volume: volume.into(),
        storage: Some("example-storage-a".into()),
        size: size.map(Into::into),
    };
    assert_eq!(
        vm_volume_inventory(&config, &test_config()),
        [
            entry("ide0", "cloud-init", "example-storage-a:vm-200-cloudinit", Some("4M")),
            entry("scsi0", "root", "example-storage-a:vm-200-hs-root-default", Some("16G")),
            entry("scsi1", "home", "example-storage-a:vm-200-hs-home-user", Some("20G")),
            entry("unused0", "unused", "example-storage-a:vm-200-disk-0", None),
        ]
    );
}

/// `OrphanedVolumeTests::test_homestack_volume_identity`
#[test]
fn homestack_volume_identity() {
    let home = volume_identity("example-storage-a:vm-200-hs-home-user").map(|v| (v.vmid, v.role, v.name));
    assert_eq!(home, Some((200, "home".to_owned(), "user".to_owned())));
    let root = volume_identity("local:200/vm-200-hs-root-default.raw").map(|v| (v.volid, v.vmid, v.role, v.name));
    assert_eq!(
        root,
        Some((
            "local:200/vm-200-hs-root-default.raw".to_owned(),
            200,
            "root".to_owned(),
            "default".to_owned()
        ))
    );
    assert!(volume_identity("example-storage-a:vm-200-disk-1").is_none());
    assert!(volume_identity("example-storage-a:vm-200-home-user").is_none());
}

/// `OrphanedVolumeTests::test_attached_disk_volumes_reads_vm_disks_only`
#[test]
fn attached_disk_volumes_reads_vm_disks_only() {
    let config = vm(&[
        ("scsi0", "example-storage-a:vm-200-hs-root-default,iothread=1,size=16G"),
        ("scsi1", "example-storage-a:vm-200-hs-home-user,size=20G"),
        ("cicustom", "user=local:snippets/test.yaml"),
        ("unused0", "example-storage-a:vm-200-disk-9"),
        ("ide2", "none,media=cdrom"),
    ]);
    let expected: BTreeSet<String> = [
        "example-storage-a:vm-200-hs-root-default",
        "example-storage-a:vm-200-hs-home-user",
    ]
    .iter()
    .map(|v| (*v).to_owned())
    .collect();
    assert_eq!(attached_disk_volumes(&config), expected);
}

/// `OrphanedVolumeTests::test_storage_layout_maps_node_to_assigned_pve_storage_ids`
#[test]
fn storage_layout_maps_node_to_assigned_pve_storage_ids() {
    let cfg = test_config();
    assert_eq!(
        layout_name_for_node("example-node-1").as_deref(),
        Some("homestack-storage-1")
    );
    assert_eq!(storage_ids_for_node(&cfg, "example-node-1"), ["example-storage-a"]);
    assert_eq!(storage_ids_for_node(&cfg, "example-node-2"), ["example-storage-b"]);
    assert!(storage_ids_for_node(&cfg, "other").is_empty());
    assert_eq!(layout_name_for_node("pve-010").as_deref(), Some("homestack-storage-10"));
}
