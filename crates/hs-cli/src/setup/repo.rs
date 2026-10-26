//! GitHub repositories on a workspace (Python `repo.py`): a checkout under
//! `[repo] checkout_root`, a workspace-local deploy key registered with
//! GitHub through the desktop's `gh`, and a fast-forward-only update that
//! never cleans, resets or replaces anything.

use hs_install::shell;
use serde_json::{Value, json};

use super::Settings;
use super::catalog::{gh, github_json, github_pages};
use super::plan::repository_paths;
use super::text::repository;
use super::workspace::Workspace;
use crate::AppError;
use crate::system::{Output, System};

/// The steps a setup shows, in order.
#[must_use]
pub fn repository_setup_steps(actions: &[String]) -> Vec<&'static str> {
    let has = |action: &str| actions.iter().any(|a| a == action);
    let mut steps = Vec::new();
    for (action, label) in [
        ("generate-key", "Generate deploy key"),
        ("derive-public-key", "Restore public deploy key"),
        ("replace-missing-private-key", "Replace incomplete deploy key"),
    ] {
        if has(action) {
            steps.push(label);
            break;
        }
    }
    steps.push("Reconcile GitHub deploy key");
    if has("clone") {
        steps.push("Clone repository");
    }
    if has("fetch-upstream") {
        steps.push("Fetch repository upstream");
        steps.push("Inspect and update repository");
    }
    if has("set-origin") {
        steps.push("Configure Git remote");
    }
    if has("clone") || has("set-ssh-command") {
        steps.push("Configure repository SSH");
    }
    steps.push("Verify Git access");
    steps.push("Refresh repository status");
    steps
}

/// The repository given, else `[repo] owner` / the workspace's name.
pub fn resolve_repository_argument(
    owner: Option<&str>,
    explicit: Option<&str>,
    workspace: &str,
) -> Result<String, AppError> {
    if let Some(explicit) = explicit {
        return repository(explicit);
    }
    let Some(owner) = owner.filter(|owner| !owner.is_empty()) else {
        return Err(AppError::new(
            "Repository was not specified and [repo] owner is not configured",
        ));
    };
    repository(&format!("{owner}/{workspace}"))
}

/// `OWNER/REPO` of a GitHub remote URL, if it is one.
#[must_use]
pub fn repository_from_remote(url: Option<&str>) -> Option<String> {
    let value = url.unwrap_or_default().trim();
    let two_parts = |text: &str| {
        text.split_once('/')
            .is_some_and(|(owner, rest)| !owner.is_empty() && !rest.is_empty())
    };
    for (prefix, slash) in [
        ("git@github.com:", false),
        ("ssh://git@github.com/", false),
        ("https://github.com/", true),
    ] {
        let Some(rest) = value.strip_prefix(prefix) else {
            continue;
        };
        let mut candidates = Vec::new();
        let trimmed = if slash {
            rest.strip_suffix('/').unwrap_or(rest)
        } else {
            rest
        };
        // The shortest match first, as the lazy pattern takes it.
        if let Some(core) = trimmed.strip_suffix(".git") {
            candidates.push(core);
        }
        candidates.push(trimmed);
        if slash && trimmed != rest {
            candidates.push(rest);
        }
        return candidates
            .into_iter()
            .find(|candidate| two_parts(candidate))
            .map(str::to_owned);
    }
    None
}

fn identity(key: &str) -> Option<String> {
    let parts: Vec<&str> = key.split_whitespace().collect();
    (parts.len() >= 2).then(|| format!("{} {}", parts[0], parts[1]))
}

fn stored_ssh(key: &str) -> String {
    format!("ssh -i {key} -o IdentitiesOnly=yes")
}

fn bootstrap_ssh(key: &str) -> String {
    format!(
        "{} -o StrictHostKeyChecking=accept-new -o BatchMode=yes",
        stored_ssh(key)
    )
}

/// The deploy keys of `name` on GitHub.
fn keys(system: &dyn System, name: &str) -> Result<Vec<Value>, AppError> {
    let keys = github_pages(system, &format!("repos/{name}/keys?per_page=100"))?;
    if keys.iter().any(|item| !item.is_object()) {
        return Err(AppError::new("GitHub returned an invalid deploy-key entry"));
    }
    Ok(keys)
}

fn matching_key(keys: &[Value], public_key: &str) -> Option<Value> {
    let wanted = identity(public_key)?;
    keys.iter()
        .find(|item| identity(item["key"].as_str().unwrap_or_default()).as_deref() == Some(wanted.as_str()))
        .cloned()
}

fn add_key(system: &dyn System, name: &str, title: &str, public_key: &str) -> Result<(), AppError> {
    let Some(identity) = identity(public_key) else {
        return Err(AppError::new("Workspace public deploy key is invalid"));
    };
    gh(
        system,
        &[
            "--method",
            "POST",
            &format!("repos/{name}/keys"),
            "-f",
            &format!("title={title}"),
            "-f",
            &format!("key={identity}"),
            "-F",
            "read_only=false",
        ],
    )?;
    Ok(())
}

fn delete_key(system: &dyn System, name: &str, id: i64) -> Result<(), AppError> {
    gh(system, &["--method", "DELETE", &format!("repos/{name}/keys/{id}")])?;
    Ok(())
}

/// A command on the workspace; ssh's failure is an error.
fn run(workspace: &mut dyn Workspace, command: &str) -> Result<Output, AppError> {
    workspace.run(command, None, false)
}

/// A command that must succeed (Python's `check=True`).
fn run_checked(workspace: &mut dyn Workspace, command: &str) -> Result<Output, AppError> {
    let output = run(workspace, command)?;
    if output.code != 0 {
        return Err(AppError::new(format!(
            "Workspace command failed (exit {}); output withheld",
            output.code
        )));
    }
    Ok(output)
}

fn output_of(workspace: &mut dyn Workspace, command: &str) -> Result<String, AppError> {
    let output = run(workspace, command)?;
    Ok(if output.code == 0 {
        output.stdout.trim().to_owned()
    } else {
        String::new()
    })
}

fn upstream_counts(workspace: &mut dyn Workspace, checkout: &str) -> Result<Option<(i64, i64)>, AppError> {
    let output = run(
        workspace,
        &format!(
            "git -C {} rev-list --left-right --count HEAD...@{{upstream}}",
            shell::quote(checkout)
        ),
    )?;
    if output.code != 0 {
        return Ok(None);
    }
    let fields: Vec<&str> = output.stdout.split_whitespace().collect();
    if fields.len() != 2 {
        return Ok(None);
    }
    Ok(match (fields[0].parse(), fields[1].parse()) {
        (Ok(ahead), Ok(behind)) => Some((ahead, behind)),
        _ => None,
    })
}

fn checkout_error(state: &Value) -> Option<AppError> {
    if state["working_tree"] == json!("modified") {
        return Some(AppError::new(
            "Checkout has uncommitted, staged, or untracked changes; refusing to update it",
        ));
    }
    if state["working_tree"] != json!("clean") {
        return Some(AppError::new(
            "Could not inspect the checkout's working tree; refusing to update it",
        ));
    }
    if state["head_state"] != json!("attached") || !state["branch"].as_str().is_some_and(|b| !b.is_empty()) {
        return Some(AppError::new(
            "Checkout is in detached HEAD state; refusing to update it",
        ));
    }
    if !state["upstream"].as_str().is_some_and(|u| !u.is_empty()) {
        return Some(AppError::new(
            "Current branch has no configured upstream; refusing to update it",
        ));
    }
    let present = |key: &str| state[key].as_str().is_some_and(|value| !value.is_empty());
    if !present("upstream_remote")
        || state["upstream_remote"] == json!(".")
        || !present("upstream_merge")
        || !present("upstream_ref")
    {
        return Some(AppError::new(
            "Current branch does not track a fetchable remote branch; refusing to update it",
        ));
    }
    None
}

fn history_error(state: &Value) -> Option<AppError> {
    let (Some(ahead), Some(behind)) = (state["ahead"].as_i64(), state["behind"].as_i64()) else {
        return Some(AppError::new(
            "Could not determine the checkout's upstream history; refusing to update it",
        ));
    };
    if ahead < 0 || behind < 0 {
        return Some(AppError::new(
            "Could not determine the checkout's upstream history; refusing to update it",
        ));
    }
    if ahead > 0 {
        if behind > 0 {
            return Some(AppError::new(
                "Checkout history has diverged from its upstream; refusing to update it",
            ));
        }
        return Some(AppError::new(
            "Checkout contains local commits ahead of its upstream; refusing to update it",
        ));
    }
    None
}

fn optional(text: String) -> Value {
    if text.is_empty() {
        Value::Null
    } else {
        Value::String(text)
    }
}

/// The repository's state on the workspace and on GitHub.
#[allow(clippy::too_many_lines)]
pub fn inspect_repository(
    settings: &Settings,
    system: &dyn System,
    workspace: &mut dyn Workspace,
    vmid: u32,
    name: &str,
    repository_name: &str,
    verify_access: bool,
) -> Result<Value, AppError> {
    let repository_name = repository(repository_name)?;
    github_json(system, &[&format!("repos/{repository_name}")], false)?;
    let deploy_keys = keys(system, &repository_name)?;
    let (checkout, key, public_key_path) = repository_paths(settings, &repository_name)?;
    let quoted = shell::quote(&checkout);
    let mut tools = serde_json::Map::new();
    for tool in ["git", "ssh", "ssh-keygen"] {
        let found = run(workspace, &format!("command -v {tool} >/dev/null 2>&1"))?.code == 0;
        tools.insert(tool.to_owned(), json!(found));
    }
    let has = |tool: &str| tools[tool] == json!(true);
    let exists = run(workspace, &format!("test -e {quoted}"))?.code == 0;
    let mut state = json!({
        "checkout_state": "missing", "origin": null, "working_tree": "unavailable", "head_state": "unavailable",
        "branch": null, "upstream": null, "upstream_remote": null, "upstream_merge": null, "upstream_ref": null,
        "ahead": null, "behind": null,
    });
    if exists && has("git") {
        let inside = run(workspace, &format!("git -C {quoted} rev-parse --is-inside-work-tree"))?;
        if inside.code != 0 || inside.stdout.trim() != "true" {
            state["checkout_state"] = json!("not-a-repository");
        } else {
            let origin = output_of(workspace, &format!("git -C {quoted} remote get-url origin"))?;
            let remote = repository_from_remote(Some(&origin));
            state["checkout_state"] = json!(match remote.as_deref() {
                Some(found) if found == repository_name => "ready",
                Some(_) => "different-repository",
                None => "repository-without-recognized-origin",
            });
            state["origin"] = optional(origin);
            let status = run(
                workspace,
                &format!("git -C {quoted} status --porcelain --untracked-files=all"),
            )?;
            state["working_tree"] = json!(if status.code != 0 {
                "unavailable"
            } else if status.stdout.trim().is_empty() {
                "clean"
            } else {
                "modified"
            });
            let branch = run(workspace, &format!("git -C {quoted} symbolic-ref --quiet --short HEAD"))?;
            if branch.code == 0 && !branch.stdout.trim().is_empty() {
                let branch_name = branch.stdout.trim().to_owned();
                state["head_state"] = json!("attached");
                state["branch"] = json!(branch_name);
                let upstream = run(
                    workspace,
                    &format!("git -C {quoted} rev-parse --abbrev-ref --symbolic-full-name @{{upstream}}"),
                )?;
                if upstream.code == 0 && !upstream.stdout.trim().is_empty() {
                    state["upstream"] = json!(upstream.stdout.trim());
                    let remote_key = shell::quote(&format!("branch.{branch_name}.remote"));
                    let merge_key = shell::quote(&format!("branch.{branch_name}.merge"));
                    state["upstream_remote"] = optional(output_of(
                        workspace,
                        &format!("git -C {quoted} config --get {remote_key}"),
                    )?);
                    state["upstream_merge"] = optional(output_of(
                        workspace,
                        &format!("git -C {quoted} config --get {merge_key}"),
                    )?);
                    state["upstream_ref"] = optional(output_of(
                        workspace,
                        &format!("git -C {quoted} rev-parse --symbolic-full-name @{{upstream}}"),
                    )?);
                    if let Some((ahead, behind)) = upstream_counts(workspace, &checkout)? {
                        state["ahead"] = json!(ahead);
                        state["behind"] = json!(behind);
                    }
                }
            } else {
                state["head_state"] = json!("detached");
            }
        }
    } else if exists {
        state["checkout_state"] = json!("uninspectable");
    }
    let private = run(workspace, &format!("test -f {}", shell::quote(&key)))?.code == 0;
    let public = run(workspace, &format!("test -f {}", shell::quote(&public_key_path)))?.code == 0;
    let public_key = if public {
        output_of(workspace, &format!("cat {}", shell::quote(&public_key_path)))?
    } else {
        String::new()
    };
    let fingerprint = if public && has("ssh-keygen") {
        output_of(workspace, &format!("ssh-keygen -lf {}", shell::quote(&public_key_path)))?
    } else {
        String::new()
    };
    let key_state = if private && public && has("ssh-keygen") {
        let derived = output_of(workspace, &format!("ssh-keygen -y -f {}", shell::quote(&key)))?;
        if identity(&derived) == identity(&public_key) {
            "ready"
        } else {
            "mismatched"
        }
    } else if private && public {
        "uninspectable"
    } else if private {
        "private-only"
    } else if public {
        "public-only"
    } else {
        "missing"
    };
    let matching = matching_key(&deploy_keys, &public_key);
    let deploy_key_id = matching
        .as_ref()
        .and_then(|m| m["id"].as_i64())
        .map_or(Value::Null, |id| json!(id));
    let deploy_key_state = match &matching {
        None => "missing",
        Some(m) if m["read_only"] == json!(true) => "read-only",
        Some(_) => "read-write",
    };
    let expected_ssh = stored_ssh(&key);
    let core_ssh = if state["checkout_state"] == json!("ready") {
        output_of(
            workspace,
            &format!("git -C {quoted} config --local --get core.sshCommand"),
        )?
    } else {
        String::new()
    };
    let ssh_state = if core_ssh == expected_ssh {
        "ready"
    } else if core_ssh.is_empty() {
        "missing"
    } else {
        "different"
    };
    let mut access = "not-tested";
    if verify_access && has("git") && has("ssh") && private {
        let remote_url = format!("git@github.com:{repository_name}.git");
        let result = run(
            workspace,
            &format!(
                "GIT_SSH_COMMAND={} git ls-remote {} HEAD",
                shell::quote(&bootstrap_ssh(&key)),
                shell::quote(&remote_url)
            ),
        )?;
        access = if result.code == 0 { "working" } else { "failed" };
    }
    let all_tools = tools.values().all(|v| *v == json!(true));
    let ready = state["checkout_state"] == json!("ready")
        && key_state == "ready"
        && deploy_key_state == "read-write"
        && ssh_state == "ready"
        && access == "working"
        && all_tools;
    let untouched =
        state["checkout_state"] == json!("missing") && key_state == "missing" && deploy_key_state == "missing";
    let mut result = json!({
        "ok": true, "command": "repo", "vmid": vmid, "workspace": name, "repository": repository_name,
        "checkout": checkout, "key_path": key, "key_state": key_state,
        "key_fingerprint": optional(fingerprint), "deploy_key_state": deploy_key_state,
        "deploy_key_id": deploy_key_id, "core_ssh_command": optional(core_ssh),
        "ssh_config_state": ssh_state, "access": access, "tools": tools, "ready": ready,
        "status": if ready { "ready" } else if untouched { "not configured" } else { "needs setup" },
    });
    if let (Some(result), Some(state)) = (result.as_object_mut(), state.as_object()) {
        result.extend(state.clone());
    }
    Ok(result)
}

/// What a setup does to reach a configured checkout; refuses anything it
/// would have to clean, reset, replace or rotate.
pub fn repository_setup_actions(state: &Value) -> Result<Vec<String>, AppError> {
    let checkout = state["checkout_state"].as_str().unwrap_or_default();
    if [
        "not-a-repository",
        "repository-without-recognized-origin",
        "different-repository",
    ]
    .contains(&checkout)
    {
        return Err(AppError::new(format!(
            "Checkout path is unsafe to modify: {checkout}. HomeStack will not clean, reset, or replace it."
        )));
    }
    let key = state["key_state"].as_str().unwrap_or("missing");
    if key == "mismatched" {
        return Err(AppError::new(
            "Repository deploy-key private/public files do not match; rotate the key",
        ));
    }
    let mut actions: Vec<String> = Vec::new();
    match key {
        "missing" => actions.push("generate-key".into()),
        "private-only" => actions.push("derive-public-key".into()),
        "public-only" => actions.push("replace-missing-private-key".into()),
        _ => {}
    }
    match state["deploy_key_state"].as_str().unwrap_or("missing") {
        "missing" => actions.push("register-key".into()),
        "read-only" => actions.push("replace-read-only-key".into()),
        _ => {}
    }
    if checkout == "missing" {
        actions.push("clone".into());
    } else {
        if let Some(error) = checkout_error(state) {
            return Err(error);
        }
        if !state["origin"]
            .as_str()
            .unwrap_or_default()
            .starts_with("git@github.com:")
        {
            actions.push("set-origin".into());
        }
        if state["ssh_config_state"] != json!("ready") {
            actions.push("set-ssh-command".into());
        }
        actions.push("fetch-upstream".into());
    }
    if !actions.is_empty() || state["access"] != json!("working") {
        actions.push("verify-access".into());
    }
    Ok(actions)
}

fn generate_key(workspace: &mut dyn Workspace, key: &str, public_key: &str, name: &str) -> Result<(), AppError> {
    let github = key.rsplit_once('/').map_or("", |(dir, _)| dir).to_owned();
    let homestack = github.rsplit_once('/').map_or("", |(dir, _)| dir).to_owned();
    let ssh = homestack.rsplit_once('/').map_or("", |(dir, _)| dir).to_owned();
    let (q_ssh, q_homestack, q_github) = (shell::quote(&ssh), shell::quote(&homestack), shell::quote(&github));
    run_checked(
        workspace,
        &format!(
            "install -d -m 700 {q_ssh} {q_homestack} {q_github} && chmod 700 {q_ssh} {q_homestack} {q_github} && umask 077 && ssh-keygen -q -t ed25519 -f {} -N '' -C {} && chmod 600 {} && chmod 644 {}",
            shell::quote(key),
            shell::quote(&format!("homestack:{name}")),
            shell::quote(key),
            shell::quote(public_key)
        ),
    )?;
    Ok(())
}

/// git, ssh and ssh-keygen on the workspace.
pub fn require_tools(state: &Value) -> Result<(), AppError> {
    let missing: Vec<&str> = ["git", "ssh", "ssh-keygen"]
        .into_iter()
        .filter(|tool| state["tools"][tool] != json!(true))
        .collect();
    if !missing.is_empty() {
        return Err(AppError::new(format!(
            "Workspace is missing required repository tools: {}",
            missing.join(", ")
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn fetch_upstream(
    workspace: &mut dyn Workspace,
    checkout: &str,
    key: &str,
    name: &str,
    upstream: &str,
    remote: &str,
    merge_ref: &str,
    upstream_ref: &str,
) -> Result<(), AppError> {
    let configured = output_of(
        workspace,
        &format!(
            "git -C {} remote get-url {}",
            shell::quote(checkout),
            shell::quote(remote)
        ),
    )?;
    if configured.is_empty() {
        return Err(AppError::new(format!(
            "Configured upstream remote {} is missing; refusing to update it",
            super::text::py_repr(remote)
        )));
    }
    let fetch_url = if repository_from_remote(Some(&configured)).as_deref() == Some(name) {
        format!("git@github.com:{name}.git")
    } else {
        configured
    };
    let refspec = format!("{merge_ref}:{upstream_ref}");
    let result = run(
        workspace,
        &format!(
            "GIT_SSH_COMMAND={} git -C {} -c core.hooksPath=/dev/null fetch --no-tags -- {} {}",
            shell::quote(&bootstrap_ssh(key)),
            shell::quote(checkout),
            shell::quote(&fetch_url),
            shell::quote(&refspec)
        ),
    )?;
    if result.code != 0 {
        return Err(AppError::new(format!(
            "Could not fetch configured repository upstream {upstream}; the working tree was not modified"
        )));
    }
    Ok(())
}

fn working_tree(workspace: &mut dyn Workspace, checkout: &str) -> Result<(String, Output), AppError> {
    let status = run(
        workspace,
        &format!(
            "git -C {} status --porcelain --untracked-files=all",
            shell::quote(checkout)
        ),
    )?;
    let tree = if status.code != 0 {
        "unavailable"
    } else if status.stdout.trim().is_empty() {
        "clean"
    } else {
        "modified"
    };
    Ok((tree.to_owned(), status))
}

/// Bring the repository to its configured state (Python
/// `setup_repository`); `activity` hears each step.
#[allow(clippy::too_many_lines)]
pub fn setup_repository(
    settings: &Settings,
    system: &dyn System,
    workspace: &mut dyn Workspace,
    state: &Value,
    activity: &mut dyn FnMut(&str),
) -> Result<Value, AppError> {
    require_tools(state)?;
    let actions = repository_setup_actions(state)?;
    if actions.is_empty() {
        let mut result = state.clone();
        result["changed"] = json!(false);
        result["message"] = json!("Repository is already configured.");
        return Ok(result);
    }
    let has = |action: &str| actions.iter().any(|a| a == action);
    let name = state["repository"].as_str().unwrap_or_default().to_owned();
    let workspace_name = state["workspace"].as_str().unwrap_or_default().to_owned();
    let vmid = u32::try_from(state["vmid"].as_u64().unwrap_or(0)).unwrap_or(0);
    let (checkout, key, public_key_path) = repository_paths(settings, &name)?;
    let title = format!("HomeStack {workspace_name}");
    let mut fast_forwarded = false;
    if has("generate-key") {
        activity("Generate deploy key");
        generate_key(workspace, &key, &public_key_path, &name)?;
    } else if has("derive-public-key") {
        activity("Restore public deploy key");
        run_checked(
            workspace,
            &format!(
                "umask 077 && ssh-keygen -y -f {} > {} && chmod 644 {}",
                shell::quote(&key),
                shell::quote(&public_key_path),
                shell::quote(&public_key_path)
            ),
        )?;
    } else if has("replace-missing-private-key") {
        activity("Replace incomplete deploy key");
        if let Some(id) = state["deploy_key_id"].as_i64() {
            delete_key(system, &name, id)?;
        }
        run_checked(workspace, &format!("rm -f -- {}", shell::quote(&public_key_path)))?;
        generate_key(workspace, &key, &public_key_path, &name)?;
    }
    activity("Reconcile GitHub deploy key");
    let public_key = output_of(workspace, &format!("cat {}", shell::quote(&public_key_path)))?;
    if identity(&public_key).is_none() {
        return Err(AppError::new("Workspace did not produce a valid public deploy key"));
    }
    let deploy_keys = keys(system, &name)?;
    let mut matching = matching_key(&deploy_keys, &public_key);
    if let Some(found) = &matching
        && found["read_only"] == json!(true)
    {
        let Some(id) = found["id"].as_i64() else {
            return Err(AppError::new("GitHub returned a deploy key without a numeric ID"));
        };
        delete_key(system, &name, id)?;
        matching = None;
    }
    if matching.is_none() {
        add_key(system, &name, &title, &public_key)?;
    }
    let remote_url = format!("git@github.com:{name}.git");
    let quoted = shell::quote(&checkout);
    if has("clone") {
        activity("Clone repository");
        let parent = checkout.rsplit_once('/').map_or("/", |(dir, _)| dir).to_owned();
        run_checked(workspace, &format!("mkdir -p -- {}", shell::quote(&parent)))?;
        run_checked(
            workspace,
            &format!(
                "GIT_SSH_COMMAND={} git clone -- {} {quoted}",
                shell::quote(&bootstrap_ssh(&key)),
                shell::quote(&remote_url)
            ),
        )?;
    }
    if has("fetch-upstream") {
        activity("Fetch repository upstream");
        let text = |key: &str| state[key].as_str().unwrap_or_default().to_owned();
        fetch_upstream(
            workspace,
            &checkout,
            &key,
            &name,
            &text("upstream"),
            &text("upstream_remote"),
            &text("upstream_merge"),
            &text("upstream_ref"),
        )?;
        let (tree, _) = working_tree(workspace, &checkout)?;
        let counts = upstream_counts(workspace, &checkout)?;
        let mut current = state.clone();
        current["working_tree"] = json!(tree);
        current["ahead"] = counts.map_or(Value::Null, |(ahead, _)| json!(ahead));
        current["behind"] = counts.map_or(Value::Null, |(_, behind)| json!(behind));
        if let Some(error) = checkout_error(&current).or_else(|| history_error(&current)) {
            return Err(error);
        }
        let behind = counts.map_or(0, |(_, behind)| behind);
        if behind > 0 {
            activity("Fast-forward repository");
            let merge_options = format!("branch.{}.mergeOptions=", text("branch"));
            let merged = run(
                workspace,
                &format!(
                    "git -C {quoted} -c {} -c core.hooksPath=/dev/null merge --ff-only --no-squash --no-autostash --no-overwrite-ignore @{{upstream}}",
                    shell::quote(&merge_options)
                ),
            )?;
            if merged.code != 0 {
                return Err(AppError::new(
                    "Repository could not be fast-forwarded; no reset or cleanup was attempted",
                ));
            }
            let verified = upstream_counts(workspace, &checkout)?;
            let (_, status) = working_tree(workspace, &checkout)?;
            if verified != Some((0, 0)) || status.code != 0 || !status.stdout.trim().is_empty() {
                return Err(AppError::new(
                    "Repository fast-forward completed without a clean up-to-date checkout",
                ));
            }
            fast_forwarded = true;
        } else {
            activity("Repository already up to date");
        }
    }
    if has("set-origin") {
        activity("Configure Git remote");
        run_checked(
            workspace,
            &format!("git -C {quoted} remote set-url origin {}", shell::quote(&remote_url)),
        )?;
    }
    if has("clone") || has("set-ssh-command") {
        activity("Configure repository SSH");
        run_checked(
            workspace,
            &format!(
                "git -C {quoted} config --local core.sshCommand {}",
                shell::quote(&stored_ssh(&key))
            ),
        )?;
    }
    activity("Verify Git access");
    let verify = run(
        workspace,
        &format!(
            "GIT_SSH_COMMAND={} git ls-remote {} HEAD",
            shell::quote(&bootstrap_ssh(&key)),
            shell::quote(&remote_url)
        ),
    )?;
    if verify.code != 0 {
        return Err(AppError::new(
            "Repository setup completed but Git access verification failed",
        ));
    }
    activity("Refresh repository status");
    let mut refreshed = inspect_repository(settings, system, workspace, vmid, &workspace_name, &name, true)?;
    let configuration_changed = actions.iter().any(|a| a != "verify-access" && a != "fetch-upstream");
    let message = if fast_forwarded {
        "Repository fast-forwarded to its upstream."
    } else if configuration_changed {
        "Repository setup completed."
    } else {
        "Repository is already up to date."
    };
    refreshed["changed"] = json!(fast_forwarded || configuration_changed);
    refreshed["message"] = json!(message);
    Ok(refreshed)
}

/// Replace the deploy key with a new one (Python `rotate_repository_key`).
pub fn rotate_repository_key(
    settings: &Settings,
    system: &dyn System,
    workspace: &mut dyn Workspace,
    state: &Value,
    activity: &mut dyn FnMut(&str),
) -> Result<Value, AppError> {
    require_tools(state)?;
    let Some(id) = state["deploy_key_id"]
        .as_i64()
        .filter(|_| state["checkout_state"] == json!("ready"))
    else {
        return Err(AppError::new(
            "Repository deploy key is not configured; run Setup first",
        ));
    };
    let name = state["repository"].as_str().unwrap_or_default().to_owned();
    let workspace_name = state["workspace"].as_str().unwrap_or_default().to_owned();
    let vmid = u32::try_from(state["vmid"].as_u64().unwrap_or(0)).unwrap_or(0);
    let (_, key, public_key_path) = repository_paths(settings, &name)?;
    let temporary = format!("{key}.rotate-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let temporary_pub = format!("{temporary}.pub");
    let rotated = (|| -> Result<(), AppError> {
        activity("Generate replacement deploy key");
        run_checked(
            workspace,
            &format!(
                "umask 077 && ssh-keygen -q -t ed25519 -f {} -N '' -C {}",
                shell::quote(&temporary),
                shell::quote(&format!("homestack:{name}"))
            ),
        )?;
        activity("Remove previous GitHub deploy key");
        delete_key(system, &name, id)?;
        activity("Install replacement deploy key");
        run_checked(
            workspace,
            &format!(
                "mv -f -- {} {} && mv -f -- {} {} && chmod 600 {} && chmod 644 {}",
                shell::quote(&temporary),
                shell::quote(&key),
                shell::quote(&temporary_pub),
                shell::quote(&public_key_path),
                shell::quote(&key),
                shell::quote(&public_key_path)
            ),
        )?;
        activity("Register GitHub deploy key");
        let public_key = output_of(workspace, &format!("cat {}", shell::quote(&public_key_path)))?;
        add_key(system, &name, &format!("HomeStack {workspace_name}"), &public_key)
    })();
    let _ = run(
        workspace,
        &format!("rm -f -- {} {}", shell::quote(&temporary), shell::quote(&temporary_pub)),
    );
    rotated?;
    activity("Verify Git access");
    let remote_url = format!("git@github.com:{name}.git");
    let verify = run(
        workspace,
        &format!(
            "GIT_SSH_COMMAND={} git ls-remote {} HEAD",
            shell::quote(&bootstrap_ssh(&key)),
            shell::quote(&remote_url)
        ),
    )?;
    if verify.code != 0 {
        return Err(AppError::new(
            "Deploy key was rotated but Git access verification failed. Re-run Setup to repair registration.",
        ));
    }
    activity("Refresh repository status");
    let mut refreshed = inspect_repository(settings, system, workspace, vmid, &workspace_name, &name, true)?;
    refreshed["changed"] = json!(true);
    refreshed["message"] = json!("Repository deploy key rotated.");
    Ok(refreshed)
}
