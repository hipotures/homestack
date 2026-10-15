//! Repositories: the helpers and setup actions of `repo`, and setup on real
//! temporary Git working trees (Python `test_repo.py`,
//! `test_setup_repositories.py`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod setup_fixture;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use common::FakeSystem;
use hs_cli::AppError;
use hs_cli::setup::Settings;
use hs_cli::setup::cli::check_role;
use hs_cli::setup::definitions::{Entry, Params};
use hs_cli::setup::engine::{Connection, Quiet, execute_plan, preflight_entry};
use hs_cli::setup::plan::{Plan, repository_paths};
use hs_cli::setup::repo::{
    inspect_repository, repository_from_remote, repository_setup_actions, repository_setup_steps,
    resolve_repository_argument, setup_repository,
};
use hs_cli::setup::workspace::Workspace;
use hs_cli::system::Output;
use serde_json::{Value, json};

const REPOSITORY: &str = "example-owner/example-project";

// ---- RepositoryHelpersTests ----

/// Port of `test_repository_argument_uses_explicit_value_or_owner_workspace_default`.
#[test]
fn repository_argument_uses_explicit_value_or_owner_workspace_default() {
    assert_eq!(
        resolve_repository_argument(Some("example-owner"), Some(REPOSITORY), "workspace-name").unwrap(),
        REPOSITORY
    );
    assert_eq!(
        resolve_repository_argument(Some("example-owner"), None, "workspace-name").unwrap(),
        "example-owner/workspace-name"
    );
    let error = resolve_repository_argument(None, None, "workspace-name").unwrap_err();
    assert!(error.0.contains("[repo] owner"), "{}", error.0);
}

/// Port of `test_numeric_target_can_resolve_to_workspace_named_repository`.
#[test]
fn numeric_target_can_resolve_to_workspace_named_repository() {
    assert_eq!(
        resolve_repository_argument(Some("example-owner"), None, "example-project").unwrap(),
        REPOSITORY
    );
}

/// Port of `test_repository_workspace_info_ignores_legacy_lifecycle_layout`:
/// only the role tags decide (the server reports no mount layout).
#[test]
fn repository_workspace_info_ignores_legacy_lifecycle_layout() {
    assert!(!check_role(201, &["homestack-ws"]).unwrap());
}

/// Port of `test_repository_workspace_info_still_requires_workspace_tag`.
#[test]
fn repository_workspace_info_still_requires_workspace_tag() {
    let error = check_role(201, &[]).unwrap_err();
    assert!(error.0.contains("homestack-ws"), "{}", error.0);
}

/// Port of `test_repository_paths_use_persistent_home`.
#[test]
fn repository_paths_use_persistent_home() {
    let home = tempfile::tempdir().unwrap();
    let (checkout, key, public) = repository_paths(&setup_fixture::settings(home.path()), REPOSITORY).unwrap();
    assert_eq!(checkout, "/home/user/DEV/example-project");
    assert_eq!(key, "/home/user/.ssh/homestack/github/example-owner-example-project");
    assert_eq!(public, format!("{key}.pub"));
}

/// Port of `test_supported_github_remotes_resolve_to_owner_repo`.
#[test]
fn supported_github_remotes_resolve_to_owner_repo() {
    for value in [
        "git@github.com:example-owner/example-project.git",
        "git@github.com:example-owner/example-project",
        "https://github.com/example-owner/example-project.git",
        "ssh://git@github.com/example-owner/example-project.git",
    ] {
        assert_eq!(
            repository_from_remote(Some(value)).as_deref(),
            Some(REPOSITORY),
            "{value}"
        );
    }
    assert_eq!(
        repository_from_remote(Some("git@example.com:example-owner/example-project.git")),
        None
    );
}

// ---- RepositorySetupPlanTests ----

fn ready_state(updates: &Value) -> Value {
    let mut state = json!({
        "checkout_state": "ready", "key_state": "ready", "deploy_key_state": "read-write",
        "origin": "git@github.com:example-owner/example-project.git", "ssh_config_state": "ready",
        "access": "working", "working_tree": "clean", "head_state": "attached", "branch": "main",
        "upstream": "origin/main", "upstream_remote": "origin", "upstream_merge": "refs/heads/main",
        "upstream_ref": "refs/remotes/origin/main", "ahead": 0, "behind": 0,
    });
    for (key, value) in updates.as_object().unwrap() {
        state[key] = value.clone();
    }
    state
}

fn fresh() -> Value {
    json!({"checkout_state": "missing", "key_state": "missing", "deploy_key_state": "missing",
           "origin": null, "ssh_config_state": "missing", "access": "not-tested"})
}

/// Port of `test_ready_repository_fetches_before_reporting_noop`.
#[test]
fn ready_repository_fetches_before_reporting_noop() {
    assert_eq!(
        repository_setup_actions(&ready_state(&json!({}))).unwrap(),
        ["fetch-upstream", "verify-access"]
    );
}

/// Port of `test_existing_https_checkout_is_not_recloned`.
#[test]
fn existing_https_checkout_is_not_recloned() {
    let actions = repository_setup_actions(&ready_state(&json!({
        "key_state": "missing", "deploy_key_state": "missing",
        "origin": "https://github.com/example-owner/example-project.git",
        "ssh_config_state": "missing", "access": "not-tested",
    })))
    .unwrap();
    assert_eq!(
        actions,
        [
            "generate-key",
            "register-key",
            "set-origin",
            "set-ssh-command",
            "fetch-upstream",
            "verify-access"
        ]
    );
    assert!(!actions.iter().any(|a| a == "clone"));
}

/// Port of `test_missing_checkout_is_cloned_without_destructive_actions`.
#[test]
fn missing_checkout_is_cloned_without_destructive_actions() {
    let actions = repository_setup_actions(&ready_state(&fresh())).unwrap();
    assert!(actions.iter().any(|a| a == "clone"));
    assert!(!actions.iter().any(|a| a == "reset" || a == "clean"));
}

/// Port of `test_fresh_setup_steps_are_visible_in_execution_order`.
#[test]
fn fresh_setup_steps_are_visible_in_execution_order() {
    let actions = repository_setup_actions(&ready_state(&fresh())).unwrap();
    assert_eq!(
        repository_setup_steps(&actions),
        [
            "Generate deploy key",
            "Reconcile GitHub deploy key",
            "Clone repository",
            "Configure repository SSH",
            "Verify Git access",
            "Refresh repository status"
        ]
    );
}

/// Port of `test_conflicting_checkout_is_rejected`.
#[test]
fn conflicting_checkout_is_rejected() {
    for checkout_state in [
        "not-a-repository",
        "repository-without-recognized-origin",
        "different-repository",
    ] {
        let error = repository_setup_actions(&ready_state(&json!({"checkout_state": checkout_state}))).unwrap_err();
        assert!(error.0.contains("will not clean, reset, or replace"), "{}", error.0);
    }
}

/// Port of `test_dirty_checkout_is_rejected`.
#[test]
fn dirty_checkout_is_rejected() {
    let error = repository_setup_actions(&ready_state(&json!({"working_tree": "modified"}))).unwrap_err();
    let at = error.0.find("uncommitted").expect("uncommitted");
    assert!(error.0[at..].contains("untracked"), "{}", error.0);
}

/// Port of `test_history_is_rechecked_after_fetch`.
#[test]
fn history_is_rechecked_after_fetch() {
    for (ahead, behind) in [(1, 0), (1, 1)] {
        let actions = repository_setup_actions(&ready_state(&json!({"ahead": ahead, "behind": behind}))).unwrap();
        assert!(actions.iter().any(|a| a == "fetch-upstream"));
    }
}

/// Port of `test_detached_head_is_rejected`.
#[test]
fn detached_head_is_rejected() {
    let error = repository_setup_actions(&ready_state(&json!({"head_state": "detached", "branch": null}))).unwrap_err();
    assert!(error.0.contains("detached HEAD"), "{}", error.0);
}

/// Port of `test_missing_upstream_is_rejected`.
#[test]
fn missing_upstream_is_rejected() {
    let error = repository_setup_actions(&ready_state(&json!({"upstream": null}))).unwrap_err();
    assert!(error.0.contains("no configured upstream"), "{}", error.0);
}

// ---- RepositoryIntegrationTests: real temporary Git working trees ----

fn git(args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn available() -> bool {
    ["git", "ssh-keygen"].iter().all(|tool| {
        Command::new("sh")
            .args(["-c", &format!("command -v {tool}")])
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// The deploy keys GitHub holds, by repository.
type Keys = Arc<Mutex<HashMap<String, Vec<Value>>>>;

/// A workspace whose `/home/user` is a temporary directory and whose
/// GitHub remotes are local bare repositories (Python `run_workspace`).
struct GitWorkspace {
    home: PathBuf,
    sources: HashMap<String, PathBuf>,
    commands: Vec<String>,
}

impl Workspace for GitWorkspace {
    fn run(&mut self, command: &str, input: Option<&str>, _interactive: bool) -> Result<Output, AppError> {
        self.commands.push(command.to_owned());
        if input.is_some() {
            // The guest program (Python patches `setup.guest`).
            return Ok(Output::stdout(r#"{"ok": true}"#));
        }
        let home = self.home.display().to_string();
        let mut command = command.replace("/home/user", &home);
        if command.contains("git ls-remote") {
            return Ok(Output::default());
        }
        if let Some((_, tail)) = command.split_once("git clone -- ") {
            let words: Vec<String> = tail
                .split_whitespace()
                .map(|word| word.trim_matches('\'').to_owned())
                .collect();
            let (url, destination) = (&words[0], &words[1]);
            let identity = repository_from_remote(Some(url)).unwrap();
            let source = self.sources[&identity].display().to_string();
            git(&["clone", "--", &source, destination]);
            git(&["-C", destination, "remote", "set-url", "origin", url]);
            return Ok(Output::default());
        }
        for (identity, source) in &self.sources {
            command = command.replace(&format!("git@github.com:{identity}.git"), &source.display().to_string());
        }
        let output = Command::new("/bin/sh")
            .args(["-c", &command])
            .stdin(Stdio::null())
            .output()
            .expect("sh runs");
        Ok(Output {
            code: output.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&output.stdout).replace(&home, "/home/user"),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn transfer(&mut self, _source: &str, _destination: &str) -> Result<(), AppError> {
        Err(AppError::new("no transfers in these tests"))
    }
}

/// GitHub through `gh api`: repositories with administration permission,
/// their deploy keys, and no revocation (Python `github`, `add_key`).
fn github(keys: &Keys, admin: bool) -> FakeSystem {
    let keys = Arc::clone(keys);
    FakeSystem::new().run(move |argv| {
        let args = &argv[4..];
        if args.first().is_some_and(|a| a == "--method") {
            let endpoint = &args[2];
            assert_eq!(args[1], "POST", "Unexpected key revocation: {args:?}");
            let identity = endpoint
                .trim_start_matches("repos/")
                .trim_end_matches("/keys")
                .to_owned();
            let public = args
                .iter()
                .find_map(|a| a.strip_prefix("key="))
                .unwrap_or_default()
                .to_owned();
            let title = args
                .iter()
                .find_map(|a| a.strip_prefix("title="))
                .unwrap_or_default()
                .to_owned();
            let mut keys = keys.lock().unwrap();
            let id = keys.len() + 1;
            keys.entry(identity)
                .or_default()
                .push(json!({"id": id, "title": title, "key": public, "read_only": false}));
            return Ok(Output::stdout("{}"));
        }
        let endpoint = args.last().unwrap().split('?').next().unwrap_or_default().to_owned();
        if let Some(identity) = endpoint.strip_prefix("repos/").and_then(|e| e.strip_suffix("/keys")) {
            let found = keys.lock().unwrap().get(identity).cloned().unwrap_or_default();
            return Ok(Output::stdout(&json!([found]).to_string()));
        }
        let identity = endpoint.trim_start_matches("repos/");
        Ok(Output::stdout(
            &json!({"full_name": identity, "permissions": {"admin": admin}}).to_string(),
        ))
    })
}

struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    settings: Settings,
    seeds: HashMap<String, PathBuf>,
    workspace: GitWorkspace,
    keys: Keys,
    system: FakeSystem,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let home = root.join("home");
        std::fs::create_dir(&home).unwrap();
        let mut sources = HashMap::new();
        let mut seeds = HashMap::new();
        for name in ["owner/one", "owner/two"] {
            let flat = name.replace('/', "-");
            let source = root.join(&flat);
            let seed = root.join(format!("seed-{flat}"));
            let (source_text, seed_text) = (source.display().to_string(), seed.display().to_string());
            git(&["init", "--bare", &source_text]);
            git(&["init", "-b", "main", &seed_text]);
            git(&["-C", &seed_text, "config", "user.name", "HomeStack tests"]);
            git(&["-C", &seed_text, "config", "user.email", "tests@example.invalid"]);
            std::fs::write(seed.join("README"), format!("{name} initial\n")).unwrap();
            std::fs::write(seed.join(".gitignore"), "secret\n").unwrap();
            git(&["-C", &seed_text, "add", "README", ".gitignore"]);
            git(&["-C", &seed_text, "commit", "-m", "Initial commit"]);
            git(&["-C", &seed_text, "remote", "add", "origin", &source_text]);
            git(&["-C", &seed_text, "push", "-u", "origin", "main"]);
            git(&["--git-dir", &source_text, "symbolic-ref", "HEAD", "refs/heads/main"]);
            sources.insert(name.to_owned(), source);
            seeds.insert(name.to_owned(), seed);
        }
        let keys = Keys::default();
        let system = github(&keys, true);
        Self {
            settings: setup_fixture::settings(&root.join("desktop")),
            workspace: GitWorkspace {
                home: home.clone(),
                sources,
                commands: Vec::new(),
            },
            home,
            seeds,
            keys,
            system,
            _tmp: tmp,
        }
    }

    fn inspect(&mut self, identity: &str, verify_access: bool) -> Value {
        inspect_repository(
            &self.settings,
            &self.system,
            &mut self.workspace,
            200,
            "workspace",
            identity,
            verify_access,
        )
        .unwrap()
    }

    fn setup(&mut self, state: &Value) -> Result<Value, AppError> {
        setup_repository(&self.settings, &self.system, &mut self.workspace, state, &mut |_| {})
    }

    /// Python `_setup_repository`.
    fn provision(&mut self, identity: &str) -> Value {
        let state = self.inspect(identity, false);
        let result = self.setup(&state).unwrap();
        assert_eq!(result["ready"], true, "{result}");
        result
    }

    /// Python `_advance_source`.
    fn advance(&self, identity: &str) {
        let seed = &self.seeds[identity];
        let seed_text = seed.display().to_string();
        std::fs::write(seed.join("advance"), "new upstream content\n").unwrap();
        git(&["-C", &seed_text, "add", "advance"]);
        git(&["-C", &seed_text, "commit", "-m", "Advance upstream"]);
        git(&["-C", &seed_text, "push", "origin", "main"]);
    }

    fn checkout(&self) -> PathBuf {
        self.home.join("DEV/one")
    }

    fn commit_locally(&self) {
        let checkout = self.checkout().display().to_string();
        git(&["-C", &checkout, "config", "user.name", "HomeStack tests"]);
        git(&["-C", &checkout, "config", "user.email", "tests@example.invalid"]);
        std::fs::write(self.checkout().join("local"), "local commit\n").unwrap();
        git(&["-C", &checkout, "add", "local"]);
        git(&["-C", &checkout, "commit", "-m", "Local commit"]);
    }

    fn head(&self) -> String {
        git(&["-C", &self.checkout().display().to_string(), "rev-parse", "HEAD"])
            .trim()
            .to_owned()
    }

    fn counts(&self) -> String {
        git(&[
            "-C",
            &self.checkout().display().to_string(),
            "rev-list",
            "--left-right",
            "--count",
            "HEAD...@{upstream}",
        ])
        .trim()
        .to_owned()
    }

    fn ran(&self, text: &str) -> usize {
        self.workspace.commands.iter().filter(|c| c.contains(text)).count()
    }
}

fn repository_entry() -> Entry {
    Entry {
        id: "owner/one".to_owned(),
        group: "repo".to_owned(),
        handler: "repository".to_owned(),
        label: "One".to_owned(),
        description: "Repository".to_owned(),
        params: Params::Repository {
            repository: "owner/one".to_owned(),
        },
        depends_on: Vec::new(),
    }
}

/// Port of `test_two_repositories_reuse_keys_isolate_ssh`.
#[test]
fn two_repositories_reuse_keys_isolate_ssh() {
    if !available() {
        return;
    }
    let mut f = Fixture::new();
    for identity in ["owner/one", "owner/two"] {
        assert_eq!(f.provision(identity)["changed"], true);
    }
    let identities: Vec<String> = ["one", "two"]
        .iter()
        .map(|name| {
            let checkout = f.home.join("DEV").join(name).display().to_string();
            git(&["-C", &checkout, "config", "--local", "--get", "core.sshCommand"])
        })
        .collect();
    assert_ne!(identities[0], identities[1]);
    assert_eq!(f.ran("git clone --"), 2);
    assert_eq!(f.ran("git pull") + f.ran("git reset") + f.ran("git clean"), 0);
    assert_eq!(f.keys.lock().unwrap().len(), 2);
}

/// Port of `test_existing_dirty_repository_is_rejected`.
#[test]
fn existing_dirty_repository_is_rejected() {
    if !available() {
        return;
    }
    let mut f = Fixture::new();
    f.provision("owner/one");
    let dirty = f.checkout().join("uncommitted");
    std::fs::write(&dirty, "User data stays untouched").unwrap();
    let key = f.home.join(".ssh/homestack/github/owner-one");
    let key_before = std::fs::read(&key).unwrap();
    let state = f.inspect("owner/one", true);
    assert_eq!(state["working_tree"], "modified");
    f.workspace.commands.clear();
    let error = f.setup(&state).unwrap_err();
    assert!(error.0.contains("uncommitted"), "{}", error.0);
    assert_eq!(std::fs::read_to_string(&dirty).unwrap(), "User data stays untouched");
    assert_eq!(std::fs::read(&key).unwrap(), key_before);
    assert_eq!(f.ran("fetch --no-tags"), 0);
}

/// Port of `test_existing_up_to_date_repository_is_a_noop`.
#[test]
fn existing_up_to_date_repository_is_a_noop() {
    if !available() {
        return;
    }
    let mut f = Fixture::new();
    f.provision("owner/one");
    f.workspace.commands.clear();
    let state = f.inspect("owner/one", false);
    let result = f.setup(&state).unwrap();
    assert_eq!(result["changed"], false);
    assert_eq!(result["message"], "Repository is already up to date.");
    assert!(f.ran("fetch --no-tags") > 0);
    assert_eq!(f.ran("merge --ff-only"), 0);
}

/// Port of `test_existing_behind_repository_is_fast_forwarded`.
#[test]
fn existing_behind_repository_is_fast_forwarded() {
    if !available() {
        return;
    }
    let mut f = Fixture::new();
    f.provision("owner/one");
    f.advance("owner/one");
    let state = f.inspect("owner/one", false);
    let result = f.setup(&state).unwrap();
    assert_eq!(result["changed"], true);
    assert_eq!(result["message"], "Repository fast-forwarded to its upstream.");
    assert_eq!(f.counts(), "0\t0");
    assert_eq!(f.ran("merge --ff-only"), 1);
}

/// Port of `test_existing_behind_repository_ignores_unsafe_merge_options`.
#[test]
fn existing_behind_repository_ignores_unsafe_merge_options() {
    if !available() {
        return;
    }
    let mut f = Fixture::new();
    f.provision("owner/one");
    let checkout = f.checkout().display().to_string();
    git(&["-C", &checkout, "config", "branch.main.mergeOptions", "--squash"]);
    f.advance("owner/one");
    let state = f.inspect("owner/one", false);
    let result = f.setup(&state).unwrap();
    assert_eq!(result["changed"], true);
    assert_eq!(f.counts(), "0\t0");
    let merge = f
        .workspace
        .commands
        .iter()
        .find(|c| c.contains("merge --ff-only"))
        .unwrap();
    assert!(
        merge.contains("--no-squash --no-autostash --no-overwrite-ignore"),
        "{merge}"
    );
    assert!(merge.contains("core.hooksPath=/dev/null"), "{merge}");
}

/// Port of `test_fast_forward_does_not_overwrite_an_ignored_untracked_file`.
#[test]
fn fast_forward_does_not_overwrite_an_ignored_untracked_file() {
    if !available() {
        return;
    }
    let mut f = Fixture::new();
    f.provision("owner/one");
    let checkout = f.checkout().display().to_string();
    let secret = f.checkout().join("secret");
    std::fs::write(&secret, "local ignored data\n").unwrap();
    assert!(git(&["-C", &checkout, "status", "--porcelain"]).is_empty());
    let seed = f.seeds["owner/one"].clone();
    let seed_text = seed.display().to_string();
    std::fs::write(seed.join("secret"), "upstream data\n").unwrap();
    git(&["-C", &seed_text, "add", "-f", "secret"]);
    git(&["-C", &seed_text, "commit", "-m", "Track ignored path"]);
    git(&["-C", &seed_text, "push", "origin", "main"]);
    let head_before = f.head();
    let state = f.inspect("owner/one", false);
    let error = f.setup(&state).unwrap_err();
    assert!(error.0.contains("could not be fast-forwarded"), "{}", error.0);
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "local ignored data\n");
    assert_eq!(f.head(), head_before);
}

/// Port of `test_existing_ahead_repository_is_rejected`.
#[test]
fn existing_ahead_repository_is_rejected() {
    if !available() {
        return;
    }
    let mut f = Fixture::new();
    f.provision("owner/one");
    f.commit_locally();
    let head_before = f.head();
    let state = f.inspect("owner/one", false);
    f.workspace.commands.clear();
    let error = f.setup(&state).unwrap_err();
    assert!(error.0.contains("local commits"), "{}", error.0);
    assert!(f.ran("fetch --no-tags") > 0);
    assert_eq!(f.ran("merge --ff-only"), 0);
    assert_eq!(f.head(), head_before);
}

/// Port of `test_setup_reports_rejected_repository_as_a_failed_item`.
#[test]
fn setup_reports_rejected_repository_as_a_failed_item() {
    if !available() {
        return;
    }
    let mut f = Fixture::new();
    f.provision("owner/one");
    f.commit_locally();
    let plan = Plan {
        target: setup_fixture::target(),
        entries: vec![repository_entry()],
        catalog_id: None,
        unattended: true,
    };
    let result = execute_plan(
        &f.settings,
        &f.system,
        &plan,
        Connection::Open(&mut f.workspace),
        &mut Quiet,
    );
    assert_eq!(result["ok"], false, "{result}");
    assert_eq!(result["results"][0]["status"], "failed", "{result}");
    let detail = result["results"][0]["detail"].as_str().unwrap();
    assert!(detail.contains("local commits ahead"), "{detail}");
}

/// Port of `test_existing_diverged_repository_is_rejected`.
#[test]
fn existing_diverged_repository_is_rejected() {
    if !available() {
        return;
    }
    let mut f = Fixture::new();
    f.provision("owner/one");
    f.commit_locally();
    let head_before = f.head();
    f.advance("owner/one");
    let state = f.inspect("owner/one", false);
    assert_eq!((state["ahead"].clone(), state["behind"].clone()), (json!(1), json!(0)));
    let error = f.setup(&state).unwrap_err();
    assert!(error.0.contains("diverged"), "{}", error.0);
    assert_eq!(f.ran("merge --ff-only"), 0);
    assert_eq!(f.head(), head_before);
}

/// Port of `test_existing_detached_head_is_rejected`.
#[test]
fn existing_detached_head_is_rejected() {
    if !available() {
        return;
    }
    let mut f = Fixture::new();
    f.provision("owner/one");
    git(&[
        "-C",
        &f.checkout().display().to_string(),
        "checkout",
        "--detach",
        "HEAD",
    ]);
    let state = f.inspect("owner/one", false);
    let error = f.setup(&state).unwrap_err();
    assert!(error.0.contains("detached HEAD"), "{}", error.0);
}

/// Port of `test_existing_missing_upstream_is_rejected`.
#[test]
fn existing_missing_upstream_is_rejected() {
    if !available() {
        return;
    }
    let mut f = Fixture::new();
    f.provision("owner/one");
    git(&["-C", &f.checkout().display().to_string(), "branch", "--unset-upstream"]);
    let state = f.inspect("owner/one", false);
    let error = f.setup(&state).unwrap_err();
    assert!(error.0.contains("no configured upstream"), "{}", error.0);
}

/// Port of `test_permissions_and_read_only_key_block_preflight_without_mutations`.
#[test]
fn permissions_and_read_only_key_block_preflight_without_mutations() {
    if !available() {
        return;
    }
    let mut f = Fixture::new();
    let entry = repository_entry();
    let target = setup_fixture::target();
    let without_admin = github(&f.keys, false);
    let error = preflight_entry(&mut f.workspace, &f.settings, &target, &without_admin, &entry, true).unwrap_err();
    assert!(error.0.contains("permission"), "{}", error.0);
    assert!(f.workspace.commands.is_empty());
    // A checkout whose deploy key GitHub holds read-only.
    f.provision("owner/one");
    for key in f.keys.lock().unwrap().values_mut().flatten() {
        key["read_only"] = json!(true);
    }
    let error = preflight_entry(&mut f.workspace, &f.settings, &target, &f.system, &entry, true).unwrap_err();
    assert!(error.0.contains("maintenance"), "{}", error.0);
}

/// Port of `test_existing_non_git_directory_is_never_replaced`.
#[test]
fn existing_non_git_directory_is_never_replaced() {
    if !available() {
        return;
    }
    let mut f = Fixture::new();
    let destination = f.checkout();
    std::fs::create_dir_all(&destination).unwrap();
    std::fs::write(destination.join("keep"), "Existing data").unwrap();
    let state = f.inspect("owner/one", false);
    let error = f.setup(&state).unwrap_err();
    assert!(error.0.contains("unsafe"), "{}", error.0);
    assert_eq!(
        std::fs::read_to_string(destination.join("keep")).unwrap(),
        "Existing data"
    );
}

/// Inspection reads every repository in one guest call: an absent
/// checkout, and a directory that is not a Git checkout (a conflict that
/// would be overwritten, with its metadata).
#[test]
fn inspection_reports_repository_checkouts() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::create_dir_all(home.join("DEV/one")).unwrap();
    let mut two = repository_entry();
    two.id = "owner/two".to_owned();
    two.params = Params::Repository {
        repository: "owner/two".to_owned(),
    };
    let mut workspace = setup_fixture::TestWorkspace::new(home);
    let state = hs_cli::setup::engine::inspect_workspace_state(
        &mut workspace,
        &setup_fixture::settings(home),
        &setup_fixture::target(),
        &[repository_entry(), two],
    )
    .unwrap();
    let one = &state["items"]["owner/one"];
    assert_eq!(one["state"], "conflict", "{one}");
    assert_eq!(one["ready"], false);
    assert_eq!(one["will_overwrite"], true);
    assert_eq!(one["files"][0]["path"], "DEV/one");
    let two = &state["items"]["owner/two"];
    assert_eq!(two["state"], "absent", "{two}");
    assert_eq!(two["will_overwrite"], false);
    assert_eq!(two["files"], json!([]));
    let calls = workspace
        .guests
        .iter()
        .filter(|payload| payload["operation"] == "repositories")
        .count();
    assert_eq!(calls, 1);
}
