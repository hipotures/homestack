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
    match block_on(gold_readiness(
        pve,
        &test_config().root_disk,
        &test_config().user,
        "example-node-1",
        101,
    )) {
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

const TOOLS: &str = r#"for c in cloud-init sshd nmcli lsblk blkid wipefs mkfs.ext4 mount mountpoint systemctl systemd-escape install; do command -v "$c" >/dev/null 2>&1 || printf "%s\n" "$c"; done"#;
const RSYNC: &str = "if command -v rsync >/dev/null 2>&1; then echo PRESENT; else echo ABSENT; fi";
const REPO_TOOLS: &str = r#"for c in git ssh-keygen; do command -v "$c" >/dev/null 2>&1 || printf "%s\n" "$c"; done"#;
const ACCOUNT: &str = "getent passwd user || true";
const REGULAR: &str = r#"awk -F: '$3 >= 1000 && $3 < 65534 {print $1 ":" $3 ":" $4}' /etc/passwd"#;
const SUDO: &str = "if command -v sudo >/dev/null 2>&1; then echo PRESENT; else echo ABSENT; fi";
const ROOT_KEYS: &str = "test -s /root/.ssh/authorized_keys && echo OK || echo MISSING";
// Not in Python: what the vendor snippet needs (decision 1 = A, phase 4b).
const VENDOR_TOOLS: &str = r#"for c in netplan udevadm NetworkManager python3; do command -v "$c" >/dev/null 2>&1 || printf "%s\n" "$c"; done"#;
const PYTHON_YAML: &str = "python3 -c 'import yaml' >/dev/null 2>&1 && echo OK || echo MISSING";

fn out(text: &str) -> Value {
    json!({"exited": 1, "exitcode": 0, "out-data": text})
}

/// A running Gold whose guest answers like the `guest_value` of the
/// Python tests, with `rsync` answering `rsync`.
fn running_gold(rsync: &str) -> FakePve {
    running_gold_with(rsync, "", "OK")
}

fn running_gold_with(rsync: &str, vendor_missing: &str, yaml: &str) -> FakePve {
    FakePve::new()
        .with("GET /nodes/example-node-1/qemu/101/config", gold_config())
        .with(
            "GET /nodes/example-node-1/qemu/101/status/current",
            json!({"status": "running"}),
        )
        .with("POST /nodes/example-node-1/qemu/101/agent/ping", json!({"result": {}}))
        .with_exec("example-node-1", 101, TOOLS, 1, out(""))
        .with_exec("example-node-1", 101, RSYNC, 2, out(&format!("{rsync}\n")))
        .with_exec("example-node-1", 101, REPO_TOOLS, 3, out(""))
        .with_exec(
            "example-node-1",
            101,
            ACCOUNT,
            4,
            out("user:x:1000:1000::/home/user:/bin/bash\n"),
        )
        .with_exec("example-node-1", 101, REGULAR, 5, out("user:1000:1000\n"))
        .with_exec("example-node-1", 101, SUDO, 6, out("ABSENT\n"))
        .with_exec("example-node-1", 101, ROOT_KEYS, 7, out("OK\n"))
        .with_exec("example-node-1", 101, VENDOR_TOOLS, 8, out(vendor_missing))
        .with_exec("example-node-1", 101, PYTHON_YAML, 9, out(&format!("{yaml}\n")))
}

fn optional_failures(result: &hs_model::status::GoldReadiness) -> Vec<&str> {
    result
        .checks
        .iter()
        .filter(|c| c.requirement == "optional" && c.status == "fail")
        .map(|c| c.name.as_str())
        .collect()
}

/// `GoldReadinessTests::test_running_gold_checks_guest_contract`, plus the
/// vendor snippet's tools after Python's checks (phase 4b).
#[test]
fn running_gold_checks_guest_contract() {
    let pve = running_gold("PRESENT");
    let result = readiness(&pve);
    assert!(result.ok, "{:?}", result.checks);
    assert!(result.guest_checked);
    assert!(optional_failures(&result).is_empty());
    let names: Vec<(&str, &str)> = result
        .checks
        .iter()
        .skip(9)
        .map(|c| (c.name.as_str(), c.status.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("QEMU Guest Agent response", "pass"),
            ("Required guest tools", "pass"),
            ("rsync for Setup Files", "pass"),
            ("git/ssh-keygen for homestack repo", "pass"),
            ("Workspace account", "pass"),
            ("Regular user policy", "pass"),
            ("No sudo", "pass"),
            ("Root authorized_keys", "pass"),
            ("Workspace public-key source", "pass"),
            ("Cloud-Init vendor snippet tools", "pass"),
        ]
    );
}

/// Not in Python: the vendor snippet (decision 1 = A) makes NetworkManager
/// netplan's renderer, triggers udev and runs cloud-init's Python, so Gold
/// without them boots a workspace with no network or no home.
#[test]
fn gold_without_the_vendor_snippet_tools_is_not_ready() {
    let result = readiness(&running_gold_with("PRESENT", "netplan\n", "MISSING"));
    assert!(!result.ok);
    let check = result
        .checks
        .iter()
        .find(|c| c.name == "Cloud-Init vendor snippet tools");
    assert_eq!(
        check.map(|c| (
            c.scope.as_str(),
            c.status.as_str(),
            c.detail.as_str(),
            c.requirement.as_str()
        )),
        Some(("guest", "fail", "missing: netplan, python3-yaml", "required"))
    );

    let stopped = readiness(&stopped_gold(gold_config()));
    let check = stopped
        .checks
        .iter()
        .find(|c| c.name == "Cloud-Init vendor snippet tools");
    assert_eq!(check.map(|c| c.status.as_str()), Some("skip"));
}

/// `GoldReadinessTests::test_missing_rsync_is_optional`
#[test]
fn missing_rsync_is_optional() {
    let pve = running_gold("ABSENT");
    let result = readiness(&pve);
    assert!(result.ok);
    assert_eq!(optional_failures(&result), ["rsync for Setup Files"]);
}

/// Not in Python (it runs as root): without `VM.GuestAgent.Unrestricted` on
/// Gold the guest contract is skipped with the reason, not failed.
#[test]
fn guest_contract_without_exec_privilege_is_skipped() {
    let pve = FakePve::new()
        .with("GET /nodes/example-node-1/qemu/101/config", gold_config())
        .with(
            "GET /nodes/example-node-1/qemu/101/status/current",
            json!({"status": "running"}),
        )
        .with("POST /nodes/example-node-1/qemu/101/agent/ping", json!({"result": {}}))
        .failing(
            &common::exec_key("example-node-1", 101, TOOLS),
            403,
            "Permission check failed (/vms/101, VM.GuestAgent.Unrestricted)",
        );
    let result = readiness(&pve);
    let tools = result.checks.iter().find(|c| c.name == "Required guest tools");
    assert_eq!(
        tools.map(|c| (c.status.as_str(), c.detail.as_str())),
        Some((
            "skip",
            "needs VM.GuestAgent.Unrestricted on /vms/101 to run commands in Gold"
        ))
    );
    let keys = result.checks.iter().find(|c| c.name == "Workspace public-key source");
    assert_eq!(keys.map(|c| c.status.as_str()), Some("pass"), "Proxmox sshkeys suffice");
    assert!(result.ok);
    assert_eq!(pve.calls().len(), 4, "no command after the refused one");
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
