//! Autodiscovery: a guest whose HomeStack tag names a pool, but which is in
//! no pool (an uninstall empties the pools and deletes them, the tags stay),
//! is offered to be put back in that pool. The tag decides, never the
//! address.

use crate::state::{POOL_SERVICE, POOL_TEST, POOL_WORKSPACES, State, Vm};
use crate::{Choice, Question};

/// A workspace VM made by HomeStack.
pub const WORKSPACE_TAG: &str = "homestack-ws";
/// A workspace VM that HomeStack does not refresh.
pub const ALIEN_TAG: &str = "homestack-alien";
/// A workspace of the test part (with one of the two above).
pub const TEST_TAG: &str = "homestack-test";
/// A server container (`homestack-deploy`).
pub const SERVER_TAG: &str = "homestack-server";
pub const GOLD_TAG: &str = "homestack-gold";

/// The `--adopt` option.
pub const OPTION: &str = "--adopt";

fn has(vm: &Vm, tag: &str) -> bool {
    vm.tags.iter().any(|item| item == tag)
}

/// The pool `vm`'s tags put it in. Gold is not here: it is the one chosen
/// for the install, not every VM tagged `homestack-gold`.
#[must_use]
pub fn pool_by_tags(vm: &Vm) -> Option<&'static str> {
    if vm.template || has(vm, GOLD_TAG) {
        return None;
    }
    match vm.kind.as_str() {
        "lxc" if has(vm, SERVER_TAG) => Some(POOL_SERVICE),
        "qemu" if has(vm, WORKSPACE_TAG) || has(vm, ALIEN_TAG) => {
            Some(if has(vm, TEST_TAG) { POOL_TEST } else { POOL_WORKSPACES })
        }
        _ => None,
    }
}

/// The pools an install of the part can put guests back in.
#[must_use]
pub const fn pools_of(test: bool) -> [&'static str; 2] {
    [if test { POOL_TEST } else { POOL_WORKSPACES }, POOL_SERVICE]
}

/// The guests in no pool whose tags put them in one of the part's pools,
/// with that pool, by VMID.
#[must_use]
pub fn found(state: &State, test: bool) -> Vec<(u32, &'static str)> {
    let pools = pools_of(test);
    state
        .vms
        .iter()
        .filter(|(_, vm)| vm.pool.is_empty())
        .filter_map(|(vmid, vm)| pool_by_tags(vm).map(|pool| (*vmid, pool)))
        .filter(|(_, pool)| pools.contains(pool))
        .collect()
}

/// Why `vmid` cannot be put back in a pool by the install of the part.
pub fn refusal(state: &State, test: bool, vmid: u32) -> Option<String> {
    let Some(vm) = state.vms.get(&vmid) else {
        return Some(format!("{OPTION} {vmid}: no such VM or container"));
    };
    if !vm.pool.is_empty() {
        return Some(format!("{OPTION} {vmid}: it is already in pool {}", vm.pool));
    }
    match pool_by_tags(vm) {
        None => Some(format!(
            "{OPTION} {vmid}: no tag puts it in a pool ({WORKSPACE_TAG} or {ALIEN_TAG} on a VM, {SERVER_TAG} on a container)"
        )),
        Some(pool) if !pools_of(test).contains(&pool) => Some(format!(
            "{OPTION} {vmid}: its tags put it in pool {pool}, which this part does not install"
        )),
        Some(_) => None,
    }
}

/// The question of the form: every guest found, each ticked.
#[must_use]
pub fn question(state: &State, test: bool) -> Option<Question> {
    let found = found(state, test);
    if found.is_empty() {
        return None;
    }
    let choices: Vec<Choice> = found
        .iter()
        .map(|(vmid, pool)| {
            let vm = &state.vms[vmid];
            Choice {
                value: vmid.to_string(),
                detail: format!("{:<16} {:<4} {:<8} -> pool {pool}", vm.name, vm.kind, vm.node),
            }
        })
        .collect();
    Some(Question {
        option: OPTION,
        title: "Found by their tags".to_owned(),
        note: "VMs and containers with a HomeStack tag that are in no pool, as an uninstall leaves them. Each \
ticked one is put in the pool its tag names; nothing else of it changes."
            .to_owned(),
        multiple: true,
        defaults: (0..choices.len()).collect(),
        choices,
    })
}
