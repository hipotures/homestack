//! The checks of what belongs to the cluster's owner: HomeStack only looks
//! (PVE version, storages and their content types, Gold and its bridge, tag
//! rules, the nodes' API certificates).

use serde_json::Value;

use crate::probe::{get, text};
use crate::root::Root;
use crate::settings::Settings;
use crate::shell::quote;
use crate::state::{State, Vm};

pub const MIN_MAJOR: u32 = 9;
pub const MIN_MINOR: u32 = 2;

#[derive(Debug, Default)]
pub struct Findings {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub infos: Vec<String>,
    pub gold: Option<u32>,
    pub sdn_path: String,
}

pub fn in_list(word: &str, list: &str) -> bool {
    list.split([',', ';']).any(|item| item.trim() == word)
}

fn has_tag(vm: &Vm, tag: &str) -> bool {
    vm.tags.iter().any(|item| item == tag)
}

pub fn check_versions(root: &mut dyn Root, state: &State, found: &mut Findings) {
    for node in state.topology.nodes.iter().filter(|node| node.online) {
        let Ok(answer) = get(root, &format!("/nodes/{}/version", node.name), "") else {
            found.errors.push(format!("{}: cannot read the PVE version", node.name));
            continue;
        };
        let version = text(answer.get("version"));
        let mut parts = version.split('.').map(|part| part.parse::<u32>().unwrap_or(0));
        let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        if major < MIN_MAJOR || (major == MIN_MAJOR && minor < MIN_MINOR) {
            found.errors.push(format!(
                "{} runs PVE {version}; HomeStack needs {MIN_MAJOR}.{MIN_MINOR} or later",
                node.name
            ));
        }
    }
}

/// The storages that allow `content`, as a user can choose them: id, type
/// and where it is.
fn offer(state: &State, content: &str) -> String {
    let found: Vec<String> = state
        .storages
        .iter()
        .filter(|(_, storage)| !storage.disabled && storage.content.iter().any(|item| item == content))
        .map(|(id, storage)| {
            let nodes = if storage.nodes.is_empty() {
                "all nodes".to_owned()
            } else {
                storage.nodes.join(",")
            };
            let shared = if storage.shared { ", shared" } else { "" };
            format!("{id} ({}, {nodes}{shared})", storage.kind)
        })
        .collect();
    if found.is_empty() {
        format!(
            "no storage allows content type {content}: the cluster's owner adds it (Datacenter > Storage > the storage > Content)"
        )
    } else {
        format!("choose from: {}", found.join(", "))
    }
}

fn check_storage(state: &State, found: &mut Findings, id: &str, content: &str) {
    match state.storages.get(id) {
        None => found
            .errors
            .push(format!("storage {id} does not exist; {}", offer(state, content))),
        Some(storage) if storage.disabled => found.errors.push(format!("storage {id} is disabled")),
        Some(storage) if !storage.content.iter().any(|item| item == content) => found.errors.push(format!(
            "storage {id} does not allow content type {content} (the cluster's owner adds it: Datacenter > Storage > {id} > Content); {}",
            offer(state, content)
        )),
        Some(_) => {}
    }
}

pub fn check_storages(state: &State, settings: &Settings, found: &mut Findings) {
    if settings.storages.is_empty() {
        found.errors.push(format!(
            "--storages is required: the storages of [storage_layouts], where the workspace disks go (several, separated by commas); {}",
            offer(state, "images")
        ));
    }
    for id in &settings.storages {
        check_storage(state, found, id, "images");
    }
    match &settings.snippet_storage {
        None => found.errors.push(format!(
            "--snippet-storage is required: [cloud_init] snippet_storage, where the Cloud-Init vendor snippets go; {}",
            offer(state, "snippets")
        )),
        Some(id) => check_storage(state, found, id, "snippets"),
    }
    if settings.service_storages.is_empty() {
        found.errors.push(format!(
            "--service-storages is required: the storages for the server containers' root disks (several, separated by commas); {}",
            offer(state, "rootdir")
        ));
    }
    for id in &settings.service_storages {
        check_storage(state, found, id, "rootdir");
    }
    match &settings.template_storage {
        None => found.errors.push(format!(
            "--template-storage is required: the storage for the server image; {}",
            offer(state, "vztmpl")
        )),
        Some(id) => check_storage(state, found, id, "vztmpl"),
    }
}

/// The VMs as a user can choose Gold: id, name and where it is.
fn vms(state: &State, only: impl Fn(&Vm) -> bool) -> String {
    const SHOWN: usize = 30;
    let all: Vec<String> = state
        .vms
        .iter()
        .filter(|(_, vm)| only(vm))
        .map(|(vmid, vm)| {
            let status = if vm.status.is_empty() {
                String::new()
            } else {
                format!(", {}", vm.status)
            };
            format!("{vmid} {} ({}{status})", vm.name, vm.node)
        })
        .collect();
    let mut shown = all.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    if all.len() > SHOWN {
        shown.push_str(&format!(" and {} more", all.len() - SHOWN));
    }
    shown
}

/// Gold: the one given, or the only VM tagged `homestack-gold`.
pub fn find_gold(state: &State, settings: &Settings, found: &mut Findings) -> Option<u32> {
    if let Some(vmid) = settings.gold {
        if state.vms.contains_key(&vmid) {
            return Some(vmid);
        }
        found.errors.push(format!(
            "Gold {vmid} does not exist; QEMU VMs: {}",
            vms(state, |vm| vm.kind == "qemu")
        ));
        return None;
    }
    let tagged: Vec<u32> = state
        .vms
        .iter()
        .filter(|(_, vm)| has_tag(vm, "homestack-gold"))
        .map(|(vmid, _)| *vmid)
        .collect();
    match tagged.as_slice() {
        [] => {
            found.errors.push(format!(
                "no VM is tagged homestack-gold: prepare Gold (README, Preparing a Gold VM) or pass --gold VMID with one of the QEMU VMs: {}",
                vms(state, |vm| vm.kind == "qemu")
            ));
            None
        }
        [one] => Some(*one),
        _ => {
            found.errors.push(format!(
                "several VMs are tagged homestack-gold: {}: pass --gold VMID",
                vms(state, |vm| has_tag(vm, "homestack-gold"))
            ));
            None
        }
    }
}

pub fn check_gold(root: &mut dyn Root, state: &State, settings: &Settings, found: &mut Findings) {
    let Some(gold) = find_gold(state, settings, found) else {
        return;
    };
    found.gold = Some(gold);
    let Some(vm) = state.vms.get(&gold) else {
        return;
    };
    if vm.kind != "qemu" {
        found.errors.push(format!("Gold {gold} is not a QEMU VM"));
        return;
    }
    if !vm.pool.is_empty() && vm.pool != "homestack-gold" {
        found.errors.push(format!(
            "Gold {gold} is in pool {0}; HomeStack needs it in homestack-gold (take it out of {0} first)",
            vm.pool
        ));
    }
    let Ok(config) = state.vm_config(root, gold) else {
        found.errors.push(format!("cannot read Gold {gold}'s configuration"));
        return;
    };
    let net0 = text(config.get("net0"));
    let bridge = net0
        .split(',')
        .find_map(|part| part.strip_prefix("bridge="))
        .unwrap_or_default()
        .to_owned();
    if bridge.is_empty() {
        found.errors.push(format!("Gold {gold}'s net0 has no bridge"));
        return;
    }
    let zone = get(root, "/cluster/sdn/vnets", "")
        .ok()
        .and_then(|vnets| {
            vnets.as_array().and_then(|items| {
                items
                    .iter()
                    .find(|item| text(item.get("vnet")) == bridge)
                    .map(|item| text(item.get("zone")))
            })
        })
        .filter(|zone| !zone.is_empty())
        .unwrap_or_else(|| "localnetwork".to_owned());
    found.sdn_path = format!("/sdn/zones/{zone}/{bridge}");
}

pub fn check_tags(root: &mut dyn Root, settings: &Settings, found: &mut Findings) {
    let Ok(options) = get(root, "/cluster/options", "") else {
        found.errors.push("cannot read /cluster/options".to_owned());
        return;
    };
    let registered = text(options.get("registered-tags"));
    let access = text(options.get("user-tag-access"));
    let setting = |key: &str| {
        access
            .split(',')
            .find_map(|part| part.strip_prefix(&format!("{key}=")))
            .unwrap_or_default()
            .to_owned()
    };
    let allow = setting("user-allow");
    let allow_list = setting("user-allow-list");
    let mut tags = vec!["homestack-ws", "homestack-lock"];
    if settings.test {
        tags.push("homestack-test");
    }
    for tag in tags {
        if in_list(tag, &registered) {
            found.errors.push(format!(
                "tag {tag} is in registered-tags, so only users with Sys.Modify on / may set it (Datacenter > Options > Tag Style)"
            ));
        } else if allow == "none" || (allow == "list" && !in_list(tag, &allow_list)) {
            found
                .errors
                .push(format!("tag {tag} is not allowed by user-tag-access ({access})"));
        } else if allow == "existing" {
            found.warnings.push(format!(
                "user-tag-access is 'existing': tag {tag} must already exist in the cluster"
            ));
        }
    }
}

pub fn check_certificates(root: &mut dyn Root, state: &State, found: &mut Findings) {
    for node in state.topology.nodes.iter().filter(|node| node.online) {
        let Ok(info) = get(root, &format!("/nodes/{}/certificates/info", node.name), "") else {
            found
                .warnings
                .push(format!("{}: cannot read the certificate information", node.name));
            continue;
        };
        let entries: &[Value] = info.as_array().map_or(&[], Vec::as_slice);
        let names_of = |file: &str| -> Vec<String> {
            entries
                .iter()
                .find(|entry| text(entry.get("filename")) == file)
                .and_then(|entry| entry.get("san"))
                .map(|san| match san {
                    Value::Array(items) => items.iter().map(|item| text(Some(item))).collect(),
                    other => text(Some(other)).split(',').map(str::to_owned).collect(),
                })
                .unwrap_or_default()
        };
        let file = if entries
            .iter()
            .any(|entry| text(entry.get("filename")) == "pveproxy-ssl.pem")
        {
            "pveproxy-ssl.pem"
        } else {
            "pve-ssl.pem"
        };
        let names = names_of(file);
        let Some(name) = names.iter().find(|name| name.starts_with(&format!("{}.", node.name))) else {
            let shown = if names.is_empty() {
                "none".to_owned()
            } else {
                names.join(",")
            };
            found.warnings.push(format!(
                "{0}: the API certificate ({file}) has no name {0}.<domain> (names: {shown}); the server must reach {0} by a name on its certificate",
                node.name
            ));
            continue;
        };
        let address = if node.ip.is_empty() {
            name.as_str()
        } else {
            node.ip.as_str()
        };
        let verified = root.run(&format!(
            "openssl s_client -connect {}:8006 -servername {n} -verify_hostname {n} -verify_return_error -brief < /dev/null > /dev/null 2>&1",
            quote(address),
            n = quote(name)
        ));
        if verified.is_ok_and(|output| output.code == 0) {
            found
                .infos
                .push(format!("{}: API certificate verifies for {name}", node.name));
        } else {
            found.warnings.push(format!(
                "{}: the API certificate does not verify for {name} with the system's CAs; give the server /etc/pve/pve-root-ca.pem in HOMESTACK_PVE_CA_FILE, or let Proxmox get a trusted one (ACME)",
                node.name
            ));
        }
        let resolves = root.run(&format!("getent hosts {} > /dev/null 2>&1", quote(name)));
        if !resolves.is_ok_and(|output| output.code == 0) {
            found.warnings.push(format!(
                "{}: {name} does not resolve on {}; the server needs it in its DNS or /etc/hosts",
                node.name, state.local_node
            ));
        }
    }
}
