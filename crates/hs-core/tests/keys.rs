//! Ports of `python/tests/test_workspace_ssh.py` (`GoldKeyRoutingTests`)
//! and new tests of `workspace_ssh.get_workspace_authorized_keys`,
//! `validate_authorized_keys` and `parse_authorized_key_records` (C5).

mod common;

use base64::Engine;
use common::{FakePve, block_on, key, test_config};
use hs_core::keys::{KeyRecord, parse_authorized_key_records, validate_authorized_keys, workspace_authorized_keys};
use serde_json::{Value, json};

const GOLD_NODE: &str = "example-node-2";

fn resources(status: &str) -> Value {
    json!([
        {"type": "qemu", "vmid": 200, "node": "example-node-1", "status": "running"},
        {"type": "qemu", "vmid": 101, "node": GOLD_NODE, "status": status},
    ])
}

fn file_read(path: &str) -> String {
    key(
        "GET",
        &format!("/nodes/{GOLD_NODE}/qemu/101/agent/file-read"),
        &[("file", path), ("offset", "0"), ("count", "1048576"), ("decode", "0")],
    )
}

fn content(text: &str) -> Value {
    json!({"content": base64::engine::general_purpose::STANDARD.encode(text), "bytes-read": text.len()})
}

/// Gold on `example-node-2`, running, with the given `sshkeys`.
fn gold(sshkeys: Option<&str>) -> FakePve {
    let config = sshkeys.map_or_else(|| json!({}), |keys| json!({"sshkeys": keys}));
    FakePve::new()
        .with("GET /cluster/resources?type=vm", resources("running"))
        .with(&format!("GET /nodes/{GOLD_NODE}/qemu/101/config"), config)
        .with(
            &format!("GET /nodes/{GOLD_NODE}/qemu/101/status/current"),
            json!({"status": "running"}),
        )
        .with(
            &format!("POST /nodes/{GOLD_NODE}/qemu/101/agent/ping"),
            json!({"result": {}}),
        )
}

/// `GoldKeyRoutingTests::test_guest_key_fallback_runs_on_gold_owner_node`:
/// Gold's config, status, ping and guest read all go to Gold's node. The
/// read is `file-read` instead of `exec` (decision 5).
#[test]
fn guest_key_fallback_runs_on_gold_owner_node() {
    let pve = gold(None).with(
        &file_read("/home/user/.ssh/authorized_keys"),
        content("ssh-ed25519 AAAATEST routed\n"),
    );
    let found = block_on(workspace_authorized_keys(&pve, &test_config()));
    let found = found.unwrap_or_else(|error| panic!("{error}"));
    assert!(found.keys.contains("AAAATEST"));
    assert!(found.source.contains("/home/user/.ssh/authorized_keys"));
    let calls = pve.calls();
    assert!(
        calls[1..]
            .iter()
            .all(|call| call.contains(&format!("/nodes/{GOLD_NODE}/"))),
        "{calls:?}"
    );
    assert_eq!(calls.len(), 5);
}

#[test]
fn configured_sshkeys_come_first() {
    let pve = gold(Some("ssh-ed25519%20AAAAGOLD%20gold%0A%0A"));
    let found = block_on(workspace_authorized_keys(&pve, &test_config())).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(found.keys, "ssh-ed25519 AAAAGOLD gold\n");
    assert_eq!(found.source, "Gold VM 101:qm sshkeys");
    assert_eq!(pve.calls().len(), 2, "no status, ping or guest read");
}

/// Unusable `sshkeys` are skipped, an empty or unusable home file too.
#[test]
fn unusable_sources_fall_through_to_root() {
    let pve = gold(Some("not-a-key"))
        .with(
            &file_read("/home/user/.ssh/authorized_keys"),
            content("# only a comment\n"),
        )
        .with(
            &file_read("/root/.ssh/authorized_keys"),
            content("ecdsa-sha2-nistp256 AAAAROOT root\n"),
        );
    let found = block_on(workspace_authorized_keys(&pve, &test_config())).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(found.keys, "ecdsa-sha2-nistp256 AAAAROOT root\n");
    assert_eq!(found.source, "Gold VM 101:/root/.ssh/authorized_keys");
}

fn error_of(pve: &FakePve) -> Option<String> {
    block_on(workspace_authorized_keys(pve, &test_config()))
        .err()
        .map(|e| e.to_string())
}

#[test]
fn no_usable_key_anywhere() {
    let missing = "Agent error: failed to open file '/x' (mode: 'r'): No such file or directory";
    let pve = gold(None)
        .with(&file_read("/home/user/.ssh/authorized_keys"), content(""))
        .failing(&file_read("/root/.ssh/authorized_keys"), 500, missing);
    assert_eq!(
        error_of(&pve).as_deref(),
        Some(
            "Gold VM 101 has no usable public keys in its Proxmox sshkeys setting, \
             /home/user/.ssh/authorized_keys, or /root/.ssh/authorized_keys"
        )
    );
}

#[test]
fn missing_gold() {
    let pve = FakePve::new().with(
        "GET /cluster/resources?type=vm",
        json!([{"type": "qemu", "vmid": 200, "node": "example-node-1"}]),
    );
    assert_eq!(error_of(&pve).as_deref(), Some("Gold VM 101 does not exist"));
}

#[test]
fn gold_without_node() {
    let pve = FakePve::new().with(
        "GET /cluster/resources?type=vm",
        json!([{"type": "qemu", "vmid": 101, "node": " "}]),
    );
    assert_eq!(
        error_of(&pve).as_deref(),
        Some("Gold VM 101 has no node in cluster inventory")
    );
}

#[test]
fn stopped_gold_without_keys() {
    let pve = FakePve::new()
        .with("GET /cluster/resources?type=vm", resources("stopped"))
        .with(
            &format!("GET /nodes/{GOLD_NODE}/qemu/101/config"),
            json!({"sshkeys": ""}),
        )
        .with(
            &format!("GET /nodes/{GOLD_NODE}/qemu/101/status/current"),
            json!({"status": "stopped"}),
        );
    assert_eq!(
        error_of(&pve).as_deref(),
        Some("Gold VM 101 is stopped and its Proxmox sshkeys setting does not contain usable public keys.")
    );
}

#[test]
fn gold_agent_not_available() {
    let pve = FakePve::new()
        .with("GET /cluster/resources?type=vm", resources("running"))
        .with(&format!("GET /nodes/{GOLD_NODE}/qemu/101/config"), json!({}))
        .with(
            &format!("GET /nodes/{GOLD_NODE}/qemu/101/status/current"),
            json!({"status": "running"}),
        )
        .failing(
            &format!("POST /nodes/{GOLD_NODE}/qemu/101/agent/ping"),
            500,
            "QEMU guest agent is not running",
        );
    assert_eq!(
        error_of(&pve).as_deref(),
        Some("Gold VM 101 QEMU Guest Agent is not available")
    );
}

/// A refused read (missing privilege) is an error, not a missing file.
#[test]
fn refused_guest_read_is_an_error() {
    let pve = gold(None).failing(
        &file_read("/home/user/.ssh/authorized_keys"),
        403,
        "Permission check failed (/vms/101, VM.GuestAgent.FileRead|VM.GuestAgent.Unrestricted)",
    );
    let error = error_of(&pve).unwrap_or_default();
    assert!(error.contains("VM.GuestAgent.FileRead"), "{error}");
}

#[test]
fn validate_keeps_key_lines_only() {
    assert_eq!(
        validate_authorized_keys("\n# comment\n  ssh-ed25519 AAAA a  \nsk-ecdsa-sha2-nistp256@openssh.com B\n")
            .ok()
            .as_deref(),
        Some("ssh-ed25519 AAAA a\nsk-ecdsa-sha2-nistp256@openssh.com B\n")
    );
    let long = format!("rsa {}", "x".repeat(100));
    assert_eq!(
        validate_authorized_keys(&long).err().map(|e| e.to_string()),
        Some(format!("Unexpected line in authorized_keys source: '{}'", &long[..80]))
    );
    assert_eq!(
        validate_authorized_keys("# none\n")
            .err()
            .map(|e| e.to_string())
            .as_deref(),
        Some("No SSH public keys found for workspace user")
    );
}

#[test]
fn parse_records_label_by_comment_or_blob() {
    let records = parse_authorized_key_records(
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 me@desk  \nsk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1\n",
    );
    assert_eq!(
        records.ok(),
        Some(vec![
            KeyRecord {
                key_type: "ssh-ed25519".to_owned(),
                blob: "AAAAC3NzaC1lZDI1NTE5".to_owned(),
                comment: "me@desk".to_owned(),
                label: "me@desk".to_owned(),
                identity: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5".to_owned(),
            },
            KeyRecord {
                key_type: "sk-ssh-ed25519@openssh.com".to_owned(),
                blob: "AAAAGnNrLXNzaC1lZDI1".to_owned(),
                comment: String::new(),
                label: "sk-ssh-ed25519@openssh.com AAAAGnNrLXNz…".to_owned(),
                identity: "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1".to_owned(),
            },
        ])
    );
    assert_eq!(
        parse_authorized_key_records("ssh-ed25519")
            .err()
            .map(|e| e.to_string())
            .as_deref(),
        Some("Invalid authorized_keys line: 'ssh-ed25519'")
    );
}
