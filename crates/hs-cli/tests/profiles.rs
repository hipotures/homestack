//! Profiles must not borrow another environment's configuration or SSH state.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use hs_cli::{local_ssh, profile};
use hs_config::client::WorkspaceSsh;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_homestack"));
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env_remove("HOMESTACK_ACTIVE_PROFILE");
    command
}

#[test]
fn selectors_work_before_and_after_commands_and_reject_unsafe_names() -> Result {
    for words in [
        vec!["--profil", "p2p", "status"],
        vec!["status", "--profile=p2p"],
        vec!["--profile", "p2p", "status"],
    ] {
        let argv = words.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(profile::selection(&argv)?, ("p2p".into(), vec!["status".into()]));
    }
    assert_eq!(profile::selection(&[])?, ("default".into(), vec![]));
    for words in [
        vec!["--profil"],
        vec!["--profil", "../default"],
        vec!["--profile="],
        vec!["--profile", "UPPER"],
        vec!["--profile", "e2e", "--profil", "default"],
        vec!["--profil", "p2p", "--config", "/production/client.toml"],
    ] {
        assert!(
            profile::selection(&words.iter().map(|s| s.to_string()).collect::<Vec<_>>()).is_err(),
            "accepted {words:?}"
        );
    }
    Ok(())
}

#[test]
fn default_and_named_profiles_have_separate_configurations_and_no_fallback() -> Result {
    let home = tempfile::tempdir()?;
    let production = home.path().join(".config/homestack");
    std::fs::create_dir_all(&production)?;
    std::fs::write(
        production.join("client.toml"),
        "production sentinel; must never be read by p2p",
    )?;
    for args in [vec!["--help"], vec!["--profil", "default", "--help"]] {
        let output = command(home.path()).args(args).output()?;
        assert!(output.status.success());
        assert!(
            String::from_utf8(output.stdout)?.contains(&format!("Configuration: {}/client.toml", production.display()))
        );
    }
    let named = production.join("profiles/p2p");
    let output = command(home.path()).args(["--profil", "p2p", "--help"]).output()?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8(output.stdout)?.contains(&format!(
        "Configuration: {}/config/homestack/client.toml",
        named.display()
    )));
    assert_eq!(std::fs::metadata(&named)?.permissions().mode() & 0o777, 0o700);
    let output = command(home.path())
        .args(["status", "--profil", "p2p", "--json"])
        .output()?;
    assert!(!output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        json["error"]
            .as_str()
            .is_some_and(|s| s.contains("profiles/p2p/config/homestack/client.toml") && s.contains("not found"))
    );
    assert_eq!(
        std::fs::read_to_string(production.join("client.toml"))?,
        "production sentinel; must never be read by p2p"
    );
    let output = command(home.path()).arg("profiles").output()?;
    let text = String::from_utf8(output.stdout)?;
    assert!(output.status.success());
    assert!(text.contains("default\t") && text.contains("p2p\t"));
    Ok(())
}

#[test]
fn missing_e2e_is_an_error_even_when_default_is_configured() -> Result {
    let home = tempfile::tempdir()?;
    let default_config = home.path().join(".config/homestack/client.toml");
    std::fs::create_dir_all(default_config.parent().ok_or("missing configuration directory")?)?;
    std::fs::write(&default_config, "default profile sentinel")?;
    let output = command(home.path()).args(["--profil", "e2e", "status"]).output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("Cannot open profile e2e"));
    assert!(output.stdout.is_empty());
    assert_eq!(std::fs::read_to_string(default_config)?, "default profile sentinel");
    Ok(())
}

#[test]
fn ssh_keys_known_hosts_and_cleanup_are_scoped_to_each_profile() -> Result {
    let storage = tempfile::Builder::new().prefix("profile space ").tempdir()?;
    let settings = WorkspaceSsh {
        user: "lab".into(),
        identity_files: vec!["~/.ssh/lab-key".into()],
        identities_only: true,
        log_level: "ERROR".into(),
        fido_devices: vec![],
    };
    for name in ["p2p", "test"] {
        let root = storage.path().join(name);
        let mut child = profile::prepare(&root, Path::new("/usr/bin/env"), Path::new("/usr/bin/ssh"), &[], name)?;
        for key in ["SSH_AUTH_SOCK", "SSH_AGENT_PID", "HOMESTACK_ASKPASS_SOCKET"] {
            assert!(child.get_envs().any(|(name, value)| name == key && value.is_none()));
        }
        let output = child.output()?;
        let env = String::from_utf8(output.stdout)?;
        assert!(!env.contains("SSH_AUTH_SOCK=") && !env.contains("SSH_AGENT_PID="));
        assert!(env.contains(&format!("XDG_STATE_HOME={}/state", root.display())));
        let home = root.join("home");
        local_ssh::write_local_ssh_config(&home, &settings, 200, "workspace", "192.0.2.200")?;
        let output = Command::new(root.join("bin/ssh")).args(["-G", "workspace"]).output()?;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let ssh = String::from_utf8(output.stdout)?;
        for expected in [
            format!("identityfile {}/.ssh/lab-key", home.display()),
            format!(
                "userknownhostsfile {}/.ssh/config.d/homestack/known_hosts/vm200",
                home.display()
            ),
            "identityagent none".into(),
            "globalknownhostsfile /dev/null".into(),
        ] {
            assert!(ssh.lines().any(|line| line == expected), "missing {expected}: {ssh}");
        }
        assert!(!ssh.contains("identityfile ~/.ssh/id_"));
        if hs_config::profiles::desktop_herdr_wrapper().is_some() {
            let wrapper = std::fs::read_to_string(root.join("bin/herdr"))?;
            assert!(!wrapper.contains("Herdr is disabled"));
            assert!(wrapper.contains("export HOME=") && wrapper.contains("export XDG_CONFIG_HOME="));
        }
        std::fs::write(local_ssh::known_hosts_file(&home, 200), name)?;
    }
    let p2p = storage.path().join("p2p/home");
    local_ssh::remove_local_ssh_config(&p2p, 200)?;
    local_ssh::forget_known_hosts(&p2p, 200)?;
    assert!(local_ssh::has_config(&storage.path().join("test/home"), 200));
    assert_eq!(
        std::fs::read_to_string(local_ssh::known_hosts_file(&storage.path().join("test/home"), 200))?,
        "test"
    );
    Ok(())
}
