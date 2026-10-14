//! The plan of an install or uninstall: what would change, in order. The
//! rules are those of `deploy/pve/homestack-pve.sh` (phase 4b): HomeStack
//! owns its user, tokens, roles, pools, ACL entries and vendor snippets;
//! everything else is only checked.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::InstallError;
use crate::checks::{self, Findings};
use crate::probe::{Access, Topology};
use crate::root::Root;
use crate::settings::{Command, Settings};
use crate::shell::{join, quote};
use crate::snippets::{self, SNIPPETS};
use crate::state::{Acl, HS_USER, State, privs};

const POOL_GOLD: &str = "homestack-gold";
const POOL_SERVICE: &str = "homestack-service";
const DEPLOY_TOKEN: &str = "deploy";

/// The roles HomeStack owns and their privileges.
const ROLES: [(&str, &str); 6] = [
    (
        "HomeStack",
        "Pool.Audit,VM.Allocate,VM.Audit,VM.Config.Cloudinit,VM.Config.Disk,VM.Config.Options,VM.GuestAgent.Audit,VM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted,VM.Migrate,VM.PowerMgmt",
    ),
    (
        "HomeStackGold",
        "Pool.Audit,VM.Audit,VM.Clone,VM.GuestAgent.Audit,VM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted",
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
            Action::CreateToken { token, comment, file } => format!(
                "pveum user token add {HS_USER} {token} --privsep 1 --comment {} (secret to {})",
                quote(comment),
                file.display()
            ),
            Action::WriteSnippet { node, name, .. } => format!("write {name} on {node}"),
            Action::RemoveSnippet { node, name, .. } => format!("remove {name} from {node}"),
        }
    }
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
    pool: &'a str,
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
    entries.insert(acl_token(&format!("/pool/{}", names.pool), names.token, "HomeStack"));
    entries.insert(acl_token(&format!("/pool/{POOL_GOLD}"), names.token, "HomeStackGold"));
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
            if file.exists() {
                return Err(InstallError::failed(format!(
                    "{} exists; move it away so the new token's secret can be written there",
                    file.display()
                )));
            }
            steps.push(Step {
                description: format!("create token {HS_USER}!{id} (secret to {}, mode 0600)", file.display()),
                action: Action::CreateToken {
                    token: id.to_owned(),
                    comment: comment.to_owned(),
                    file,
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
            &["pool", "add", POOL_GOLD, "--comment", "HomeStack Gold"],
        ));
    }
    if !state.pools.contains_key(POOL_SERVICE) {
        steps.push(pveum(
            format!("create pool {POOL_SERVICE}"),
            &["pool", "add", POOL_SERVICE, "--comment", "HomeStack server containers"],
        ));
    }
    if !state.pools.contains_key(ctx.pool) {
        let comment = if ctx.settings.test {
            "HomeStack tests"
        } else {
            "HomeStack workspaces"
        };
        steps.push(pveum(
            format!("create pool {}", ctx.pool),
            &["pool", "add", ctx.pool, "--comment", comment],
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
    if let Some(gold) = ctx.gold
        && state.vms.get(&gold).is_some_and(|vm| vm.pool != POOL_GOLD)
    {
        steps.push(pveum(
            format!("put Gold {gold} in pool {POOL_GOLD}"),
            &["pool", "modify", POOL_GOLD, "--vms", &gold.to_string()],
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
        (vec!["test"], vec!["homestack-test"])
    } else {
        (
            vec!["server", "test", DEPLOY_TOKEN],
            vec!["homestack", "homestack-test", POOL_GOLD, POOL_SERVICE],
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
    for pool in pools {
        let Some(members) = state.pools.get(pool) else { continue };
        let mut rest = Vec::new();
        if pool == POOL_GOLD {
            for vmid in members {
                let is_gold = vmid.parse::<u32>().ok().is_some_and(|id| {
                    Some(id) == ctx.settings.gold
                        || state
                            .vms
                            .get(&id)
                            .is_some_and(|vm| vm.tags.iter().any(|tag| tag == "homestack-gold"))
                });
                if is_gold {
                    steps.push(pveum(
                        format!("take Gold {vmid} out of pool {POOL_GOLD} (Gold stays)"),
                        &["pool", "modify", POOL_GOLD, "--vms", vmid, "--delete", "1"],
                    ));
                } else {
                    rest.push(vmid.clone());
                }
            }
        } else {
            rest.clone_from(members);
        }
        if rest.is_empty() {
            steps.push(pveum(format!("delete pool {pool}"), &["pool", "delete", pool]));
        } else {
            kept.push(format!(
                "pool {pool}: {} (its VMs stay; move or destroy them first)",
                rest.join(" ")
            ));
        }
    }
    if ctx.settings.test {
        return steps;
    }
    let (users, unreadable) = snippet_users(root, state);
    for (name, _) in SNIPPETS {
        if !unreadable.is_empty() && !ctx.settings.force {
            let ids: Vec<String> = unreadable.iter().map(u32::to_string).collect();
            kept.push(format!(
                "{name}: cannot read every VM's configuration ({}); --force removes it anyway",
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
        if !vmids.is_empty() && !ctx.settings.force {
            let ids: Vec<String> = vmids.iter().map(u32::to_string).collect();
            kept.push(format!(
                "{name} is used by VM {} (cicustom); --force removes it anyway",
                ids.join(" ")
            ));
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
    let state = State::load(root, &access.topology)?;
    // An option with only one possible value is not asked for.
    let (resolved, taken) = if command == Command::Install {
        checks::resolve(&state, settings)
    } else {
        (settings.clone(), Vec::new())
    };
    let settings = &resolved;
    let (part, pool, token) = if settings.test {
        ("test", "homestack-test", "test")
    } else {
        ("production", "homestack", "server")
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
            checks::check_gold(root, &state, settings, &mut found);
            checks::check_tags(root, settings, &mut found);
            checks::check_certificates(root, &state, &mut found);
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
        pool,
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
        taken,
        settings: resolved.clone(),
    })
}
