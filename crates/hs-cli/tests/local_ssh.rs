//! Ports of `python/tests/test_workspace_ssh.py` and
//! `test_addkey.py::LocalTests`: the desktop's SSH config of a workspace,
//! `known_hosts` and `identity_files` (I1).

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use common::{FakeSystem, TestResult, strings};
use hs_cli::identity;
use hs_cli::local_ssh;
use hs_cli::system::Output;
use hs_config::client::WorkspaceSsh;

fn settings(files: &[&str]) -> WorkspaceSsh {
    WorkspaceSsh {
        user: "user".to_owned(),
        identity_files: strings(files),
        identities_only: true,
        log_level: "FATAL".to_owned(),
        fido_devices: strings(&["yubikey", "trezor"]),
    }
}

fn example() -> WorkspaceSsh {
    WorkspaceSsh {
        fido_devices: Vec::new(),
        ..settings(&["~/.ssh/example-hardware-key"])
    }
}

fn config_dir(home: &Path) -> std::path::PathBuf {
    home.join(".ssh/config.d/homestack")
}

/// Port of `LocalSSHConfigTests::test_writes_exact_config_for_vm_200_and_vm_201_with_mode_0600`.
#[test]
fn writes_exact_config_for_vm_200_and_vm_201_with_mode_0600() -> TestResult {
    for (vmid, name, ip) in [(200, "test1", "192.0.2.200"), (201, "workspace-two", "192.0.2.201")] {
        let home = tempfile::tempdir()?;
        let path = local_ssh::write_local_ssh_config(home.path(), &example(), vmid, name, ip)?;
        assert_eq!(path, config_dir(home.path()).join(format!("vm{vmid}-{name}.conf")));
        assert_eq!(
            std::fs::read_to_string(&path)?,
            format!(
                "Host {name} vm{vmid}\n    HostName {ip}\n    User user\n    IdentityFile ~/.ssh/example-hardware-key\n    IdentitiesOnly yes\n    LogLevel FATAL\n"
            )
        );
        assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
    }
    Ok(())
}

/// Port of `LocalSSHConfigTests::test_uses_workspace_ssh_config_instead_of_hardcoded_keys`.
#[test]
fn uses_workspace_ssh_config_instead_of_hardcoded_keys() -> TestResult {
    let home = tempfile::tempdir()?;
    let custom = WorkspaceSsh {
        user: "user".to_owned(),
        identity_files: strings(&["~/.ssh/custom-key-a", "/keys/custom-key-b"]),
        identities_only: false,
        log_level: "ERROR".to_owned(),
        fido_devices: Vec::new(),
    };
    let path = local_ssh::write_local_ssh_config(home.path(), &custom, 202, "custom", "192.0.2.202")?;
    assert_eq!(
        std::fs::read_to_string(path)?,
        "Host custom vm202\n    HostName 192.0.2.202\n    User user\n    IdentityFile ~/.ssh/custom-key-a\n    IdentityFile /keys/custom-key-b\n    IdentitiesOnly no\n    LogLevel ERROR\n"
    );
    Ok(())
}

/// Port of `LocalSSHConfigTests::test_replaces_only_stale_config_for_same_vmid`.
#[test]
fn replaces_only_stale_config_for_same_vmid() -> TestResult {
    let home = tempfile::tempdir()?;
    let dir = config_dir(home.path());
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("vm200-old-name.conf"), "old")?;
    std::fs::write(dir.join("vm201-keep.conf"), "keep")?;
    let created = local_ssh::write_local_ssh_config(home.path(), &example(), 200, "new-name", "192.0.2.200")?;
    assert!(!dir.join("vm200-old-name.conf").exists());
    assert!(created.exists());
    assert_eq!(std::fs::read_to_string(dir.join("vm201-keep.conf"))?, "keep");
    Ok(())
}

/// Port of `LocalSSHConfigTests::test_atomic_replace_failure_keeps_previous_entry`.
#[test]
fn atomic_replace_failure_keeps_previous_entry() -> TestResult {
    let home = tempfile::tempdir()?;
    let dir = config_dir(home.path());
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("vm200-old-name.conf"), "old")?;
    let error =
        local_ssh::write_local_ssh_config_with(home.path(), &example(), 200, "new-name", "192.0.2.200", |_, _| {
            Err(std::io::Error::other("replace failed"))
        })
        .err()
        .map(|e| e.to_string());
    assert!(error.is_some_and(|e| e.contains("replace failed")));
    assert!(dir.join("vm200-old-name.conf").exists());
    assert!(!dir.join("vm200-new-name.conf").exists());
    for entry in std::fs::read_dir(&dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        assert!(!name.starts_with(".vm200-new-name.conf."), "{name}");
    }
    Ok(())
}

/// Port of `LocalSSHConfigTests::test_remove_local_ssh_config_removes_only_matching_vmid`.
#[test]
fn remove_local_ssh_config_removes_only_matching_vmid() -> TestResult {
    let home = tempfile::tempdir()?;
    let dir = config_dir(home.path());
    std::fs::create_dir_all(&dir)?;
    let current = dir.join("vm210-test210.conf");
    let stale = dir.join("vm210-oldname.conf");
    let other = dir.join("vm201-keep.conf");
    let main = home.path().join(".ssh/config");
    for path in [&current, &stale, &other, &main] {
        std::fs::write(path, "x")?;
    }
    let removed = local_ssh::remove_local_ssh_config(home.path(), 210)?;
    assert_eq!(removed, [stale.clone(), current.clone()]);
    assert!(!current.exists() && !stale.exists());
    assert!(other.exists() && main.exists());
    Ok(())
}

/// Port of `KnownHostsTests::test_forget_local_ssh_host_removes_plain_and_bracketed_entries`.
#[test]
fn forget_local_ssh_host_removes_plain_and_bracketed_entries() -> TestResult {
    let home = tempfile::tempdir()?;
    let system = FakeSystem::new().run(|argv| {
        Ok(if argv.get(1).is_some_and(|a| a == "-F") {
            Output::stdout("# found\n")
        } else {
            Output::default()
        })
    });
    let removed = local_ssh::forget_local_ssh_host(&system, home.path(), "192.0.2.200")?;
    assert_eq!(removed, ["192.0.2.200", "[192.0.2.200]:22"]);
    let known = home.path().join(".ssh/known_hosts").display().to_string();
    let runs = system.runs();
    for target in ["192.0.2.200", "[192.0.2.200]:22"] {
        assert!(
            runs.contains(&strings(&["ssh-keygen", "-f", &known, "-R", target])),
            "{runs:?}"
        );
    }
    Ok(())
}

/// Port of `KnownHostsTests::test_forget_local_ssh_host_is_noop_when_not_found`.
#[test]
fn forget_local_ssh_host_is_noop_when_not_found() -> TestResult {
    let home = tempfile::tempdir()?;
    let system = FakeSystem::new().run(|_| Ok(Output::failed(1, "")));
    assert!(local_ssh::forget_local_ssh_host(&system, home.path(), "192.0.2.200")?.is_empty());
    assert!(!system.runs().iter().any(|argv| argv.iter().any(|a| a == "-R")));
    let missing = FakeSystem::new().which(|_| None);
    assert!(local_ssh::forget_local_ssh_host(&missing, home.path(), "192.0.2.200").is_err());
    Ok(())
}

/// Port of `LocalTests::test_private_key_must_exist_next_to_the_public_one`.
#[test]
fn private_key_must_exist_next_to_the_public_one() -> TestResult {
    let home = tempfile::tempdir()?;
    let public = home.path().join("id_x.pub");
    std::fs::write(&public, "ecdsa-sk-key AAAA trezor")?;
    let error = identity::private_key_for(home.path(), &public)
        .err()
        .map(|e| e.to_string());
    assert!(error.is_some_and(|e| e.contains("was not found")));
    std::fs::write(home.path().join("id_x"), "k")?;
    let (private, shown) = identity::private_key_for(home.path(), &public)?;
    assert_eq!(private, home.path().join("id_x"));
    assert_eq!(shown, "~/id_x");
    Ok(())
}

/// Port of `LocalTests::test_identity_is_appended_once_and_the_rest_of_the_config_is_untouched`.
#[test]
fn identity_is_appended_once_and_the_rest_of_the_config_is_untouched() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("client.toml");
    let text = "# my notes\n[node]\nname = \"n\"\n\n[workspace_ssh]\nuser = \"user\"\nidentity_files = [\"~/.ssh/a\", \"~/.ssh/b\"]\nlog_level = \"FATAL\"\n\n[repo]\nowner = \"o\"\n";
    std::fs::write(&path, text)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
    assert!(identity::register_identity(dir.path(), &path, "~/.ssh/c")?);
    assert_eq!(
        std::fs::read_to_string(&path)?,
        text.replace(
            "[\"~/.ssh/a\", \"~/.ssh/b\"]",
            "[\"~/.ssh/a\", \"~/.ssh/b\", \"~/.ssh/c\"]"
        )
    );
    assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o644);
    assert!(!identity::register_identity(dir.path(), &path, "~/.ssh/c")?);
    assert!(!identity::register_identity(dir.path(), &path, "~/.ssh/a")?);
    Ok(())
}

/// Port of `LocalTests::test_identity_in_another_section_or_multiline_array_is_not_touched`.
#[test]
fn identity_in_another_section_or_multiline_array_is_not_touched() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("client.toml");
    for text in [
        "[workspace_ssh]\nuser = \"u\"\n[other]\nidentity_files = [\"x\"]\n",
        "[workspace_ssh]\nidentity_files = [\n  \"x\",\n]\n",
        "[node]\nname = \"n\"\n",
    ] {
        std::fs::write(&path, text)?;
        let error = identity::register_identity(dir.path(), &path, "~/.ssh/c")
            .err()
            .map(|e| e.to_string());
        assert!(error.is_some_and(|e| e.contains("one-line array")), "{text:?}");
        assert_eq!(std::fs::read_to_string(&path)?, text);
    }
    Ok(())
}

/// Port of `LocalTests::test_ssh_config_lists_plain_keys_unchanged_and_picks_by_connected_key_otherwise`.
#[test]
fn ssh_config_lists_plain_keys_unchanged_and_picks_by_connected_key_otherwise() {
    let plain = local_ssh::local_ssh_lines(&settings(&["~/.ssh/k1", "~/.ssh/k2"]), 5, "n", "192.0.2.5");
    assert_eq!(
        plain,
        strings(&[
            "Host n vm5",
            "    HostName 192.0.2.5",
            "    User user",
            "    IdentityFile ~/.ssh/k1",
            "    IdentityFile ~/.ssh/k2",
            "    IdentitiesOnly yes",
            "    LogLevel FATAL"
        ])
    );
    let mixed = local_ssh::local_ssh_lines(
        &settings(&[
            "~/.ssh/id_ed25519_sk_yubikey1",
            "~/.ssh/id_ed25519_sk_yubikey2",
            "~/.ssh/id_ecdsa_sk_trezor_vm",
        ]),
        200,
        "test1",
        "192.0.2.200",
    );
    let text = mixed.join("\n");
    assert!(
        text.contains("Match originalhost test1,vm200 exec \"fido2-token -L 2>/dev/null | grep -qE 'vendor=0x1050'\"")
    );
    assert!(
        text.contains(
            "Match originalhost test1,vm200 exec \"! fido2-token -L 2>/dev/null | grep -qE 'vendor=0x1050'\""
        )
    );
    assert!(text.find("yubikey1") < text.find("id_ecdsa_sk_trezor_vm"));
    assert_eq!(text.matches("IdentityFile").count(), 3);
}

/// Port of `LocalTests::test_sync_updates_the_config_and_writes_this_vms_ssh_file`.
#[test]
fn sync_updates_the_config_and_writes_this_vms_ssh_file() -> TestResult {
    let home = tempfile::tempdir()?;
    std::fs::create_dir(home.path().join(".config"))?;
    let path = home.path().join(".config/client.toml");
    std::fs::write(
        &path,
        "[workspace_ssh]\nidentity_files = [\"~/.ssh/id_ed25519_sk_yubikey1\"]\n",
    )?;
    let result = identity::sync_local_ssh(
        home.path(),
        &path,
        &settings(&["~/.ssh/id_ed25519_sk_yubikey1"]),
        "~/.ssh/id_ecdsa_sk_trezor_vm",
        206,
        "cpu",
        "192.0.2.206",
    )?;
    assert_eq!(result["identity_files_changed"], true);
    assert!(std::fs::read_to_string(&path)?.contains("id_ecdsa_sk_trezor_vm"));
    let written = std::fs::read_to_string(config_dir(home.path()).join("vm206-cpu.conf"))?;
    assert!(written.contains("IdentityFile ~/.ssh/id_ecdsa_sk_trezor_vm"));
    assert!(written.contains("Match originalhost cpu,vm206"));
    Ok(())
}

/// Port of `DiscoveryTests::test_hardware_identity_discovery_accepts_suffix_after_sk`
/// (`test_discovery.py`).
#[test]
fn hardware_identity_discovery_accepts_suffix_after_sk() -> TestResult {
    let home = tempfile::tempdir()?;
    let ssh = home.path().join(".ssh");
    std::fs::create_dir(&ssh)?;
    std::fs::write(ssh.join("id_ed25519_sk_yubikey1"), "stub")?;
    std::fs::write(ssh.join("id_ecdsa_sk_trezor"), "stub")?;
    std::fs::write(ssh.join("id_ed25519_sk_yubikey1.pub"), "public")?;
    std::fs::write(ssh.join("id_ed25519"), "software")?;
    assert_eq!(
        local_ssh::discover_hardware_identities(home.path()),
        ["~/.ssh/id_ecdsa_sk_trezor", "~/.ssh/id_ed25519_sk_yubikey1"]
    );
    Ok(())
}
