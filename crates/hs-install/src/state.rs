//! What `homestack-pve.sh load_state` reads from the cluster.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::InstallError;
use crate::probe::{Topology, flag, get, text};
use crate::root::Root;

pub const HS_USER: &str = "homestack@pve";

/// The pools HomeStack owns, whose members are read.
pub const POOLS: [&str; 4] = ["homestack", "homestack-test", "homestack-gold", "homestack-service"];

#[derive(Debug, Clone)]
pub struct Vm {
    pub name: String,
    pub status: String,
    pub node: String,
    /// `qemu` or `lxc`.
    pub kind: String,
    pub tags: Vec<String>,
    pub pool: String,
}

#[derive(Debug, Clone)]
pub struct Storage {
    pub kind: String,
    pub content: Vec<String>,
    pub shared: bool,
    pub nodes: Vec<String>,
    pub disabled: bool,
}

/// One ACL entry: `path`, `user` or `token` or `group`, who, role.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Acl {
    pub path: String,
    pub kind: String,
    pub who: String,
    pub role: String,
}

impl Acl {
    pub fn new(path: &str, kind: &str, who: &str, role: &str) -> Self {
        Self {
            path: path.to_owned(),
            kind: kind.to_owned(),
            who: who.to_owned(),
            role: role.to_owned(),
        }
    }

    /// HomeStack's user, or one of its tokens.
    pub fn managed(&self) -> bool {
        self.who == HS_USER || self.who.starts_with(&format!("{HS_USER}!"))
    }
}

#[derive(Debug)]
pub struct State {
    pub topology: Topology,
    pub local_node: String,
    pub vms: BTreeMap<u32, Vm>,
    pub storages: BTreeMap<String, Storage>,
    pub roles: BTreeMap<String, BTreeSet<String>>,
    pub acl: BTreeSet<Acl>,
    pub user_exists: bool,
    /// Tokens of the user and whether their privileges are separated.
    pub tokens: BTreeMap<String, bool>,
    /// Members (VM ids) of the HomeStack pools that exist.
    pub pools: BTreeMap<String, Vec<String>>,
}

fn list(value: &str, separators: &[char]) -> Vec<String> {
    value
        .split(|c| separators.contains(&c))
        .map(str::trim)
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

fn array(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items,
        Value::Object(_) => vec![value],
        _ => Vec::new(),
    }
}

pub fn privs(text: &str) -> BTreeSet<String> {
    list(text, &[','; 1]).into_iter().collect()
}

impl State {
    pub fn load(root: &mut dyn Root, topology: &Topology) -> Result<Self, InstallError> {
        let local_node = topology
            .local_node()
            .map(|node| node.name.clone())
            .ok_or_else(|| InstallError::failed("cannot tell which node this is (pvesh get /cluster/status)"))?;

        let mut vms = BTreeMap::new();
        for item in array(get(root, "/cluster/resources", " --type vm")?) {
            let Some(vmid) = item
                .get("vmid")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
            else {
                continue;
            };
            vms.insert(
                vmid,
                Vm {
                    name: text(item.get("name")),
                    status: text(item.get("status")),
                    node: text(item.get("node")),
                    kind: text(item.get("type")),
                    tags: list(&text(item.get("tags")), &[';', ',']),
                    pool: text(item.get("pool")),
                },
            );
        }

        let mut storages = BTreeMap::new();
        for item in array(get(root, "/storage", "")?) {
            storages.insert(
                text(item.get("storage")),
                Storage {
                    kind: text(item.get("type")),
                    content: list(&text(item.get("content")), &[',']),
                    shared: flag(item.get("shared")),
                    nodes: list(&text(item.get("nodes")), &[',']),
                    disabled: flag(item.get("disable")),
                },
            );
        }

        let mut roles = BTreeMap::new();
        for item in array(pveum_json(root, "role list")?) {
            roles.insert(text(item.get("roleid")), privs(&text(item.get("privs"))));
        }

        let mut acl = BTreeSet::new();
        for item in array(pveum_json(root, "acl list")?) {
            acl.insert(Acl::new(
                &text(item.get("path")),
                &text(item.get("type")),
                &text(item.get("ugid")),
                &text(item.get("roleid")),
            ));
        }

        let user_exists = array(pveum_json(root, "user list")?)
            .iter()
            .any(|item| text(item.get("userid")) == HS_USER);
        let mut tokens = BTreeMap::new();
        if user_exists {
            for item in array(pveum_json(root, &format!("user token list {HS_USER}"))?) {
                // A missing or empty privsep is 1, as the script reads it.
                let separated = match item.get("privsep") {
                    None | Some(Value::Null) => true,
                    other => text(other).is_empty() || flag(other),
                };
                tokens.insert(text(item.get("tokenid")), separated);
            }
        }

        let mut pools = BTreeMap::new();
        for item in array(get(root, "/pools", "")?) {
            let id = text(item.get("poolid"));
            if POOLS.contains(&id.as_str()) {
                // One pool comes as an array of one object, or as the object.
                let detail = get(root, "/pools", &format!(" --poolid {id}"))?;
                let detail = detail.as_array().and_then(|items| items.first()).unwrap_or(&detail);
                let members = detail
                    .get("members")
                    .and_then(Value::as_array)
                    .map(|members| {
                        members
                            .iter()
                            .map(|member| text(member.get("vmid")))
                            .filter(|vmid| !vmid.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                pools.insert(id, members);
            }
        }

        Ok(Self {
            topology: topology.clone(),
            local_node,
            vms,
            storages,
            roles,
            acl,
            user_exists,
            tokens,
            pools,
        })
    }

    /// The `qemu` configuration of a VM, as the node that holds it reports it.
    pub fn vm_config(&self, root: &mut dyn Root, vmid: u32) -> Result<Value, InstallError> {
        let vm = self
            .vms
            .get(&vmid)
            .ok_or_else(|| InstallError::failed(format!("VM {vmid} does not exist")))?;
        get(root, &format!("/nodes/{}/qemu/{vmid}/config", vm.node), "")
    }

    pub fn managed_acl(&self) -> BTreeSet<Acl> {
        self.acl.iter().filter(|entry| entry.managed()).cloned().collect()
    }
}

fn pveum_json(root: &mut dyn Root, what: &str) -> Result<Value, InstallError> {
    let command = format!("pveum {what} --output-format json");
    let output = root.run(&command)?;
    if output.code != 0 {
        return Err(InstallError::failed(format!("`{command}` failed: {}", output.detail())));
    }
    serde_json::from_str(output.stdout.trim())
        .map_err(|error| InstallError::failed(format!("`{command}` did not answer JSON ({error})")))
}
