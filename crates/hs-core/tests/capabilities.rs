//! Privileges each HomeStack operation needs, from the "Uprawnienia" lines of
//! `docs/migration/api-mapping.md` (sections 4 and 5), on the pool the
//! instance works in (phase 4b, decisions 9 and 10).

use std::collections::BTreeSet;

use hs_core::capabilities::{DEFAULT_POOL, Operation, evaluate, probe_paths, requirements};
use hs_model::capabilities::Privilege;
use hs_model::config::Config;

const SERVER_TOML: &str = r#"version = 1
[node]
name = "pve2"
gold_vmid = 101
[storage_layouts]
homestack-storage-1 = ["one"]
homestack-storage-2 = ["two-a", "two-b"]
[root]
disk = "scsi0"
[home]
disk = "scsi1"
default_size = "20G"
[network]
prefix = "192.0.2"
cidr = 24
gateway = "192.0.2.1"
dns = ["192.0.2.53"]
[cloud_init]
snippet_storage = "local"
snippet_dir = "/var/lib/vz/snippets"
[user]
name = "user"
uid = 1000
gid = 1000
"#;

fn config() -> Config {
    match Config::parse(SERVER_TOML) {
        Ok(config) => config,
        Err(error) => panic!("{error}"),
    }
}

fn has(required: &[Privilege], path: &str, privilege: &str) -> bool {
    required.iter().any(|p| p.path == path && p.privilege == privilege)
}

#[test]
fn every_operation_reads_the_cluster_first() {
    let config = config();
    for operation in Operation::ALL {
        let required = requirements(operation, &config, DEFAULT_POOL);
        assert!(has(&required, "/nodes", "Sys.Audit"), "{operation:?}");
        assert!(
            has(&required, "/vms", "VM.Audit") || operation == Operation::GoldReadiness,
            "{operation:?}"
        );
        let unique: BTreeSet<_> = required.iter().map(|p| (&p.path, &p.privilege)).collect();
        assert_eq!(unique.len(), required.len(), "{operation:?} lists a privilege twice");
        let mut sorted = required.clone();
        sorted.sort();
        assert_eq!(sorted, required, "{operation:?} is not sorted");
    }
}

#[test]
fn storage_paths_follow_the_layouts() {
    let config = config();
    let read = requirements(Operation::Read, &config, DEFAULT_POOL);
    for storage in ["one", "two-a", "two-b"] {
        assert!(
            has(&read, &format!("/storage/{storage}"), "Datastore.Audit"),
            "{storage}"
        );
    }
    // pve2 creates and refreshes on homestack-storage-2 only.
    for operation in [Operation::Create, Operation::Refresh] {
        let required = requirements(operation, &config, DEFAULT_POOL);
        assert!(
            has(&required, "/storage/two-a", "Datastore.AllocateSpace"),
            "{operation:?}"
        );
        assert!(
            !has(&required, "/storage/one", "Datastore.AllocateSpace"),
            "{operation:?}"
        );
        assert!(
            has(&required, "/storage/local", "Datastore.AllocateSpace"),
            "{operation:?}: cicustom"
        );
        assert!(has(&required, "/vms/101", "VM.Clone"), "{operation:?}: Gold");
    }
    // A workspace may live on any node.
    for operation in [Operation::Resize, Operation::Migrate] {
        let required = requirements(operation, &config, DEFAULT_POOL);
        assert!(
            has(&required, "/storage/one", "Datastore.AllocateSpace"),
            "{operation:?}"
        );
        assert!(
            has(&required, "/storage/two-b", "Datastore.AllocateSpace"),
            "{operation:?}"
        );
    }
}

#[test]
fn the_mapping_privileges_are_required() {
    let config = config();
    let cases: &[(Operation, &[&str])] = &[
        (
            Operation::Create,
            &[
                "VM.Allocate",
                "VM.Config.Disk",
                "VM.Config.Cloudinit",
                "VM.Config.Options",
                "VM.PowerMgmt",
            ],
        ),
        (
            Operation::Refresh,
            &[
                "VM.Config.Disk",
                "VM.Config.Cloudinit",
                "VM.PowerMgmt",
                "VM.GuestAgent.Unrestricted",
            ],
        ),
        (Operation::Resize, &["VM.Config.Disk", "VM.GuestAgent.Unrestricted"]),
        (
            Operation::Migrate,
            &[
                "VM.Migrate",
                "VM.PowerMgmt",
                "VM.Config.Cloudinit",
                "VM.GuestAgent.Audit",
            ],
        ),
        (Operation::Destroy, &["VM.Allocate", "VM.PowerMgmt", "VM.Audit"]),
        (
            Operation::Addkey,
            &["VM.Config.Cloudinit", "VM.PowerMgmt", "VM.GuestAgent.Unrestricted"],
        ),
    ];
    for (operation, privileges) in cases {
        let required = requirements(*operation, &config, DEFAULT_POOL);
        for privilege in *privileges {
            assert!(
                has(&required, "/pool/homestack", privilege),
                "{operation:?} needs {privilege} on the pool"
            );
        }
    }
    // Reads cover every workspace, also those made before the pool.
    assert!(has(
        &requirements(Operation::GuestRead, &config, DEFAULT_POOL),
        "/vms",
        "VM.GuestAgent.Audit"
    ));
    assert!(has(
        &requirements(Operation::GoldReadiness, &config, DEFAULT_POOL),
        "/vms/101",
        "VM.Audit"
    ));
}

#[test]
fn probe_paths_cover_every_requirement_once() {
    let config = config();
    let paths = probe_paths(&config, DEFAULT_POOL);
    let needed: BTreeSet<String> = Operation::ALL
        .into_iter()
        .flat_map(|operation| requirements(operation, &config, DEFAULT_POOL))
        .map(|p| p.path)
        .collect();
    assert_eq!(paths, needed);
}

#[test]
fn evaluation_lists_what_is_missing() {
    let config = config();
    let granted = |path: &str, privilege: &str| path == "/nodes" || (path == "/vms" && privilege == "VM.Audit");
    let result = evaluate(&config, DEFAULT_POOL, granted);
    let read = match result.iter().find(|op| op.operation == Operation::Read) {
        Some(read) => read,
        None => panic!("no read operation"),
    };
    assert!(!read.available);
    assert_eq!(
        read.missing.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
        ["/storage/one", "/storage/two-a", "/storage/two-b"]
    );
    let all = evaluate(&config, DEFAULT_POOL, |_, _| true);
    assert!(all.iter().all(|op| op.available && op.missing.is_empty()));
    assert_eq!(all.len(), Operation::ALL.len());
}

/// The gate before phase 5: changes are allowed only in the pool, so no
/// operation needs more than reading on `/vms`.
#[test]
fn changes_need_nothing_on_vms_beyond_reading() {
    let config = config();
    for operation in Operation::ALL {
        for privilege in requirements(operation, &config, DEFAULT_POOL) {
            if privilege.path == "/vms" {
                assert!(
                    ["VM.Audit", "VM.GuestAgent.Audit"].contains(&privilege.privilege.as_str()),
                    "{operation:?} needs {} on /vms",
                    privilege.privilege
                );
            }
        }
    }
}

/// Decision 10 = C: the detached-volume scan lists the disks of the pool's
/// VMs only.
#[test]
fn the_orphan_scan_needs_disk_privileges_on_the_pool_only() {
    let required = requirements(Operation::OrphanScan, &config(), DEFAULT_POOL);
    assert!(has(&required, "/pool/homestack", "VM.Config.Disk"));
    assert!(!has(&required, "/vms", "VM.Config.Disk"));
}

/// Without `Pool.Audit` Proxmox does not say which pool a VM is in, and
/// every change checks that first.
#[test]
fn changes_need_to_see_the_pool() {
    let config = config();
    for operation in [
        Operation::Create,
        Operation::Refresh,
        Operation::Resize,
        Operation::Migrate,
        Operation::Destroy,
        Operation::Addkey,
    ] {
        let required = requirements(operation, &config, DEFAULT_POOL);
        assert!(has(&required, "/pool/homestack", "Pool.Audit"), "{operation:?}");
    }
}

#[test]
fn the_pool_is_the_instances() {
    let required = requirements(Operation::Destroy, &config(), "homestack-test");
    assert!(has(&required, "/pool/homestack-test", "VM.Allocate"));
    assert!(!required.iter().any(|p| p.path == "/pool/homestack"));
    assert_eq!(DEFAULT_POOL, "homestack");
}
