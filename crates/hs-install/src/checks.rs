//! The checks of what belongs to the cluster's owner: HomeStack only looks
//! (PVE version, storages and their content types, Gold and its bridge, tag
//! rules, the nodes' API certificates).

use serde_json::Value;

use crate::probe::{get, text};
use crate::root::Root;
use crate::settings::Settings;
use crate::shell::quote;
use crate::state::{State, Vm};
use crate::{Choice, Question};

/// The name of the VM that is Gold when none is chosen.
pub const DEFAULT_GOLD_NAME: &str = "hs-gold";
pub const MIN_MAJOR: u32 = 9;
pub const MIN_MINOR: u32 = 2;

#[derive(Debug, Default)]
pub struct Findings {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub infos: Vec<String>,
    pub questions: Vec<Question>,
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

/// The storages that allow `content`, as choices: id, type and nodes.
fn storage_choices(state: &State, content: &str) -> Vec<Choice> {
    state
        .storages
        .iter()
        .filter(|(_, storage)| !storage.disabled && storage.content.iter().any(|item| item == content))
        .map(|(id, storage)| {
            let nodes = if storage.nodes.is_empty() {
                "all nodes".to_owned()
            } else {
                storage.nodes.join(",")
            };
            let shared = if storage.shared { " (shared)" } else { "" };
            Choice {
                value: id.clone(),
                detail: format!("{:<8} {nodes}{shared}", storage.kind),
            }
        })
        .collect()
}

fn check_storage(state: &State, found: &mut Findings, id: &str, content: &str) {
    match state.storages.get(id) {
        None => found.errors.push(format!(
            "storage {id} does not exist (leave the option out to see the choices)"
        )),
        Some(storage) if storage.disabled => found.errors.push(format!("storage {id} is disabled")),
        Some(storage) if !storage.content.iter().any(|item| item == content) => found.errors.push(format!(
            "storage {id} does not allow content type {content} (the cluster's owner adds it: Datacenter > Storage > {id} > Content)"
        )),
        Some(_) => {}
    }
}

fn ask_storage(state: &State, found: &mut Findings, option: &'static str, text: &str, content: &str, multiple: bool) {
    let choices = storage_choices(state, content);
    if choices.is_empty() {
        found.errors.push(format!(
            "{option} is required, but no storage allows content type {content}: the cluster's owner adds it (Datacenter > Storage > the storage > Content)"
        ));
    } else {
        found.questions.push(Question {
            option,
            text: text.to_owned(),
            multiple,
            choices,
        });
    }
}

pub fn check_storages(state: &State, settings: &Settings, found: &mut Findings) {
    if settings.storages.is_empty() {
        ask_storage(
            state,
            found,
            "--storages",
            "where the workspace disks go (several, comma-separated)",
            "images",
            true,
        );
    }
    for id in &settings.storages {
        check_storage(state, found, id, "images");
    }
    match &settings.snippet_storage {
        None => ask_storage(
            state,
            found,
            "--snippet-storage",
            "where the Cloud-Init snippets go",
            "snippets",
            false,
        ),
        Some(id) => check_storage(state, found, id, "snippets"),
    }
    if settings.service_storages.is_empty() {
        ask_storage(
            state,
            found,
            "--service-storages",
            "root disks of the server containers (several, comma-separated)",
            "rootdir",
            true,
        );
    }
    for id in &settings.service_storages {
        check_storage(state, found, id, "rootdir");
    }
    match &settings.template_storage {
        None => ask_storage(
            state,
            found,
            "--template-storage",
            "where the server image is kept",
            "vztmpl",
            false,
        ),
        Some(id) => check_storage(state, found, id, "vztmpl"),
    }
}

/// The VMs that pass `only`, as choices for Gold: id, name, node, state.
fn vm_choices(state: &State, only: impl Fn(&Vm) -> bool) -> Vec<Choice> {
    const SHOWN: usize = 30;
    state
        .vms
        .iter()
        .filter(|(_, vm)| only(vm))
        .take(SHOWN)
        .map(|(vmid, vm)| Choice {
            value: vmid.to_string(),
            detail: format!("{:<16} {:<6} {}", vm.name, vm.node, vm.status)
                .trim_end()
                .to_owned(),
        })
        .collect()
}

/// Gold: the one given, or the only VM tagged `homestack-gold`; otherwise a
/// question with the candidates.
pub fn find_gold(state: &State, settings: &Settings, found: &mut Findings) -> Option<u32> {
    if let Some(vmid) = settings.gold {
        if state.vms.contains_key(&vmid) {
            return Some(vmid);
        }
        found.errors.push(format!(
            "Gold {vmid} does not exist (leave --gold out to see the QEMU VMs)"
        ));
        return None;
    }
    let tagged: Vec<u32> = state
        .vms
        .iter()
        .filter(|(_, vm)| has_tag(vm, "homestack-gold"))
        .map(|(vmid, _)| *vmid)
        .collect();
    if let [one] = tagged.as_slice() {
        return Some(*one);
    }
    // The default Gold is the VM named `hs-gold`; the installer tags it.
    let named: Vec<u32> = state
        .vms
        .iter()
        .filter(|(_, vm)| vm.kind == "qemu" && vm.name == DEFAULT_GOLD_NAME)
        .map(|(vmid, _)| *vmid)
        .collect();
    if let [one] = named.as_slice() {
        let others: Vec<String> = tagged.iter().map(u32::to_string).collect();
        if !others.is_empty() {
            found.warnings.push(format!(
                "Gold is VM {one} ({DEFAULT_GOLD_NAME}); VM {} also tagged homestack-gold, left as it is (pass --gold to choose another)",
                others.join(", ")
            ));
        }
        return Some(*one);
    }
    let (text, choices) = match tagged.as_slice() {
        [one] => return Some(*one),
        [] => (
            "no VM is tagged homestack-gold (prepare Gold first, see the README); QEMU VMs".to_owned(),
            vm_choices(state, |vm| vm.kind == "qemu"),
        ),
        _ => (
            "several VMs are tagged homestack-gold".to_owned(),
            vm_choices(state, |vm| has_tag(vm, "homestack-gold")),
        ),
    };
    found.questions.push(Question {
        option: "--gold",
        text,
        multiple: false,
        choices,
    });
    None
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
    for role in ["homestack-ws", "homestack-alien"] {
        if has_tag(vm, role) {
            found.errors.push(format!(
                "Gold {gold} is tagged {role}, a workspace role; take that tag off before it can be Gold"
            ));
        }
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
