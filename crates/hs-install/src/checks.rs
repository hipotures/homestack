//! The checks of what belongs to the cluster's owner: HomeStack only looks
//! (PVE version, storages and their content types, Gold and its bridge, tag
//! rules, the nodes' API certificates).

use serde_json::Value;

use crate::probe::{get, text};
use crate::root::Root;
use crate::settings::Settings;
use crate::shell::quote;
use crate::state::{POOL_TEST, State, Vm, gold_pool};
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
    /// Node -> the name on its API certificate.
    pub api_names: std::collections::BTreeMap<String, String>,
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

/// The storages that can hold `content`, as choices: id, type and nodes.
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

/// Storage types that hold files rather than disks: a backup directory or a
/// network share is a poor default for VM and container disks.
const FILE_STORAGES: [&str; 6] = ["dir", "nfs", "cifs", "cephfs", "glusterfs", "pbs"];

/// The choices ticked at first: for disks the block storages, for one file
/// (snippets, the image) `local` when it is there.
fn defaults(state: &State, content: &str, choices: &[Choice]) -> Vec<usize> {
    match content {
        "images" | "rootdir" => choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| {
                state
                    .storages
                    .get(&choice.value)
                    .is_some_and(|storage| !FILE_STORAGES.contains(&storage.kind.as_str()))
            })
            .map(|(index, _)| index)
            .collect(),
        _ => choices
            .iter()
            .position(|choice| choice.value == "local")
            .into_iter()
            .collect(),
    }
}

/// Options not given that have exactly one possible value get it, so the
/// user is not asked to choose from one; what was taken is reported.
pub fn resolve(state: &State, settings: &Settings) -> (Settings, Vec<String>) {
    let mut resolved = settings.clone();
    let mut taken = Vec::new();
    let mut only = |content: &str, what: &str| -> Option<String> {
        let choices = storage_choices(state, content);
        if let [one] = choices.as_slice() {
            taken.push(format!("{what}: {} (the only storage that can hold it)", one.value));
            return Some(one.value.clone());
        }
        None
    };
    if resolved.storages.is_empty()
        && let Some(id) = only("images", "Workspace disks")
    {
        resolved.storages = vec![id];
    }
    if resolved.snippet_storage.is_none() {
        resolved.snippet_storage = only("snippets", "Cloud-Init snippets");
    }
    if resolved.service_storages.is_empty()
        && let Some(id) = only("rootdir", "Server disks")
    {
        resolved.service_storages = vec![id];
    }
    if resolved.template_storage.is_none() {
        resolved.template_storage = only("vztmpl", "Server image");
    }
    if resolved.bridge.is_none()
        && let Some(bridge) = state.bridges.iter().next().filter(|_| state.bridges.len() == 1)
    {
        resolved.bridge = Some(bridge.clone());
        taken.push(format!(
            "Server bridge: {bridge} (the only bridge present on every online node)"
        ));
    }
    (resolved, taken)
}

/// Infer the snippet storage for an uninstall when none was explicitly passed:
/// a unique snippet-capable storage carrying a HomeStack storage ACL, or the
/// only snippet-capable storage on the cluster.
pub fn uninstall_snippet_storage(state: &State) -> Option<String> {
    let choices = storage_choices(state, "snippets");
    let acl = state.managed_acl();
    let managed: Vec<&Choice> = choices
        .iter()
        .filter(|choice| {
            let path = format!("/storage/{}", choice.value);
            acl.iter().any(|entry| entry.path == path)
        })
        .collect();
    if let [one] = managed.as_slice() {
        return Some(one.value.clone());
    }
    if let [one] = choices.as_slice() {
        return Some(one.value.clone());
    }
    None
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

fn ask_storage(
    state: &State,
    found: &mut Findings,
    option: &'static str,
    (title, note): (&str, &str),
    content: &str,
    multiple: bool,
) {
    let choices = storage_choices(state, content);
    if choices.is_empty() {
        found.errors.push(format!(
            "{option} is required, but no storage allows content type {content}: the cluster's owner adds it (Datacenter > Storage > the storage > Content)"
        ));
    } else {
        let defaults = defaults(state, content, &choices);
        found.questions.push(Question {
            option,
            title: title.to_owned(),
            note: note.to_owned(),
            multiple,
            choices,
            defaults,
        });
    }
}

pub fn check_storages(state: &State, settings: &Settings, found: &mut Findings) {
    if settings.storages.is_empty() {
        ask_storage(
            state,
            found,
            "--storages",
            (
                "Storages for the workspace disks",
                "Where HomeStack creates the disks of the workspace VMs (root and home). It gets permission to use only the storages you tick.",
            ),
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
            (
                "Storage for the Cloud-Init snippets",
                "Where HomeStack keeps the two small Cloud-Init files that set up every new workspace (hostname, SSH keys, home disk). They are written on every node that has this storage, once if it is shared.",
            ),
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
            (
                "Storages for the server root disks",
                "Where the disks of the HomeStack server containers are created: the server that clients talk to runs in a container on each node.",
            ),
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
            (
                "Storage for the server image",
                "Where the server image (the HomeStack program that runs in a container) is stored on the nodes before it becomes a container.",
            ),
            "vztmpl",
            false,
        ),
        Some(id) => check_storage(state, found, id, "vztmpl"),
    }
}

fn bridge_question(state: &State) -> Question {
    Question {
        option: "--bridge",
        title: "Server network bridge".to_owned(),
        note: "The existing bridge used by the HomeStack containers; it must be present on every online node. This is selected independently of Gold.".to_owned(),
        multiple: false,
        choices: state.bridges.iter().map(|name| Choice { value: name.clone(), detail: "bridge on every online node".to_owned() }).collect(),
        defaults: if state.bridges.len() == 1 { vec![0] } else { Vec::new() },
    }
}

pub fn check_bridge(root: &mut dyn Root, state: &State, settings: &Settings, found: &mut Findings) {
    let Some(bridge) = &settings.bridge else {
        if state.bridges.is_empty() {
            found.errors.push("No network bridge is present on every online node; create a bridge for the HomeStack containers first.".to_owned());
        } else {
            found.questions.push(bridge_question(state));
        }
        return;
    };
    if !state.bridges.contains(bridge) {
        found
            .errors
            .push(format!("Bridge {bridge} is not present on every online node"));
        return;
    }
    let zone = get(root, "/cluster/sdn/vnets", "")
        .ok()
        .and_then(|vnets| {
            vnets.as_array().and_then(|items| {
                items
                    .iter()
                    .find(|item| text(item.get("vnet")) == *bridge)
                    .map(|item| text(item.get("zone")))
            })
        })
        .filter(|zone| !zone.is_empty())
        .unwrap_or_else(|| "localnetwork".to_owned());
    found.sdn_path = format!("/sdn/zones/{zone}/{bridge}");
}

/// The VMs that pass `only`, as choices for Gold: id, name, node, state.
fn vm_choices(state: &State, only: impl Fn(&Vm) -> bool) -> Vec<Choice> {
    state
        .vms
        .iter()
        .filter(|(_, vm)| only(vm))
        .map(|(vmid, vm)| Choice {
            value: vmid.to_string(),
            detail: format!("{:<16} {:<6} {}", vm.name, vm.node, vm.status)
                .trim_end()
                .to_owned(),
        })
        .collect()
}

/// Gold is optional. Use the explicit selection or an unambiguous detected Gold.
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
    // Each part has its own Gold: the test part's is the one in its pool
    // (cloned there by hand), the production part's anywhere else.
    let of_part = |vm: &Vm| {
        if settings.test {
            vm.pool == POOL_TEST
        } else {
            vm.pool != POOL_TEST
        }
    };
    let tagged: Vec<u32> = state
        .vms
        .iter()
        .filter(|(_, vm)| has_tag(vm, "homestack-gold") && of_part(vm))
        .map(|(vmid, _)| *vmid)
        .collect();
    if let [one] = tagged.as_slice() {
        return Some(*one);
    }
    // The default Gold is the VM named `hs-gold`; the installer tags it.
    let named: Vec<u32> = state
        .vms
        .iter()
        .filter(|(_, vm)| vm.kind == "qemu" && vm.name == DEFAULT_GOLD_NAME && of_part(vm))
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
    found.warnings.push(if tagged.is_empty() {
        "Gold is not configured; install HomeStack now and select a Gold VM before creating or refreshing workspaces.".to_owned()
    } else {
        "Several VMs are tagged homestack-gold; no Gold was selected. Select one before creating or refreshing workspaces.".to_owned()
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
    let pool = gold_pool(settings.test);
    if !vm.pool.is_empty() && vm.pool != pool {
        found.errors.push(format!(
            "Gold {gold} is in pool {0}; this part needs it in {pool} (take it out of {0} first)",
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
    if let Some(selected) = &settings.bridge
        && *selected != bridge
    {
        found.errors.push(format!(
            "Gold {gold} uses bridge {bridge}, but the selected server bridge is {selected}"
        ));
    }
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
    let mut tags = vec![
        crate::discover::WORKSPACE_TAG,
        "homestack-lock",
        crate::discover::SERVER_TAG,
    ];
    if settings.test {
        tags.push(crate::discover::TEST_TAG);
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

fn api_name(node: &str, names: &[String]) -> Option<String> {
    names
        .iter()
        .find(|name| name.starts_with(&format!("{node}.")) && !name.contains('*'))
        .cloned()
        .or_else(|| {
            names.iter().find_map(|name| {
                name.strip_prefix("*.")
                    .filter(|suffix| !suffix.is_empty() && !suffix.contains('*'))
                    .map(|suffix| format!("{node}.{suffix}"))
            })
        })
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
        let Some(name) = api_name(&node.name, &names) else {
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
        found.api_names.insert(node.name.clone(), name.clone());
        let address = if node.ip.is_empty() {
            name.as_str()
        } else {
            node.ip.as_str()
        };
        let verified = root.run(&format!(
            "openssl s_client -connect {}:8006 -servername {n} -verify_hostname {n} -verify_return_error -brief < /dev/null > /dev/null 2>&1",
            quote(address),
            n = quote(&name)
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
        let resolves = root.run(&format!("getent hosts {} > /dev/null 2>&1", quote(&name)));
        if !resolves.is_ok_and(|output| output.code == 0) {
            found.warnings.push(format!(
                "{}: {name} does not resolve on {}; the server needs it in its DNS or /etc/hosts",
                node.name, state.local_node
            ));
        }
    }
}

/// Every option the user has not given, as a question with its choices and
/// what is chosen at first: for the installer's form, where every setting is
/// shown and can be changed. Gold's first choice is the one the installer
/// would take by itself (the only tagged VM, or the VM named `hs-gold`).
pub fn all_questions(state: &State, settings: &Settings) -> Vec<Question> {
    let mut found = Findings::default();
    let mut questions = Vec::new();
    let storages: [(&'static str, &str, &str, bool, bool); 4] = [
        (
            "--storages",
            "Workspace disks",
            "images",
            true,
            settings.storages.is_empty(),
        ),
        (
            "--snippet-storage",
            "Cloud-Init snippets",
            "snippets",
            false,
            settings.snippet_storage.is_none(),
        ),
        (
            "--service-storages",
            "Server disks",
            "rootdir",
            true,
            settings.service_storages.is_empty(),
        ),
        (
            "--template-storage",
            "Server image",
            "vztmpl",
            false,
            settings.template_storage.is_none(),
        ),
    ];
    let notes = [
        "Where HomeStack creates the disks of the workspace VMs (root and home). It gets permission to use only the storages you tick.",
        "Where HomeStack keeps the two small Cloud-Init files that set up every new workspace. They are written on every node that has this storage, once if it is shared.",
        "Where the disks of the HomeStack server containers are created (the server runs in a container on each node).",
        "Where the server image is stored on the nodes before it becomes a container.",
    ];
    for ((option, title, content, multiple, open), note) in storages.into_iter().zip(notes) {
        if !open {
            continue;
        }
        let choices = storage_choices(state, content);
        let mut ticked = defaults(state, content, &choices);
        if ticked.is_empty() && choices.len() == 1 {
            ticked = vec![0];
        }
        questions.push(Question {
            option,
            title: title.to_owned(),
            note: note.to_owned(),
            multiple,
            choices,
            defaults: ticked,
        });
    }
    if settings.bridge.is_none() {
        questions.push(bridge_question(state));
    }
    if settings.gold.is_none() {
        let chosen = find_gold(state, settings, &mut found);
        let choices = vm_choices(state, |vm| vm.kind == "qemu" && !vm.template);
        let defaults = chosen
            .and_then(|vmid| choices.iter().position(|choice| choice.value == vmid.to_string()))
            .into_iter()
            .collect();
        questions.push(Question {
            option: "--gold",
            title: "Gold VM".to_owned(),
            note: format!("Optional. Install without Gold and configure it later, before creating or refreshing workspaces. A selected base VM is tagged homestack-gold and put in pool {}.", gold_pool(settings.test)),
            multiple: false,
            choices,
            defaults,
        });
    }
    if settings.adopt.is_empty()
        && let Some(question) = crate::discover::question(state, settings.test)
    {
        questions.push(question);
    }
    questions
}
