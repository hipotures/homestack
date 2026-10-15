//! The program run on the workspace (`guest/setup_guest.py`, Python's,
//! sent unchanged): run here through `python3 -` exactly as the setup
//! streams it (Python `test_setup.py::EnvironmentTests`,
//! `AdditionalSafetyTests`, `test_setup_state.py::GuestStateTests`,
//! `test_alien_workspace.py::GuestIdentityTests`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod setup_fixture;

use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use hs_cli::setup::guest::PROGRAM;
use serde_json::{Value, json};
use setup_fixture::{run_guest, run_guest_with_path};

const BINS: [&str; 2] = ["~/.local/bin", "~/.opencode/bin"];

fn environment(home: &Path, profile: &str, apply: bool) -> Value {
    run_guest(&json!({"operation": "environment", "home": home.display().to_string(),
                      "profile": profile, "bins": BINS, "apply": apply}))
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

fn have(program: &str) -> bool {
    hs_cli::system::which(program).is_some()
}

fn inode(path: &Path) -> u64 {
    path.metadata().unwrap().ino()
}

#[test]
fn the_guest_program_is_python_s_byte_for_byte() {
    let python = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../python/src/homestack/setup_guest.py");
    // Python's tree goes in the last phase; until then both are the same.
    if let Ok(text) = std::fs::read_to_string(python) {
        assert_eq!(PROGRAM, text);
    }
}

#[test]
fn bash_generates_canonical_files_replaces_existing_content_and_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::write(home.join(".bashrc"), "export CUSTOM_BASHRC=preserved\n").unwrap();
    std::fs::write(home.join(".bash_profile"), "export CUSTOM_LOGIN=preserved\n").unwrap();
    std::fs::write(home.join(".profile"), "export WRONG_LOGIN=used\n").unwrap();
    let names = [".bashrc", ".bash_profile", ".profile"];
    let before: Vec<Vec<u8>> = names.iter().map(|n| std::fs::read(home.join(n)).unwrap()).collect();
    let inodes: Vec<u64> = names.iter().map(|n| inode(&home.join(n))).collect();
    assert_eq!(
        strings(&environment(home, "bash", false)["changed"]),
        [".bashrc", ".bash_profile"]
    );
    assert_eq!(
        names
            .iter()
            .map(|n| std::fs::read(home.join(n)).unwrap())
            .collect::<Vec<_>>(),
        before
    );
    assert_eq!(names.iter().map(|n| inode(&home.join(n))).collect::<Vec<_>>(), inodes);

    assert_eq!(
        strings(&environment(home, "bash", true)["changed"]),
        [".bashrc", ".bash_profile"]
    );
    let bashrc = std::fs::read_to_string(home.join(".bashrc")).unwrap();
    assert!(!bashrc.contains("CUSTOM_BASHRC"));
    assert!(
        !std::fs::read_to_string(home.join(".bash_profile"))
            .unwrap()
            .contains("CUSTOM_LOGIN")
    );
    assert_eq!(std::fs::read(home.join(".profile")).unwrap(), before[2]);
    assert!(strings(&environment(home, "bash", true)["changed"]).is_empty());
    assert_eq!(std::fs::read_to_string(home.join(".bashrc")).unwrap(), bashrc);
    assert!(
        std::fs::read_dir(home).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".homestack-backup-"))
    );
    let local_bin = home.join(".local/bin").display().to_string();
    for (args, login) in [
        (
            vec![
                "--noprofile".to_owned(),
                "--rcfile".to_owned(),
                home.join(".bashrc").display().to_string(),
                "-ic".to_owned(),
            ],
            false,
        ),
        (vec!["--noprofile".to_owned(), "-lc".to_owned()], true),
    ] {
        let command = format!(
            "{}printf \"%s|%s|%s\" \"${{CUSTOM_BASHRC:-}}\" \"${{CUSTOM_LOGIN:-}}\" \"$PATH\"",
            if login { "source \"$HOME/.bash_profile\"; " } else { "" }
        );
        let output = Command::new("bash")
            .args(&args)
            .arg(command)
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("preserved"));
        assert_eq!(stdout.matches(&local_bin).count(), 1, "{stdout}");
    }
    assert_eq!(
        std::fs::read_to_string(home.join(".profile")).unwrap(),
        "export WRONG_LOGIN=used\n"
    );
}

#[test]
fn bash_login_precedence_and_unsafe_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let mut paths = strings(&environment(home, "bash", true)["paths"]);
    paths.sort();
    assert_eq!(paths, [".bashrc", ".profile"]);
    std::fs::remove_file(home.join(".bashrc")).unwrap();
    std::os::unix::fs::symlink(home.join(".profile"), home.join(".bashrc")).unwrap();
    let refused = environment(home, "bash", true);
    assert_eq!(refused["ok"], json!(false));
    assert!(refused["error"].as_str().unwrap().contains("Symlink"));
    std::fs::remove_file(home.join(".bashrc")).unwrap();
    std::os::unix::fs::symlink(home.join(".profile"), home.join(".bash_profile")).unwrap();
    assert!(
        environment(home, "bash", true)["error"]
            .as_str()
            .unwrap()
            .contains("Symlink")
    );
}

#[test]
fn non_bash_templates_replace_existing_content_deterministically() {
    for (profile, shell, relatives) in [
        ("zsh", "zsh", &[".zshenv", ".zshrc"][..]),
        ("fish", "fish", &[".config/fish/conf.d/homestack.fish"]),
        ("nu", "nu", &[".config/nushell/env.nu", ".config/nushell/config.nu"]),
    ] {
        // The program checks the startup files with the shell itself.
        if !have(shell) {
            continue;
        }
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        for relative in relatives {
            let path = home.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("custom {profile}\n")).unwrap();
        }
        let applied = environment(home, profile, true);
        assert_eq!(strings(&applied["paths"]), *relatives);
        for relative in relatives {
            assert!(
                !std::fs::read_to_string(home.join(relative))
                    .unwrap()
                    .contains(&format!("custom {profile}"))
            );
        }
        assert!(strings(&environment(home, profile, true)["changed"]).is_empty());
    }
}

#[test]
fn atomic_write_is_noop_when_unchanged_and_never_creates_sidecar_backups() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    environment(home, "bash", true);
    let before = inode(&home.join(".bashrc"));
    assert!(strings(&environment(home, "bash", true)["changed"]).is_empty());
    assert_eq!(inode(&home.join(".bashrc")), before);
    std::fs::write(home.join(".bashrc"), "custom\n").unwrap();
    assert_eq!(strings(&environment(home, "bash", true)["changed"]), [".bashrc"]);
    assert!(
        std::fs::read_dir(home).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".homestack-backup-"))
    );
}

#[test]
fn destination_symlink_ancestor_and_recursive_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::create_dir(home.join("real")).unwrap();
    std::os::unix::fs::symlink(home.join("real"), home.join("link")).unwrap();
    let paths =
        |items: Value| run_guest(&json!({"operation": "paths", "home": home.display().to_string(), "paths": items}));
    let refused = paths(json!([{"relative": "link/file"}]));
    assert!(refused["error"].as_str().unwrap().contains("Symlink"), "{refused}");
    std::os::unix::fs::symlink("/tmp", home.join("real/nested")).unwrap();
    let refused = paths(json!([{"relative": "real", "directory": true, "recursive": true}]));
    assert!(refused["error"].as_str().unwrap().contains("symlink"), "{refused}");
}

#[test]
fn zsh_profile_syntax_behavior() {
    if !have("zsh") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    environment(tmp.path(), "zsh", true);
    let output = Command::new("zsh")
        .args(["-lic", "print -r -- $PATH"])
        .env("ZDOTDIR", tmp.path())
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let local_bin = format!("{}/.local/bin", tmp.path().display());
    assert_eq!(String::from_utf8_lossy(&output.stdout).matches(&local_bin).count(), 1);
}

#[test]
fn fish_profile_syntax_behavior() {
    if !have("fish") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    environment(tmp.path(), "fish", true);
    let output = Command::new("fish")
        .args(["-lic", "string join : $PATH"])
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path().join(".config"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let local_bin = format!("{}/.local/bin", tmp.path().display());
    assert_eq!(String::from_utf8_lossy(&output.stdout).matches(&local_bin).count(), 1);
}

fn state(home: &Path, values: Value) -> Value {
    let mut payload = json!({"home": home.display().to_string(), "vmid": 200, "name": "workspace"});
    payload
        .as_object_mut()
        .unwrap()
        .extend(values.as_object().unwrap().clone());
    let answer = run_guest(&payload);
    assert_eq!(answer["ok"], json!(true), "{answer}");
    answer
}

fn registry(home: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(home.join(".local/state/homestack/setup.json")).unwrap()).unwrap()
}

#[test]
fn snapshot_is_central_and_registry_tracks_managed_files() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::write(home.join(".bashrc"), "export CUSTOM=1\n").unwrap();
    let snapshot = state(
        home,
        json!({"operation": "snapshot", "items": ["bash"], "paths": [".bashrc", ".profile"]}),
    );
    assert_eq!(snapshot["created"], json!(true));
    let root = home.join(".local/state/homestack");
    let saved = root.join(snapshot["id"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(saved.join("home/.bashrc")).unwrap(),
        "export CUSTOM=1\n"
    );
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(saved.join("snapshot.json")).unwrap()).unwrap();
    let file = |path: &str| {
        manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == json!(path))
            .unwrap()
            .clone()
    };
    assert_eq!(file(".bashrc")["exists"], json!(true));
    assert_eq!(file(".profile")["exists"], json!(false));
    assert!(file(".bashrc").get("sha256").is_some());
    let result = environment(home, "bash", true);
    assert!(!strings(&result["changed"]).is_empty());
    state(
        home,
        json!({"operation": "state-record", "id": "bash", "handler": "environment",
                       "paths": result["paths"], "snapshot": snapshot["id"]}),
    );
    let registry = registry(home);
    assert_eq!(registry["workspace"]["home_label"], json!("HS_HOME_200"));
    let bash = &registry["items"]["bash"];
    let mut recorded: Vec<String> = bash["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap().to_owned())
        .collect();
    recorded.sort();
    let mut expected = strings(&result["paths"]);
    expected.sort();
    assert_eq!(recorded, expected);
    assert!(
        bash["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["sha256"].is_string() && f["mtime_ns"].is_number())
    );
    assert_eq!(bash["last_snapshot"], snapshot["id"]);
}

#[test]
fn state_record_preserves_snapshot_for_noop_and_replaces_new_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::write(home.join(".bashrc"), "export CUSTOM=1\n").unwrap();
    let first = state(
        home,
        json!({"operation": "snapshot", "items": ["bash"], "paths": [".bashrc"]}),
    );
    assert_eq!(first["created"], json!(true));
    let applied = environment(home, "bash", true);
    state(
        home,
        json!({"operation": "state-record", "id": "bash", "handler": "environment",
                       "paths": applied["paths"], "snapshot": first["id"]}),
    );
    let no_op = environment(home, "bash", true);
    assert!(strings(&no_op["changed"]).is_empty());
    let preserved = state(
        home,
        json!({"operation": "state-record", "id": "bash", "handler": "environment",
                                        "paths": no_op["paths"], "snapshot": null}),
    )["item"]
        .clone();
    assert_eq!(preserved["last_snapshot"], first["id"]);
    std::fs::write(home.join(".bashrc"), "export CUSTOM=2\n").unwrap();
    let second = state(
        home,
        json!({"operation": "snapshot", "items": ["bash"], "paths": [".bashrc"]}),
    );
    assert_eq!(second["created"], json!(true));
    assert_ne!(second["id"], first["id"]);
    let replaced = state(
        home,
        json!({"operation": "state-record", "id": "bash", "handler": "environment",
                                       "paths": no_op["paths"], "snapshot": second["id"]}),
    )["item"]
        .clone();
    assert_eq!(replaced["last_snapshot"], second["id"]);
}

#[test]
fn application_registry_stores_dates_but_no_version_output() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let record = |installed: bool| {
        state(
            home,
            json!({"operation": "state-record", "id": "codex", "handler": "application",
                           "installed": installed, "paths": [], "snapshot": null}),
        )["item"]
            .clone()
    };
    let first = record(true);
    let second = record(false);
    assert_eq!(first["installed_at"], second["installed_at"]);
    assert!(second.get("first_managed_at").is_some());
    assert!(second.get("last_applied_at").is_some());
    assert!(second.get("version").is_none());
}

#[test]
fn registry_follows_same_persistent_home_after_workspace_rename() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    state(
        home,
        json!({"operation": "state-record", "name": "old-name", "id": "codex", "handler": "application",
                       "installed": true, "paths": [], "snapshot": null}),
    );
    let read = state(home, json!({"operation": "state-read", "name": "new-name"}));
    assert_eq!(read["registry"]["workspace"]["vmid"], json!(200));
    state(
        home,
        json!({"operation": "state-record", "name": "new-name", "id": "codex", "handler": "application",
                       "installed": false, "paths": [], "snapshot": null}),
    );
    assert_eq!(registry(home)["workspace"]["name"], json!("new-name"));
}

#[test]
fn snapshot_copies_previous_registry_only_when_overwrite_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    state(
        home,
        json!({"operation": "state-record", "id": "codex", "handler": "application",
                       "installed": true, "paths": [], "snapshot": null}),
    );
    let no_change = state(
        home,
        json!({"operation": "snapshot", "items": ["codex"], "paths": [".missing"]}),
    );
    assert_eq!(no_change["created"], json!(false));
    std::fs::create_dir(home.join(".config")).unwrap();
    std::fs::write(home.join(".config/app.conf"), "old").unwrap();
    let changed = state(
        home,
        json!({"operation": "snapshot", "items": ["codex"], "paths": [".config/app.conf"]}),
    );
    let snap = home
        .join(".local/state/homestack")
        .join(changed["id"].as_str().unwrap());
    assert!(snap.join("setup.before.json").is_file());
    assert_eq!(
        std::fs::read_to_string(snap.join("home/.config/app.conf")).unwrap(),
        "old"
    );
}

#[test]
fn setup_state_record_marks_alien_without_home_label() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    state(
        home,
        json!({"operation": "state-record", "id": "codex", "handler": "application",
                       "installed": true, "paths": [], "snapshot": null, "alien": true}),
    );
    let workspace = &registry(home)["workspace"];
    assert_eq!(workspace["kind"], json!("alien"));
    assert!(workspace.get("home_label").is_none(), "{workspace}");
}

/// The identity check runs against this desktop's own account, with fake
/// `hostname` and `findmnt` first on `PATH`; never as root.
struct Identity {
    _bin: tempfile::TempDir,
    path: String,
    payload: Value,
}

fn identity(hostname: &str, mount: &str, alien: bool) -> Option<Identity> {
    let uid = std::fs::metadata("/proc/self").ok()?.uid();
    let home = std::env::var("HOME").ok()?;
    let user = std::env::var("USER").ok()?;
    if uid == 0 || std::fs::metadata(&home).ok()?.uid() != uid {
        return None;
    }
    let bin = tempfile::tempdir().ok()?;
    for (name, output) in [("hostname", hostname), ("findmnt", mount)] {
        let path = bin.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' '{output}'\n")).ok()?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).ok()?;
    }
    let gid = std::fs::metadata("/proc/self").ok()?.gid();
    let path = format!("{}:/usr/bin:/bin", bin.path().display());
    Some(Identity {
        payload: json!({"operation": "identity", "home": home, "user": user, "uid": uid, "gid": gid,
                        "name": "alien-workspace", "vmid": 200, "alien": alien}),
        _bin: bin,
        path,
    })
}

#[test]
fn alien_identity_allows_home_on_root_filesystem() {
    let Some(check) = identity("alien-workspace", "/ ext4 root", true) else {
        return;
    };
    assert_eq!(run_guest_with_path(&check.payload, &check.path)["ok"], json!(true));
}

#[test]
fn alien_identity_still_rejects_account_and_hostname_mismatches() {
    let Some(check) = identity("alien-workspace", "/ ext4", true) else {
        return;
    };
    let mut payload = check.payload.clone();
    payload["uid"] = json!(payload["uid"].as_u64().unwrap() + 1);
    let refused = run_guest_with_path(&payload, &check.path);
    assert!(refused["error"].as_str().unwrap().contains("UID/GID"), "{refused}");
    let Some(check) = identity("wrong-name", "/ ext4", true) else {
        return;
    };
    let refused = run_guest_with_path(&check.payload, &check.path);
    assert!(refused["error"].as_str().unwrap().contains("hostname"), "{refused}");
}

#[test]
fn native_identity_still_rejects_root_filesystem_mount() {
    let Some(check) = identity("alien-workspace", "/ ext4", false) else {
        return;
    };
    let refused = run_guest_with_path(&check.payload, &check.path);
    assert!(refused["error"].as_str().unwrap().contains("mount"), "{refused}");
}

#[test]
fn actual_identity_guard_rejects_hostname_and_mount_mismatches() {
    let Some(check) = identity("different", "/ ext4 root", false) else {
        return;
    };
    let refused = run_guest_with_path(&check.payload, &check.path);
    assert!(refused["error"].as_str().unwrap().contains("hostname"), "{refused}");
    let Some(check) = identity("alien-workspace", "/ ext4 root", false) else {
        return;
    };
    let refused = run_guest_with_path(&check.payload, &check.path);
    assert!(refused["error"].as_str().unwrap().contains("mount"), "{refused}");
}
