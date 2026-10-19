//! The plan of an install or uninstall: what would change, in order. The
//! rules are those of `deploy/pve/homestack-pve.sh` (phase 4b): HomeStack
//! owns its user, tokens, roles, pools, ACL entries and vendor snippets;
//! everything else is only checked.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::InstallError;
use crate::checks::{self, Findings};
use crate::discover;
use crate::probe::{Access, Topology};
use crate::root::Root;
use crate::settings::{Command, Settings};
use crate::shell::{join, quote};
use crate::snippets::{self, SNIPPETS};
use crate::state::{Acl, HS_USER, POOL_GOLD, POOL_SERVICE, POOL_TEST, POOL_WORKSPACES, State, gold_pool, privs};

const DEPLOY_TOKEN: &str = "deploy";

/// The roles HomeStack owns and their privileges.
const ROLES: [(&str, &str); 6] = [
    (
        "HomeStack",
        "Pool.Audit,VM.Allocate,VM.Audit,VM.Config.Cloudinit,VM.Config.Disk,VM.Config.Options,VM.GuestAgent.Audit,VM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted,VM.Migrate,VM.PowerMgmt",
    ),
    (
        "HomeStackGold",
        "Pool.Audit,VM.Audit,VM.Clone,VM.Config.Options,VM.GuestAgent.Audit,VM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted,VM.PowerMgmt",
    ),
    ("HomeStackStorage", "Datastore.AllocateSpace,Datastore.Audit"),
    (
        "HomeStackDeploy",
        "Pool.Audit,VM.Allocate,VM.Audit,VM.Config.CPU,VM.Config.Disk,VM.Config.Memory,VM.Config.Network,VM.Config.Options,VM.PowerMgmt",
    ),
    ("HomeStackDeployNodes", "Sys.AccessNetwork"),
    (
        "HomeStackDeployTemplates",
        "Datastore.AllocateSpace,Datastore.AllocateTemplate,Datastore.Audit",
    ),
];

/// Roles made by hand before the installer (phases 0-4); removed once unused.
const LEGACY_ROLES: [&str; 5] = [
    "HomeStackServerDisks",
    "HomeStackServerGoldGuest",
    "HomeStackTest",
    "HomeStackTestGold",
    "HomeStackTestStorage",
];

/// What one step does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// `pveum ARGS` on the node.
    Pveum(Vec<String>),
    /// `pvesh ARGS` on the node (a change of a VM's tags).
    Pvesh(Vec<String>),
    /// A new token; its secret goes to `file` on the desktop.
    CreateToken {
        token: String,
        comment: String,
        file: PathBuf,
        /// Where a file left at `file` by an earlier installation is moved
        /// first (its token no longer exists); it is never overwritten.
        replaced: Option<PathBuf>,
    },
    /// A vendor snippet written on a node.
    WriteSnippet { node: String, name: String, path: String },
    /// A vendor snippet removed from a node.
    RemoveSnippet { node: String, name: String, path: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub description: String,
    pub action: Action,
}

impl Step {
    /// The command the step runs, for the plan.
    #[must_use]
    pub fn command_line(&self) -> String {
        match &self.action {
            Action::Pveum(args) => format!("pveum {}", join(args)),
            Action::Pvesh(args) => format!("pvesh {}", join(args)),
            Action::CreateToken {
                token, comment, file, ..
            } => format!(
                "pveum user token add {HS_USER} {token} --privsep 1 --comment {} (secret to {})",
                quote(comment),
                file.display()
            ),
            Action::WriteSnippet { node, name, .. } => format!("write {name} on {node}"),
            Action::RemoveSnippet { node, name, .. } => format!("remove {name} from {node}"),
        }
    }
}

/// What the deployment of the servers needs to know of the cluster
/// (section 8 of `docs/migration/phase-13c.md`), read with the plan.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cluster {
    /// The online nodes, in the cluster's order.
    pub nodes: Vec<String>,
    /// Each enabled storage: its content types and the nodes it is on
    /// (empty: every node).
    pub storages: BTreeMap<String, (Vec<String>, Vec<String>)>,
    /// Every guest: id -> (name, `qemu` or `lxc`, pool).
    pub guests: BTreeMap<u32, (String, String, String)>,
    /// The selected server bridge.
    pub bridge: String,
    /// Node -> the name on its API certificate.
    pub api_names: BTreeMap<String, String>,
    pub gold: Option<u32>,
    /// The node hosting the selected Gold, for workspace clone operations.
    pub gold_node: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub command: Command,
    /// "production" or "test".
    pub part: &'static str,
    pub host: String,
    pub version: String,
    pub topology: Topology,
    /// Facts found out while checking, shown as `ok:` lines.
    pub infos: Vec<String>,
    pub warnings: Vec<String>,
    /// What an uninstall leaves, and why.
    pub kept: Vec<String>,
    /// Things for the user to know after the plan, not Proxmox's business.
    pub notes: Vec<String>,
    /// Nodes that did not answer: the rest is done, the command fails.
    pub node_errors: Vec<String>,
    pub steps: Vec<Step>,
    /// The options the plan was made with, the given and the taken ones.
    pub settings: Settings,
    /// Options that had one possible value and got it, as sentences.
    pub taken: Vec<String>,
    /// The cluster as the servers need it.
    pub cluster: Cluster,
    /// Gold, as `VMID name (node)`, when the install has one.
    pub gold: Option<String>,
}

impl Plan {
    /// The plan as text (what the script printed).
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        for warning in &self.warnings {
            out.push_str(&format!("warning: {warning}\n"));
        }
        for info in &self.infos {
            out.push_str(&format!("ok: {info}\n"));
        }
        for kept in &self.kept {
            out.push_str(&format!("kept: {kept}\n"));
        }
        if self.steps.is_empty() {
            out.push_str(&format!("Nothing to change ({} part).\n", self.part));
        } else {
            out.push_str(&format!("Plan ({}, {} part):\n", self.command.name(), self.part));
            for (index, step) in self.steps.iter().enumerate() {
                out.push_str(&format!(
                    "{:>3}. {}\n     $ {}\n",
                    index + 1,
                    step.description,
                    step.command_line()
                ));
            }
        }
        for note in &self.notes {
            out.push_str(&format!("{note}\n"));
        }
        for error in &self.node_errors {
            out.push_str(&format!("error: {error}\n"));
        }
        out
    }
}

/// The name of the secret file of a token on the desktop.
#[must_use]
pub fn token_file_name(token: &str) -> String {
    match token {
        "server" => "pve-token".to_owned(),
        other => format!("pve-token-{other}"),
    }
}

fn pveum(description: impl Into<String>, args: &[&str]) -> Step {
    Step {
        description: description.into(),
        action: Action::Pveum(args.iter().map(|arg| (*arg).to_owned()).collect()),
    }
}

// --- the ACL -------------------------------------------------------------

struct Names<'a> {
    settings: &'a Settings,
    token: &'a str,
    sdn_path: &'a str,
}

fn acl_token(path: &str, token: &str, role: &str) -> Acl {
    Acl::new(path, "token", &format!("{HS_USER}!{token}"), role)
}

fn token_target_acl(names: &Names) -> BTreeSet<Acl> {
    let mut entries = BTreeSet::new();
    for path in ["/nodes", "/storage", "/sdn"] {
        entries.insert(acl_token(path, names.token, "PVEAuditor"));
    }
    let pools: &[(&str, &str)] = if names.settings.test {
        &[(POOL_TEST, "HomeStack"), (POOL_TEST, "HomeStackGold")]
    } else {
        &[
            (POOL_WORKSPACES, "HomeStack"),
            (POOL_GOLD, "HomeStackGold"),
            (POOL_TEST, "HomeStack"),
            (POOL_TEST, "HomeStackGold"),
            (POOL_SERVICE, "HomeStackDeploy"),
        ]
    };
    for (pool, role) in pools {
        entries.insert(acl_token(&format!("/pool/{pool}"), names.token, role));
    }
    for id in names
        .settings
        .storages
        .iter()
        .chain(names.settings.snippet_storage.iter())
    {
        entries.insert(acl_token(&format!("/storage/{id}"), names.token, "HomeStackStorage"));
    }
    entries.insert(acl_token(names.sdn_path, names.token, "PVESDNUser"));
    entries
}

fn deploy_target_acl(names: &Names) -> BTreeSet<Acl> {
    let token = DEPLOY_TOKEN;
    let mut entries = BTreeSet::new();
    for path in ["/nodes", "/storage", "/sdn"] {
        entries.insert(acl_token(path, token, "PVEAuditor"));
    }
    entries.insert(acl_token("/nodes", token, "HomeStackDeployNodes"));
    entries.insert(acl_token(&format!("/pool/{POOL_SERVICE}"), token, "HomeStackDeploy"));
    let template = names.settings.template_storage.clone().unwrap_or_default();
    entries.insert(acl_token(
        &format!("/storage/{template}"),
        token,
        "HomeStackDeployTemplates",
    ));
    for id in &names.settings.service_storages {
        if *id != template {
            entries.insert(acl_token(&format!("/storage/{id}"), token, "HomeStackStorage"));
        }
    }
    entries.insert(acl_token(names.sdn_path, token, "PVESDNUser"));
    entries
}

fn parent(path: &str) -> String {
    match path.rsplit_once('/') {
        Some(("", _)) | None => "/".to_owned(),
        Some((rest, _)) => rest.to_owned(),
    }
}

/// The user's entries for the token entries: on every path a token has an
/// entry on, the union of what each token resolves to there (its own entry
/// or the nearest parent's), since a token gets the intersection with its
/// user and a user's entry replaces what the user inherits.
fn user_acl(tokens: &BTreeSet<Acl>) -> BTreeSet<Acl> {
    let mut roles: BTreeMap<(&str, &str), BTreeSet<&str>> = BTreeMap::new();
    for entry in tokens {
        roles
            .entry((entry.who.as_str(), entry.path.as_str()))
            .or_default()
            .insert(entry.role.as_str());
    }
    let paths: BTreeSet<&str> = tokens.iter().map(|entry| entry.path.as_str()).collect();
    let who: BTreeSet<&str> = tokens.iter().map(|entry| entry.who.as_str()).collect();
    let mut out = BTreeSet::new();
    for path in &paths {
        for token in &who {
            let mut at = (*path).to_owned();
            loop {
                if let Some(found) = roles.get(&(*token, at.as_str())) {
                    for role in found {
                        out.insert(Acl::new(path, "user", HS_USER, role));
                    }
                    break;
                }
                if at == "/" {
                    break;
                }
                at = parent(&at);
            }
        }
    }
    out
}

/// The entries of the user and its tokens after this run.
fn target_acl(state: &State, names: &Names, command: Command, keep_user: bool, scope: &[&str]) -> BTreeSet<Acl> {
    let mut tokens: BTreeSet<Acl> = state
        .managed_acl()
        .into_iter()
        .filter(|entry| entry.kind == "token")
        .filter(|entry| {
            let id = entry.who.split_once('!').map_or("", |(_, id)| id);
            !scope.contains(&id)
        })
        .collect();
    if command == Command::Install {
        tokens.extend(token_target_acl(names));
        tokens.extend(deploy_target_acl(names));
    }
    if keep_user {
        let user = user_acl(&tokens);
        tokens.extend(user);
    }
    tokens
}

/// Plan ACL changes from the current to the target entries, grouped per
/// path and principal: additions for the user first, removals for tokens
/// first.
fn plan_acl(state: &State, target: &BTreeSet<Acl>) -> Vec<Step> {
    let current = state.managed_acl();
    let mut steps = Vec::new();
    for add in [true, false] {
        let changes: Vec<&Acl> = if add {
            target.difference(&current).collect()
        } else {
            current.difference(target).collect()
        };
        let mut groups: BTreeMap<(u8, String, String, String), Vec<&str>> = BTreeMap::new();
        for entry in changes {
            // Users before tokens when adding, tokens before users when removing.
            let order = u8::from((entry.kind == "user") != add);
            groups
                .entry((order, entry.path.clone(), entry.kind.clone(), entry.who.clone()))
                .or_default()
                .push(&entry.role);
        }
        for ((_, path, kind, who), mut roles) in groups {
            roles.sort_unstable();
            let roles = roles.join(",");
            let flag = if kind == "token" { "--tokens" } else { "--users" };
            steps.push(if add {
                pveum(
                    format!("give {who} {roles} on {path}"),
                    &["acl", "modify", &path, flag, &who, "--roles", &roles],
                )
            } else {
                pveum(
                    format!("take {roles} on {path} from {who}"),
                    &["acl", "delete", &path, flag, &who, "--roles", &roles],
                )
            });
        }
    }
    steps
}

/// Roles any ACL entry uses after this run.
fn roles_in_use(state: &State, target: &BTreeSet<Acl>) -> BTreeSet<String> {
    state
        .acl
        .iter()
        .filter(|entry| !entry.managed())
        .map(|entry| entry.role.clone())
        .chain(target.iter().map(|entry| entry.role.clone()))
        .collect()
}

// --- the plan ------------------------------------------------------------

struct Context<'a> {
    state: &'a State,
    settings: &'a Settings,
    names: Names<'a>,
    gold: Option<u32>,
    /// Snippet name -> where it is.
    paths: BTreeMap<&'static str, String>,
    /// Node -> the checksums of its snippets, by path.
    sums: BTreeMap<String, BTreeMap<String, String>>,
    snippet_nodes: Vec<String>,
    token: &'static str,
    pool: &'static str,
    part: &'static str,
}

fn plan_token(ctx: &Context, id: &str, comment: &str, steps: &mut Vec<Step>) -> Result<(), InstallError> {
    match ctx.state.tokens.get(id) {
        None => {
            let file = ctx.settings.token_dir.join(token_file_name(id));
            // A file there belongs to a token that no longer exists: it is
            // kept under another name, never overwritten.
            let replaced = file.exists().then(|| aside(&file));
            let mut description = format!("create token {HS_USER}!{id} (secret to {}, mode 0600)", file.display());
            if let Some(old) = &replaced {
                description.push_str(&format!(
                    "; the old file, whose token no longer exists, is kept as {}",
                    old.display()
                ));
            }
            steps.push(Step {
                description,
                action: Action::CreateToken {
                    token: id.to_owned(),
                    comment: comment.to_owned(),
                    file,
                    replaced,
                },
            });
        }
        Some(false) => steps.push(pveum(
            format!("separate the privileges of token {HS_USER}!{id}"),
            &["user", "token", "modify", HS_USER, id, "--privsep", "1"],
        )),
        Some(true) => {}
    }
    Ok(())
}

/// The token directory can take a new file, or nothing is changed.
fn check_token_dir(dir: &Path) -> Result<(), InstallError> {
    let mut at = dir;
    loop {
        match at.metadata() {
            Ok(meta) if meta.is_dir() => {
                return if meta.permissions().readonly() {
                    Err(InstallError::failed(format!(
                        "cannot write the token files to {}: the directory is read-only",
                        dir.display()
                    )))
                } else {
                    Ok(())
                };
            }
            Ok(_) => {
                return Err(InstallError::failed(format!(
                    "cannot write the token files to {}: {} is not a directory",
                    dir.display(),
                    at.display()
                )));
            }
            Err(_) => match at.parent() {
                Some(up) if !up.as_os_str().is_empty() => at = up,
                _ => return Ok(()),
            },
        }
    }
}

fn plan_install(ctx: &Context) -> Result<Vec<Step>, InstallError> {
    let state = ctx.state;
    let mut steps = Vec::new();
    if !state.user_exists {
        steps.push(pveum(
            format!("create user {HS_USER}"),
            &["user", "add", HS_USER, "--comment", "HomeStack server"],
        ));
    }
    for (role, wanted) in ROLES {
        match state.roles.get(role) {
            None => steps.push(pveum(
                format!("create role {role}"),
                &["role", "add", role, "--privs", wanted],
            )),
            Some(have) if *have != privs(wanted) => {
                let were: Vec<&str> = have.iter().map(String::as_str).collect();
                steps.push(pveum(
                    format!("set the privileges of role {role} (were {})", were.join(",")),
                    &["role", "modify", role, "--privs", wanted],
                ));
            }
            Some(_) => {}
        }
    }
    if !state.pools.contains_key(POOL_GOLD) {
        steps.push(pveum(
            format!("create pool {POOL_GOLD}"),
            &["pool", "add", POOL_GOLD, "--comment", "Gold"],
        ));
    }
    if !state.pools.contains_key(POOL_SERVICE) {
        steps.push(pveum(
            format!("create pool {POOL_SERVICE}"),
            &["pool", "add", POOL_SERVICE, "--comment", "Containers"],
        ));
    }
    // The production part makes the test pool too: the server's token has
    // entries on it.
    let mut pools = vec![ctx.pool];
    if !ctx.settings.test {
        pools.push(POOL_TEST);
    }
    for pool in pools {
        if state.pools.contains_key(pool) {
            continue;
        }
        let comment = if pool == POOL_TEST {
            "Test Workspaces"
        } else {
            "Workspaces"
        };
        steps.push(pveum(
            format!("create pool {pool}"),
            &["pool", "add", pool, "--comment", comment],
        ));
    }
    for vmid in ctx.settings.adopt.iter().collect::<BTreeSet<_>>() {
        let Some(vm) = state.vms.get(vmid) else { continue };
        let Some(pool) = discover::pool_by_tags(vm) else {
            continue;
        };
        steps.push(pveum(
            format!("put VM {vmid} ({}) in pool {pool}, as its tags say", vm.name),
            &["pool", "modify", pool, "--vms", &vmid.to_string()],
        ));
    }
    if let Some(gold) = ctx.gold
        && let Some(vm) = state.vms.get(&gold)
        && !vm.tags.iter().any(|tag| tag == "homestack-gold")
    {
        let mut tags = vm.tags.clone();
        tags.push("homestack-gold".to_owned());
        steps.push(Step {
            description: format!("tag Gold {gold} homestack-gold"),
            action: Action::Pvesh(vec![
                "set".to_owned(),
                format!("/nodes/{}/qemu/{gold}/config", vm.node),
                "--tags".to_owned(),
                tags.join(";"),
            ]),
        });
    }
    let gold_pool = gold_pool(ctx.settings.test);
    if let Some(gold) = ctx.gold
        && state.vms.get(&gold).is_some_and(|vm| vm.pool != gold_pool)
    {
        steps.push(pveum(
            format!("put Gold {gold} in pool {gold_pool}"),
            &["pool", "modify", gold_pool, "--vms", &gold.to_string()],
        ));
    }
    plan_token(ctx, ctx.token, &format!("HomeStack {}", ctx.part), &mut steps)?;
    plan_token(ctx, DEPLOY_TOKEN, "HomeStack deploy", &mut steps)?;
    let target = target_acl(state, &ctx.names, Command::Install, true, &[ctx.token, DEPLOY_TOKEN]);
    steps.extend(plan_acl(state, &target));
    let used = roles_in_use(state, &target);
    for role in LEGACY_ROLES {
        if state.roles.contains_key(role) && !used.contains(role) {
            steps.push(pveum(
                format!("delete role {role} (made by hand, no longer used)"),
                &["role", "delete", role],
            ));
        }
    }
    for node in &ctx.snippet_nodes {
        let Some(sums) = ctx.sums.get(node) else { continue };
        for (name, text) in SNIPPETS {
            let path = ctx.paths.get(name).cloned().unwrap_or_default();
            if sums.get(&path).map(String::as_str) != Some(snippets::sum(text).as_str()) {
                steps.push(Step {
                    description: format!("write {name} on {node}"),
                    action: Action::WriteSnippet {
                        node: node.clone(),
                        name: name.to_owned(),
                        path,
                    },
                });
            }
        }
    }
    Ok(steps)
}

/// VMs whose `cicustom` names a snippet, as (name, vmid); the VMs whose
/// configuration could not be read.
fn snippet_users(root: &mut dyn Root, state: &State) -> (Vec<(String, u32)>, Vec<u32>) {
    let mut users = Vec::new();
    let mut unreadable = Vec::new();
    for (vmid, vm) in &state.vms {
        if vm.kind != "qemu" {
            continue;
        }
        let Ok(config) = state.vm_config(root, *vmid) else {
            unreadable.push(*vmid);
            continue;
        };
        let cicustom = config
            .get("cicustom")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        for (name, _) in SNIPPETS {
            if cicustom.contains(&format!("snippets/{name}")) {
                users.push((name.to_owned(), *vmid));
            }
        }
    }
    (users, unreadable)
}

fn plan_uninstall(root: &mut dyn Root, ctx: &Context, kept: &mut Vec<String>) -> Vec<Step> {
    let state = ctx.state;
    let mut steps = Vec::new();
    let (scope, pools): (Vec<&str>, Vec<&str>) = if ctx.settings.test {
        (vec!["test"], vec![POOL_TEST])
    } else {
        (
            vec!["server", "test", DEPLOY_TOKEN],
            vec![POOL_WORKSPACES, POOL_TEST, POOL_GOLD, POOL_SERVICE],
        )
    };
    let target = target_acl(state, &ctx.names, Command::Uninstall, ctx.settings.test, &scope);
    steps.extend(plan_acl(state, &target));
    for name in &scope {
        if state.tokens.contains_key(*name) {
            steps.push(pveum(
                format!("delete token {HS_USER}!{name}"),
                &["user", "token", "delete", HS_USER, name],
            ));
        }
    }
    let used = roles_in_use(state, &target);
    if ctx.settings.test {
        for role in LEGACY_ROLES {
            if state.roles.contains_key(role) && !used.contains(role) {
                steps.push(pveum(
                    format!("delete role {role} (made by hand, no longer used)"),
                    &["role", "delete", role],
                ));
            }
        }
    } else {
        if state.user_exists {
            steps.push(pveum(format!("delete user {HS_USER}"), &["user", "delete", HS_USER]));
        }
        for role in ROLES.iter().map(|(role, _)| *role).chain(LEGACY_ROLES) {
            if !state.roles.contains_key(role) {
                continue;
            }
            if used.contains(role) {
                kept.push(format!("role {role}: still used by an ACL entry of someone else"));
            } else {
                steps.push(pveum(format!("delete role {role}"), &["role", "delete", role]));
            }
        }
    }
    // Every guest is taken out and the pool deleted; the guests and their
    // tags stay, and the next install's autodiscovery puts them back
    // (the owner, 2026-10-15).
    for pool in pools {
        let Some(members) = state.pools.get(pool) else { continue };
        for vmid in members {
            let id = vmid.parse::<u32>().ok();
            let vm = id.and_then(|id| state.vms.get(&id));
            let name = vm.map_or_else(String::new, |vm| format!(" ({})", vm.name));
            let is_gold = (pool == POOL_GOLD || pool == POOL_TEST)
                && (id.is_some() && id == ctx.settings.gold
                    || vm.is_some_and(|vm| vm.tags.iter().any(|tag| tag == discover::GOLD_TAG)));
            let description = if is_gold {
                format!("take Gold {vmid}{name} out of pool {pool} (Gold stays)")
            } else {
                format!("take VM {vmid}{name} out of pool {pool} (it stays)")
            };
            steps.push(pveum(
                description,
                &["pool", "modify", pool, "--vms", vmid, "--delete", "1"],
            ));
            let back = if is_gold {
                Some(pool)
            } else {
                vm.and_then(discover::pool_by_tags)
            };
            match back {
                Some(back) if back == pool => {}
                Some(other) => kept.push(format!(
                    "VM {vmid}{name} leaves pool {pool}; its tags put it in pool {other} at the next install"
                )),
                None => kept.push(format!(
                    "VM {vmid}{name} leaves pool {pool}; no tag of it puts it back at the next install"
                )),
            }
        }
        steps.push(pveum(format!("delete pool {pool}"), &["pool", "delete", pool]));
    }
    if ctx.settings.test {
        return steps;
    }
    let (users, unreadable) = snippet_users(root, state);
    for (name, _) in SNIPPETS {
        // A VM without its snippet does not start: a snippet a VM names, or
        // that cannot be checked, is never removed (the owner, 2026-10-15).
        if !unreadable.is_empty() {
            let ids: Vec<String> = unreadable.iter().map(u32::to_string).collect();
            kept.push(format!(
                "{name}: cannot read every VM's configuration ({}); it stays",
                ids.join(" ")
            ));
            continue;
        }
        let mut vmids: Vec<u32> = users
            .iter()
            .filter(|(user, _)| user == name)
            .map(|(_, vmid)| *vmid)
            .collect();
        vmids.sort_unstable();
        if !vmids.is_empty() {
            let ids: Vec<String> = vmids.iter().map(u32::to_string).collect();
            kept.push(format!("{name} is used by VM {} (cicustom); it stays", ids.join(" ")));
            continue;
        }
        let path = ctx.paths.get(name).cloned().unwrap_or_default();
        for node in &ctx.snippet_nodes {
            if ctx.sums.get(node).is_some_and(|sums| sums.contains_key(&path)) {
                steps.push(Step {
                    description: format!("remove {name} from {node}"),
                    action: Action::RemoveSnippet {
                        node: node.clone(),
                        name: name.to_owned(),
                        path: path.clone(),
                    },
                });
            }
        }
    }
    steps
}

/// Read the cluster and plan `command`. Nothing is changed.
pub fn make_plan(
    root: &mut dyn Root,
    access: &Access,
    command: Command,
    settings: &Settings,
) -> Result<Plan, InstallError> {
    let mut state = State::load(root, &access.topology)?;
    if command == Command::Install {
        state.read_bridges(root)?;
    }
    // An option with only one possible value is not asked for.
    let (resolved, taken) = if command == Command::Install {
        checks::resolve(&state, settings)
    } else {
        let mut resolved = settings.clone();
        if !resolved.test && resolved.snippet_storage.is_none() {
            resolved.snippet_storage = checks::uninstall_snippet_storage(&state);
        }
        (resolved, Vec::new())
    };
    let settings = &resolved;
    let (part, pool, token) = if settings.test {
        ("test", POOL_TEST, "test")
    } else {
        ("production", POOL_WORKSPACES, "server")
    };
    let mut found = Findings::default();
    let mut node_errors = Vec::new();
    for node in state.topology.nodes.iter().filter(|node| !node.online) {
        node_errors.push(format!("{} is offline; run this again when it is back", node.name));
    }
    if state.topology.quorate == Some(false) {
        found.errors.push(
            "the cluster has no quorum, so Proxmox refuses every change; bring the other nodes back first".to_owned(),
        );
    }
    match command {
        Command::Uninstall => {
            if settings.gold.is_some() {
                found.gold = checks::find_gold(&state, settings, &mut found);
            }
        }
        Command::Install => {
            checks::check_versions(root, &state, &mut found);
            checks::check_storages(&state, settings, &mut found);
            checks::check_bridge(root, &state, settings, &mut found);
            checks::check_gold(root, &state, settings, &mut found);
            checks::check_tags(root, settings, &mut found);
            checks::check_certificates(root, &state, &mut found);
            for vmid in &settings.adopt {
                found.errors.extend(discover::refusal(&state, settings.test, *vmid));
            }
            let left: Vec<String> = discover::found(&state, settings.test)
                .into_iter()
                .filter(|(vmid, _)| !settings.adopt.contains(vmid))
                .map(|(vmid, pool)| format!("{vmid} -> {pool}"))
                .collect();
            if !left.is_empty() {
                found.warnings.push(format!(
                    "in no pool, though their tags name one: {} ({} VMIDS puts them back)",
                    left.join(", "),
                    discover::OPTION
                ));
            }
        }
    }

    let mut kept = Vec::new();
    let mut paths = BTreeMap::new();
    let mut sums = BTreeMap::new();
    let mut snippet_nodes = Vec::new();
    let storage = settings
        .snippet_storage
        .as_deref()
        .filter(|id| state.storages.contains_key(*id));
    if let Some(storage) = storage {
        for (name, _) in SNIPPETS {
            paths.insert(name, snippets::path(root, storage, name)?);
        }
        for node in snippets::nodes_of(&state, storage) {
            snippet_nodes.push(node.name.clone());
            if !node.online {
                continue;
            }
            match snippets::sums_on(root, &state.local_node, &node, &paths) {
                Ok(found_sums) => {
                    sums.insert(node.name.clone(), found_sums);
                }
                Err(_) => node_errors.push(format!(
                    "{} does not answer over SSH (root@{}); run this again when it does",
                    node.name, node.ip
                )),
            }
        }
    } else if command == Command::Uninstall && !settings.test {
        kept.push("the vendor snippets: pass --snippet-storage to remove them".to_owned());
    }

    if !found.errors.is_empty() {
        return Err(InstallError::Checks {
            errors: found.errors,
            warnings: found.warnings,
        });
    }
    if !found.questions.is_empty() {
        return Err(InstallError::Missing(found.questions));
    }
    if command == Command::Install {
        check_token_dir(&settings.token_dir)?;
    }

    let names = Names {
        settings,
        token,
        sdn_path: &found.sdn_path,
    };
    let gold_label = found.gold.and_then(|vmid| {
        state
            .vms
            .get(&vmid)
            .map(|vm| format!("{vmid} {} ({})", vm.name, vm.node))
    });
    let ctx = Context {
        state: &state,
        settings,
        names,
        gold: found.gold,
        paths,
        sums,
        snippet_nodes,
        token,
        pool,
        part,
    };
    let steps = match command {
        Command::Install => plan_install(&ctx)?,
        Command::Uninstall => plan_uninstall(root, &ctx, &mut kept),
    };
    let mut notes = Vec::new();
    if command == Command::Uninstall && !settings.test {
        let files: Vec<String> = ["server", "test", DEPLOY_TOKEN]
            .iter()
            .map(|id| settings.token_dir.join(token_file_name(id)))
            .filter(|file| file.exists())
            .map(|file| file.display().to_string())
            .collect();
        if !files.is_empty() {
            notes.push(format!(
                "The token files on this desktop are not removed (their tokens are gone): {}",
                files.join(" ")
            ));
        }
    }
    let cluster = Cluster {
        nodes: state
            .topology
            .nodes
            .iter()
            .filter(|node| node.online)
            .map(|node| node.name.clone())
            .collect(),
        storages: state
            .storages
            .iter()
            .filter(|(_, storage)| !storage.disabled)
            .map(|(id, storage)| (id.clone(), (storage.content.clone(), storage.nodes.clone())))
            .collect(),
        guests: state
            .vms
            .iter()
            .map(|(vmid, vm)| (*vmid, (vm.name.clone(), vm.kind.clone(), vm.pool.clone())))
            .collect(),
        bridge: found.sdn_path.rsplit('/').next().unwrap_or_default().to_owned(),
        api_names: found.api_names,
        gold: found.gold,
        gold_node: found
            .gold
            .and_then(|vmid| state.vms.get(&vmid))
            .map(|vm| vm.node.clone()),
    };
    Ok(Plan {
        command,
        part,
        host: access.host.clone(),
        version: access.version.clone(),
        topology: access.topology.clone(),
        infos: found.infos,
        warnings: found.warnings,
        kept,
        notes,
        node_errors,
        steps,
        gold: gold_label,
        cluster,
        taken,
        settings: resolved.clone(),
    })
}

/// Every option not given in `settings`, with its choices and what is chosen
/// at first, read from the cluster. Nothing is changed.
pub fn questions(
    root: &mut dyn Root,
    access: &Access,
    settings: &Settings,
) -> Result<Vec<crate::Question>, InstallError> {
    let mut state = State::load(root, &access.topology)?;
    state.read_bridges(root)?;
    Ok(checks::all_questions(&state, settings))
}

/// A free name next to `file` for keeping it: `NAME.replaced-<seconds>`.
fn aside(file: &Path) -> PathBuf {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut candidate = file.with_file_name(format!("{name}.replaced-{seconds}"));
    let mut n = 1;
    while candidate.exists() {
        candidate = file.with_file_name(format!("{name}.replaced-{seconds}-{n}"));
        n += 1;
    }
    candidate
}
