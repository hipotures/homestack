//! Ports of `python/tests/test_gold.py` (the Proxmox part) and
//! `test_discovery.py` (the Proxmox part of discovery and network profiles).

mod common;

use common::{FakePve, block_on, test_config};
use hs_core::discovery::{discovery, workspace_networks};
use hs_core::gold::{agent_enabled, cloud_init_slots, gold_readiness};
use hs_core::inventory::VmConfig;
use hs_model::status::NetworkCandidate;
use serde_json::{Value, json};

fn gold_config() -> Value {
    json!({
        "tags": "homestack-gold",
        "scsi0": "local-lvm:vm-101-disk-0,size=20G",
        "net0": "virtio=02:00:00:00:01:01,bridge=vmbr0",
        "ide2": "local-lvm:cloudinit,media=cdrom",
        "agent": "1",
        "boot": "order=scsi0;ide2;net0",
        "sshkeys": "ssh-ed25519%20AAAATEST%20test",
    })
}

fn stopped_gold(config: Value) -> FakePve {
    FakePve::new()
        .with("GET /nodes/example-node-1/qemu/101/config", config)
        .with(
            "GET /nodes/example-node-1/qemu/101/status/current",
            json!({"status": "stopped"}),
        )
}

fn readiness(pve: &FakePve) -> hs_model::status::GoldReadiness {
    match block_on(gold_readiness(pve, &test_config().root_disk, "example-node-1", 101)) {
        Ok(readiness) => readiness,
        Err(error) => panic!("{error}"),
    }
}

/// `GoldReadinessTests::test_stopped_gold_can_be_pve_ready_with_configured_public_keys`
#[test]
fn stopped_gold_can_be_pve_ready_with_configured_public_keys() {
    let result = readiness(&stopped_gold(gold_config()));
    assert!(result.ok);
    assert!(!result.guest_checked);
    assert!(!result.checks.iter().any(|c| c.status == "fail"));
    let keys = result.checks.iter().find(|c| c.name == "Workspace public-key source");
    assert_eq!(
        keys.map(|c| c.detail.as_str()),
        Some("1 public key(s) in Proxmox sshkeys; required while Gold is stopped")
    );
}

/// `GoldReadinessTests::test_extra_home_disk_fails_gold_contract`
#[test]
fn extra_home_disk_fails_gold_contract() {
    let mut config = gold_config();
    config["scsi1"] = json!("local-lvm:vm-101-home,size=20G");
    let result = readiness(&stopped_gold(config));
    assert!(!result.ok);
    assert!(
        result
            .checks
            .iter()
            .any(|c| c.name == "Extra data disks" && c.status == "fail" && c.detail == "scsi1")
    );
}

/// `GoldReadinessTests::test_alien_role_cannot_be_used_as_gold`
#[test]
fn alien_role_cannot_be_used_as_gold() {
    let mut config = gold_config();
    config["tags"] = json!("homestack-gold;homestack-alien");
    let result = readiness(&stopped_gold(config));
    assert!(!result.ok);
    assert!(result.checks.iter().any(|c| c.name == "Role tag" && c.status == "fail"));
}

/// `GoldStaticHelpersTests::test_agent_and_cloud_init_detection`
#[test]
fn agent_and_cloud_init_detection() {
    assert!(agent_enabled(Some("1")));
    assert!(agent_enabled(Some("enabled=1,fstrim_cloned_disks=1")));
    assert!(!agent_enabled(Some("0")));
    assert!(!agent_enabled(None));
    let config: VmConfig = [
        ("ide2".to_owned(), "local-lvm:cloudinit,media=cdrom".to_owned()),
        ("scsi0".to_owned(), "local-lvm:vm-101-disk-0,size=20G".to_owned()),
    ]
    .into_iter()
    .collect();
    assert_eq!(cloud_init_slots(&config), ["ide2"]);
}

#[test]
fn running_gold_gets_the_agent_check_and_leaves_guest_contract_to_exec() {
    let pve = FakePve::new()
        .with("GET /nodes/example-node-1/qemu/101/config", gold_config())
        .with(
            "GET /nodes/example-node-1/qemu/101/status/current",
            json!({"status": "running"}),
        )
        .with("POST /nodes/example-node-1/qemu/101/agent/ping", json!({"result": {}}));
    let result = readiness(&pve);
    let agent = result.checks.iter().find(|c| c.name == "QEMU Guest Agent response");
    assert_eq!(
        agent.map(|c| (c.status.as_str(), c.detail.as_str())),
        Some(("pass", "responding"))
    );
    let tools = result.checks.iter().find(|c| c.name == "Required guest tools");
    assert_eq!(tools.map(|c| c.status.as_str()), Some("skip"));
    assert!(result.guest_checked);
    assert!(result.ok);
}

/// `EnvironmentDiscoveryTests::test_discovery_uses_only_read_only_pvesh_queries`
#[test]
fn discovery_uses_only_read_only_queries() {
    let pve = FakePve::new()
        .with("GET /version", json!({"version": "9.0"}))
        .with("GET /nodes", json!([{"node": "pve1", "status": "online"}]))
        .with(
            "GET /cluster/resources?type=vm",
            json!([{"type": "qemu", "vmid": 101, "name": "gold", "node": "pve1", "tags": "homestack-gold"}]),
        )
        .with("GET /storage", json!([{"storage": "local-lvm", "content": "images"}]));
    let report = match block_on(discovery(&pve)) {
        Ok(report) => report,
        Err(error) => panic!("{error}"),
    };
    assert!(pve.calls().iter().all(|call| call.starts_with("GET ")));
    assert_eq!(report.nodes[0].node, "pve1");
    assert_eq!(report.gold_candidates[0]["vmid"], 101);
    assert!(
        report.gold_candidates[0].get("status").is_none(),
        "only keys Proxmox reported"
    );
    assert_eq!(
        Value::Object(report.storages[0].clone()),
        json!({"storage": "local-lvm", "content": "images"})
    );
}

/// `EnvironmentDiscoveryTests::test_workspace_network_discovery_prefers_existing_workspace_on_gold_bridge`
#[test]
fn workspace_network_discovery_prefers_existing_workspace_on_gold_bridge() {
    let pve = FakePve::new()
        .with("GET /nodes/pve2/qemu/101/config", json!({"net0": "virtio=AA:BB:CC:DD:EE:01,bridge=vmbr1"}))
        .with(
            "GET /nodes/pve2/qemu/200/config",
            json!({"net0": "virtio=AA:BB:CC:DD:EE:02,bridge=vmbr1", "ipconfig0": "ip=192.168.100.200/24,gw=192.168.100.1"}),
        )
        .with(
            "GET /nodes/pve2/network",
            json!([
                {"iface": "vmbr0", "type": "bridge", "address": "192.168.1.11", "netmask": "255.255.255.0", "gateway": "192.168.1.1"},
                {"iface": "vmbr1", "type": "bridge", "address": "192.168.100.11", "netmask": "255.255.255.0", "gateway": "192.168.100.1"},
            ]),
        )
        .with("GET /nodes/pve2/dns", json!({"dns1": "192.168.100.1"}));
    let resources = [
        hs_core::status::StatusResource {
            vmid: 101,
            node: "pve2".into(),
            name: Some("gold".into()),
            status: None,
            tags: Some("homestack-gold".into()),
        },
        hs_core::status::StatusResource {
            vmid: 200,
            node: "pve2".into(),
            name: Some("ws".into()),
            status: None,
            tags: Some("homestack-ws".into()),
        },
    ];
    let candidates = block_on(workspace_networks(&pve, &resources, 101, "pve2")).unwrap_or_default();
    assert_eq!(
        candidates,
        [NetworkCandidate {
            cidr: "192.168.100.0/24".into(),
            bridge: Some("vmbr1".into()),
            gateway: Some("192.168.100.1".into()),
            dns_servers: vec!["192.168.100.1".into()],
            source: "HomeStack workspace VM 200".into(),
        }]
    );
}
