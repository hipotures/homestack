//! Proxmox privileges each HomeStack operation needs, and which of them a
//! token is missing (mapping T3 and 8.5).
//!
//! The table follows the "Uprawnienia" lines of `docs/migration/api-mapping.md`
//! (sections 4 and 5), with these readings:
//!
//! - Workspaces are checked on `/vms`: there is no HomeStack pool yet, so a
//!   privilege on `/vms` is what covers every VMID HomeStack may use.
//!   Phase 3 adds the pool (`/pool/<name>`) together with the pool-limited
//!   token.
//! - Gold is `/vms/<gold_vmid>`.
//! - `create` and `refresh` allocate on the Gold node's layout; `resize` and
//!   `migrate` may touch any node, so they need every layout's storages.
//! - Guest verification after `create`, `refresh` and `migrate` (D2) still
//!   uses `exec`, so `VM.GuestAgent.Unrestricted` until decision 5 says
//!   otherwise.
//! - `SDN.Use` on the Gold bridge is not listed: the bridge comes from Gold's
//!   `net0`, not from the configuration, so it is checked when a `create`
//!   plan is made (phase 5).
//! - Every operation reads the cluster first: `Sys.Audit` on `/nodes`, and
//!   all but the Gold check `VM.Audit` on `/vms`.

use std::collections::BTreeSet;

pub use hs_model::capabilities::{Operation, OperationCapability, Privilege};
use hs_model::config::Config;

/// Which storages an operation allocates on.
#[derive(Clone, Copy)]
enum Storages {
    None,
    /// The configured node's layout.
    Active,
    /// Every layout.
    All,
}

struct Needs {
    vms: &'static [&'static str],
    gold: &'static [&'static str],
    audit_storages: bool,
    allocate: Storages,
    snippets: bool,
}

const fn needs(operation: Operation) -> Needs {
    const NONE: &[&str] = &[];
    match operation {
        Operation::Read => Needs {
            vms: &["VM.Audit"],
            gold: NONE,
            audit_storages: true,
            allocate: Storages::None,
            snippets: false,
        },
        Operation::GuestRead => Needs {
            vms: &["VM.Audit", "VM.GuestAgent.Audit"],
            gold: NONE,
            audit_storages: false,
            allocate: Storages::None,
            snippets: false,
        },
        Operation::GoldReadiness => Needs {
            vms: NONE,
            gold: &["VM.Audit"],
            audit_storages: false,
            allocate: Storages::None,
            snippets: false,
        },
        // B1
        Operation::Create => Needs {
            vms: &[
                "VM.Allocate",
                "VM.Audit",
                "VM.Config.Cloudinit",
                "VM.Config.Disk",
                "VM.Config.Options",
                "VM.GuestAgent.Audit",
                "VM.GuestAgent.Unrestricted",
                "VM.PowerMgmt",
            ],
            gold: &["VM.Audit", "VM.Clone"],
            audit_storages: false,
            allocate: Storages::Active,
            snippets: true,
        },
        // B3
        Operation::Refresh => Needs {
            vms: &[
                "VM.Audit",
                "VM.Config.Cloudinit",
                "VM.Config.Disk",
                "VM.GuestAgent.Audit",
                "VM.GuestAgent.Unrestricted",
                "VM.PowerMgmt",
            ],
            gold: &["VM.Audit", "VM.Clone"],
            audit_storages: false,
            allocate: Storages::Active,
            snippets: true,
        },
        // B5
        Operation::Resize => Needs {
            vms: &[
                "VM.Audit",
                "VM.Config.Disk",
                "VM.GuestAgent.Audit",
                "VM.GuestAgent.Unrestricted",
            ],
            gold: NONE,
            audit_storages: false,
            allocate: Storages::All,
            snippets: false,
        },
        // B6
        Operation::Migrate => Needs {
            vms: &[
                "VM.Audit",
                "VM.Config.Cloudinit",
                "VM.GuestAgent.Audit",
                "VM.Migrate",
                "VM.PowerMgmt",
            ],
            gold: NONE,
            audit_storages: false,
            allocate: Storages::All,
            snippets: false,
        },
        // B7 (D3 reads home usage through the guest agent)
        Operation::Destroy => Needs {
            vms: &["VM.Allocate", "VM.Audit", "VM.GuestAgent.Audit", "VM.PowerMgmt"],
            gold: NONE,
            audit_storages: false,
            allocate: Storages::None,
            snippets: false,
        },
        // C4
        Operation::Addkey => Needs {
            vms: &[
                "VM.Audit",
                "VM.Config.Cloudinit",
                "VM.GuestAgent.Audit",
                "VM.GuestAgent.Unrestricted",
                "VM.PowerMgmt",
            ],
            gold: NONE,
            audit_storages: false,
            allocate: Storages::None,
            snippets: false,
        },
    }
}

/// Privileges `operation` needs with `config`, sorted and without
/// duplicates.
#[must_use]
pub fn requirements(operation: Operation, config: &Config) -> Vec<Privilege> {
    let needs = needs(operation);
    let mut required = BTreeSet::new();
    let mut add = |path: &str, privilege: &str| {
        required.insert(Privilege {
            path: path.to_owned(),
            privilege: privilege.to_owned(),
        });
    };
    add("/nodes", "Sys.Audit");
    for privilege in needs.vms {
        add("/vms", privilege);
    }
    let gold = format!("/vms/{}", config.gold_vmid);
    for privilege in needs.gold {
        add(&gold, privilege);
    }
    let all_storages: BTreeSet<&String> = config.storage_layouts.values().flatten().collect();
    if needs.audit_storages {
        for storage in &all_storages {
            add(&format!("/storage/{storage}"), "Datastore.Audit");
        }
    }
    let allocate: Vec<&String> = match needs.allocate {
        Storages::None => Vec::new(),
        Storages::Active => config.active_layout().iter().collect(),
        Storages::All => all_storages.into_iter().collect(),
    };
    for storage in allocate {
        add(&format!("/storage/{storage}"), "Datastore.AllocateSpace");
    }
    if needs.snippets {
        add(
            &format!("/storage/{}", config.cloud_init.snippet_storage),
            "Datastore.AllocateSpace",
        );
    }
    required.into_iter().collect()
}

/// Every ACL path the operations need, so the probe asks Proxmox once per
/// path.
#[must_use]
pub fn probe_paths(config: &Config) -> BTreeSet<String> {
    Operation::ALL
        .into_iter()
        .flat_map(|operation| requirements(operation, config))
        .map(|privilege| privilege.path)
        .collect()
}

/// Every operation with what it needs and what `granted` lacks.
pub fn evaluate(config: &Config, granted: impl Fn(&str, &str) -> bool) -> Vec<OperationCapability> {
    Operation::ALL
        .into_iter()
        .map(|operation| {
            let required = requirements(operation, config);
            let missing: Vec<Privilege> = required
                .iter()
                .filter(|p| !granted(&p.path, &p.privilege))
                .cloned()
                .collect();
            OperationCapability {
                operation,
                available: missing.is_empty(),
                required,
                missing,
            }
        })
        .collect()
}
