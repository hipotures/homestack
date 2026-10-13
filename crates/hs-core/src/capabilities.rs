//! Proxmox privileges each HomeStack operation needs, and which of them a
//! token is missing (mapping T3 and 8.5).
//!
//! The table follows the "Uprawnienia" lines of `docs/migration/api-mapping.md`
//! (sections 4 and 5), with these readings:
//!
//! - Workspaces are read and changed only in the instance's pool
//!   (`/pool/<pool>`, `homestack` unless `HOMESTACK_PVE_POOL` says
//!   otherwise): roles on a pool apply to its VMs, and the pool exists
//!   before its first VM does. Nothing is needed on `/vms`: the token sees
//!   no VM outside the HomeStack pools (owner, 2026-10-13). Every change
//!   also needs `Pool.Audit` on the pool: without it Proxmox does not say
//!   which pool a VM is in (phase 3).
//! - Gold is `/vms/<gold_vmid>`: Proxmox adds the roles of Gold's pool
//!   (`homestack-gold`, decision 9) to that path, so asking about it
//!   covers both.
//! - `refresh` allocates on the Gold node's layout; `create`, `resize` and
//!   `migrate` may touch any node, so they need every layout's storages.
//! - Guest verification (D2) reads with `VM.GuestAgent.Audit` and
//!   `FileRead` (decision 5): `create` needs no `VM.GuestAgent.Unrestricted`.
//!   `refresh` keeps it until phase 6 ports its verification.
//! - `SDN.Use` on the Gold bridge is not listed: the bridge comes from Gold's
//!   `net0`, not from the configuration, so `plan_create` checks it.
//! - Detached volumes (`orphan_scan`) need `VM.Config.Disk` on the pool
//!   (decision 10 = C): Proxmox lists a VM volume only to who may change that
//!   VM's disks (`PVE::Storage::check_volume_access`), so volumes of deleted
//!   VMs stay hidden and the scan says it is incomplete.
//! - Every operation reads the cluster first: `Sys.Audit` on `/nodes`, and
//!   all but the Gold check `VM.Audit` on the pool.

use std::collections::BTreeSet;

use hs_model::capabilities::{Installation, InstallationCheck, InstallationItem, InstallationStatus};
pub use hs_model::capabilities::{Operation, OperationCapability, Privilege};
use hs_model::config::Config;
use serde_json::Value;

use crate::pve::Pve;

/// The pool an instance changes workspaces in, unless configured otherwise.
pub const DEFAULT_POOL: &str = "homestack";
/// Gold's pool (decision 9), unless configured otherwise.
pub const DEFAULT_GOLD_POOL: &str = "homestack-gold";

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
    /// On the instance's pool (the token sees no VM outside the HomeStack
    /// pools).
    pool: &'static [&'static str],
    gold: &'static [&'static str],
    audit_storages: bool,
    allocate: Storages,
    snippets: bool,
}

const fn needs(operation: Operation) -> Needs {
    const NONE: &[&str] = &[];
    match operation {
        Operation::Read => Needs {
            pool: &["VM.Audit"],
            gold: NONE,
            audit_storages: true,
            allocate: Storages::None,
            snippets: false,
        },
        Operation::OrphanScan => Needs {
            pool: &["VM.Audit", "VM.Config.Disk"],
            gold: NONE,
            audit_storages: true,
            allocate: Storages::None,
            snippets: false,
        },
        Operation::GuestRead => Needs {
            pool: &["VM.Audit", "VM.GuestAgent.Audit"],
            gold: NONE,
            audit_storages: false,
            allocate: Storages::None,
            snippets: false,
        },
        Operation::GoldReadiness => Needs {
            pool: NONE,
            gold: &["VM.Audit"],
            audit_storages: false,
            allocate: Storages::None,
            snippets: false,
        },
        // B1, B2 (on another node the clone is migrated there, decision 3 = A)
        Operation::Create => Needs {
            pool: &[
                "Pool.Audit",
                "VM.Allocate",
                "VM.Audit",
                "VM.Config.Cloudinit",
                "VM.Config.Disk",
                "VM.Config.Options",
                "VM.GuestAgent.Audit",
                "VM.GuestAgent.FileRead",
                "VM.Migrate",
                "VM.PowerMgmt",
            ],
            gold: &["VM.Audit", "VM.Clone"],
            audit_storages: false,
            allocate: Storages::All,
            snippets: true,
        },
        // B3
        Operation::Refresh => Needs {
            pool: &[
                "Pool.Audit",
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
            pool: &[
                "Pool.Audit",
                "VM.Audit",
                "VM.Config.Disk",
                "VM.GuestAgent.Audit",
                "VM.GuestAgent.FileRead",
                "VM.GuestAgent.Unrestricted",
            ],
            gold: NONE,
            audit_storages: false,
            allocate: Storages::All,
            snippets: false,
        },
        // B6 (no `qm cloudinit update` under decision 1 = A; the home's
        // label is read from its superblock, phase 8)
        Operation::Migrate => Needs {
            pool: &[
                "Pool.Audit",
                "VM.Audit",
                "VM.GuestAgent.Audit",
                "VM.GuestAgent.FileRead",
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
            pool: &[
                "Pool.Audit",
                "VM.Allocate",
                "VM.Audit",
                "VM.GuestAgent.Audit",
                "VM.PowerMgmt",
            ],
            gold: NONE,
            audit_storages: false,
            allocate: Storages::None,
            snippets: false,
        },
        // C4 (the keys go in through `exec` and are checked with
        // `file-read`; `cicustom` is switched to the keep snippet, phase 9)
        Operation::Addkey => Needs {
            pool: &[
                "Pool.Audit",
                "VM.Audit",
                "VM.Config.Cloudinit",
                "VM.GuestAgent.Audit",
                "VM.GuestAgent.FileRead",
                "VM.GuestAgent.Unrestricted",
                "VM.PowerMgmt",
            ],
            gold: NONE,
            audit_storages: false,
            allocate: Storages::None,
            snippets: true,
        },
    }
}

/// Privileges `operation` needs with `config`, sorted and without
/// duplicates.
#[must_use]
pub fn requirements(operation: Operation, config: &Config, pool: &str) -> Vec<Privilege> {
    let needs = needs(operation);
    let mut required = BTreeSet::new();
    let mut add = |path: &str, privilege: &str| {
        required.insert(Privilege {
            path: path.to_owned(),
            privilege: privilege.to_owned(),
        });
    };
    add("/nodes", "Sys.Audit");
    let pool = format!("/pool/{pool}");
    for privilege in needs.pool {
        add(&pool, privilege);
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
pub fn probe_paths(config: &Config, pool: &str) -> BTreeSet<String> {
    Operation::ALL
        .into_iter()
        .flat_map(|operation| requirements(operation, config, pool))
        .map(|privilege| privilege.path)
        .collect()
}

/// Every operation with what it needs and what `granted` lacks.
pub fn evaluate(config: &Config, pool: &str, granted: impl Fn(&str, &str) -> bool) -> Vec<OperationCapability> {
    Operation::ALL
        .into_iter()
        .map(|operation| {
            let required = requirements(operation, config, pool);
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

fn check(item: InstallationItem, status: InstallationStatus, detail: impl Into<String>) -> InstallationCheck {
    InstallationCheck {
        item,
        status,
        detail: detail.into(),
    }
}

/// What of `deploy/pve/homestack-pve.sh install` the token can see: both
/// pools (`GET /pools`, which lists the pools with `Pool.Audit`), Gold in
/// its pool, and the snippets, which it cannot see.
pub async fn installation(pve: &impl Pve, config: &Config, pool: &str, gold_pool: &str) -> Installation {
    use InstallationItem::{GoldInPool, GoldPool, Pool, Snippets};
    use InstallationStatus::{Missing, Ok, Unknown};
    const INSTALL: &str = "run deploy/pve/homestack-pve.sh install on a node";
    let mut checks = Vec::new();
    match pve.get("/pools", &[]).await {
        Err(error) => {
            for item in [Pool, GoldPool, GoldInPool] {
                checks.push(check(item, Unknown, format!("Cannot list the pools: {error}")));
            }
        }
        Result::Ok(list) => {
            let exists = |id: &str| {
                list.as_array().is_some_and(|pools| {
                    pools
                        .iter()
                        .any(|p| p.get("poolid").and_then(Value::as_str) == Some(id))
                })
            };
            for (item, id) in [(Pool, pool), (GoldPool, gold_pool)] {
                checks.push(if exists(id) {
                    check(item, Ok, format!("pool {id} exists"))
                } else {
                    check(
                        item,
                        Missing,
                        format!("pool {id} does not exist, or the token lacks Pool.Audit on /pool/{id}; {INSTALL}"),
                    )
                });
            }
            let gold = config.gold_vmid;
            checks.push(if exists(gold_pool) {
                match pve.get("/pools", &[("poolid", gold_pool)]).await {
                    Result::Ok(found) => {
                        let member = found
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|p| p.get("members").and_then(Value::as_array))
                            .flatten()
                            .any(|m| m.get("vmid").and_then(Value::as_u64) == Some(u64::from(gold)));
                        if member {
                            check(GoldInPool, Ok, format!("Gold {gold} is in pool {gold_pool}"))
                        } else {
                            check(
                                GoldInPool,
                                Missing,
                                format!("Gold {gold} is not in pool {gold_pool}; {INSTALL}"),
                            )
                        }
                    }
                    Err(error) => check(GoldInPool, Unknown, format!("Cannot read pool {gold_pool}: {error}")),
                }
            } else {
                check(
                    GoldInPool,
                    Missing,
                    format!("Gold {gold} is not in pool {gold_pool}; {INSTALL}"),
                )
            });
        }
    }
    checks.push(check(
        Snippets,
        Unknown,
        format!(
            "Listing snippets needs Datastore.Allocate on /storage/{}, which the server does not get; \
             deploy/pve/homestack-pve.sh status checks them on every node",
            config.cloud_init.snippet_storage
        ),
    ));
    Installation {
        pool: pool.to_owned(),
        gold_pool: gold_pool.to_owned(),
        complete: checks.iter().all(|c| c.status != Missing),
        checks,
    }
}
