//! Run the real TUI with a disposable desktop profile and the lab's SSH key.

use std::io::IsTerminal;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::{Result, phases::shell_quote};

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableMouseCapture,
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

fn ssh_quote(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.display().to_string().replace('\\', "\\\\").replace('"', "\\\"")
    )
}

fn write(path: &Path, text: &str, mode: u32) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(mode)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

fn prepare(dir: &Path, program: &Path, ssh: &Path, image: &str) -> Result<Command> {
    let root = dir.join("client");
    for name in ["home", "config", "state", "cache", "bin"] {
        let path = root.join(name);
        std::fs::create_dir_all(&path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    // OpenSSH reads the passwd database's home, not $HOME. An explicit -F
    // also prevents reading the real desktop's SSH config and Include files.
    let config = root.join("ssh_config");
    write(
        &config,
        &format!(
            "Host pve-e2e\n  ProxyCommand none\nInclude {}\nHost *\n  ProxyCommand /bin/false\n",
            ssh_quote(&dir.join("ssh_config"))
        ),
        0o600,
    )?;
    write(
        &root.join("bin/ssh"),
        &format!(
            "#!/bin/sh\nexec {} -F {} \"$@\"\n",
            shell_quote(&ssh.display().to_string()),
            shell_quote(&config.display().to_string())
        ),
        0o700,
    )?;
    // A production Herdr root shell must not be offered as a second route.
    write(
        &root.join("bin/herdr"),
        "#!/bin/sh\nprintf '%s\\n' 'Herdr is disabled in the E2E client; use SSH root@pve-e2e.' >&2\nexit 1\n",
        0o700,
    )?;
    let mut paths = vec![root.join("bin")];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    let mut command = Command::new(program);
    command.arg("tui");
    if !root.join("config/homestack/client.toml").is_file() {
        command.args(["--ssh", "root@pve-e2e", "--server-domain", crate::domain::SEARCH]);
    }
    command
        .args(["--server-image", image])
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("PATH", std::env::join_paths(paths)?)
        .env_remove("SSH_AUTH_SOCK")
        .env_remove("SSH_AGENT_PID")
        .env_remove("HOMESTACK_ASKPASS_SOCKET");
    Ok(command)
}

fn executable(name: &str) -> Result<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH is not set")?)
        .map(|dir| dir.join(name))
        .find(|path| {
            path.metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
        .ok_or_else(|| format!("missing host tool {name}").into())
}

pub async fn launch(artifacts: &Path) -> Result<()> {
    let report: Value = serde_json::from_slice(&std::fs::read(artifacts.join("report.json"))?)?;
    if report["status"] != "paused" {
        return Err("the selected run is not paused; start a VM with --pause-after verify".into());
    }
    let dir = PathBuf::from(
        report["temporary_directory"]
            .as_str()
            .ok_or("run has no temporary directory")?,
    )
    .canonicalize()?;
    if !dir.join("ssh_config").is_file() || !dir.join("identity").is_file() {
        return Err("the selected run has no VM SSH credentials yet".into());
    }
    // An old report alone is not sufficient. Closing this connection without
    // sending 'R' leaves the controller paused.
    drop(UnixStream::connect(dir.join("inspection.sock"))?);
    let program = std::env::current_exe()?.with_file_name("homestack");
    if !program
        .metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    {
        return Err("build the client first: cargo build --locked -p hs-cli -p hs-e2e".into());
    }
    let metadata: Value = serde_json::from_slice(&std::fs::read(dir.join("service.json")).map_err(
        |_| "HomeStack install inputs are missing; run homestack-e2e prepare --artifacts <this run> first",
    )?)?;
    if metadata["run_id"] != report["run_id"] {
        return Err("service settings belong to another run".into());
    }
    let image = metadata["image"]
        .as_str()
        .ok_or("service settings have no server image")?;
    let mut command = prepare(&dir, &program, &executable("ssh")?, image)?;
    let fixture = Path::new(
        metadata["fixture_directory"]
            .as_str()
            .ok_or("service settings have no fixture directory")?,
    );
    let fixture_lock = std::fs::File::options()
        .read(true)
        .write(true)
        .open(fixture.join("fixture.lock"))?;
    fixture_lock
        .try_lock()
        .map_err(|_| "the HomeStack E2E API fixture is in use by another client")?;
    let registry = crate::registry_proxy::start().await?;
    let mut tunnel = crate::tunnel::Tunnel::start(&dir, registry.port).await?;
    let proxy = format!("socks5h://127.0.0.1:{}", tunnel.port);
    for key in [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ] {
        command.env(key, &proxy);
    }
    for key in ["NO_PROXY", "no_proxy"] {
        command.env(key, "ghcr.io,pkg-containers.githubusercontent.com");
    }
    eprintln!("E2E client profile: {}", dir.join("client").display());
    eprintln!("Production client configuration and SSH agent are excluded.");
    eprintln!(
        "Installer defaults: SSH root@pve-e2e; server domain {}. Herdr is disabled.",
        crate::domain::SEARCH
    );
    eprintln!("This profile and its credentials are removed with the VM. Keep the controller paused.");
    eprintln!("HTTPS uses the lab SSH SOCKS tunnel; TLS hostname and public-CA checks stay enabled.");
    let _terminal = if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        crossterm::terminal::enable_raw_mode()?;
        Some(TerminalGuard)
    } else {
        None
    };
    let mut child = tokio::process::Command::from(command).kill_on_drop(true).spawn()?;
    let status = tokio::select! {
        status = child.wait() => status,
        _ = tunnel.wait() => {
            child.start_kill()?;
            child.wait().await?;
            return Err("the lab SSH tunnel closed; the E2E client has stopped".into());
        }
    };
    tunnel.stop().await?;
    if status?.success() {
        Ok(())
    } else {
        Err("the isolated HomeStack client exited unsuccessfully".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Result<tempfile::TempDir> {
        let dir = tempfile::tempdir()?;
        write(
            &dir.path().join("ssh_config"),
            "Host pve-e2e\n  HostName 127.0.0.1\n  User root\n  Port 42345\n  IdentityFile /test/lab-key\n  IdentityAgent none\n  UserKnownHostsFile /test/lab-hosts\n  IdentitiesOnly yes\n  StrictHostKeyChecking yes\n",
            0o600,
        )?;
        Ok(dir)
    }

    #[test]
    fn client_uses_an_empty_profile_without_inherited_credentials() -> Result<()> {
        let dir = fixture()?;
        let program = dir.path().join("client-program");
        write(
            &program,
            "#!/bin/sh\nprintf '%s\\n' \"$HOME\" \"$XDG_CONFIG_HOME\" \"$XDG_STATE_HOME\" \"$XDG_CACHE_HOME\"\ntest -z \"${SSH_AUTH_SOCK-}\"\ntest ! -e \"$XDG_CONFIG_HOME/homestack/client.toml\"\n",
            0o700,
        )?;
        let mut command = prepare(
            dir.path(),
            &program,
            Path::new("/usr/bin/ssh"),
            "ghcr.io/test/server:test",
        )?;
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "tui",
                "--ssh",
                "root@pve-e2e",
                "--server-domain",
                "e2e.homestack.app",
                "--server-image",
                "ghcr.io/test/server:test"
            ]
        );
        let output = command.output()?;
        assert!(output.status.success());
        let expected = ["home", "config", "state", "cache"]
            .map(|name| format!("{}\n", dir.path().join("client").join(name).display()))
            .concat();
        assert_eq!(String::from_utf8(output.stdout)?, expected);
        for name in ["SSH_AUTH_SOCK", "SSH_AGENT_PID", "HOMESTACK_ASKPASS_SOCKET"] {
            assert!(command.get_envs().any(|(key, value)| key == name && value.is_none()));
        }
        assert_eq!(
            std::fs::metadata(dir.path().join("client"))?.permissions().mode() & 0o777,
            0o700
        );
        // Reopening the same run retains only its own client configuration.
        let client_config = dir.path().join("client/config/homestack");
        std::fs::create_dir(&client_config)?;
        std::fs::write(client_config.join("client.toml"), "lab-only")?;
        let reopened = prepare(
            dir.path(),
            Path::new("/bin/true"),
            Path::new("/usr/bin/ssh"),
            "ghcr.io/test/server:test",
        )?;
        assert_eq!(
            reopened.get_args().collect::<Vec<_>>(),
            ["tui", "--server-image", "ghcr.io/test/server:test"]
        );
        assert_eq!(std::fs::read_to_string(client_config.join("client.toml"))?, "lab-only");
        let root = dir.path().to_owned();
        dir.close()?;
        assert!(!root.exists());
        Ok(())
    }

    #[test]
    fn ssh_uses_the_lab_config_and_herdr_cannot_reach_production() -> Result<()> {
        let dir = fixture()?;
        let ssh = executable("ssh")?;
        prepare(dir.path(), Path::new("/bin/true"), &ssh, "ghcr.io/test/server:test")?;
        for target in ["root@pve-e2e", "root@production.invalid"] {
            let output = Command::new(dir.path().join("client/bin/ssh"))
                .args(["-G", target])
                .output()?;
            assert!(output.status.success());
            let config = String::from_utf8(output.stdout)?;
            if target == "root@pve-e2e" {
                // OpenSSH omits ProxyCommand from -G output for 'none'.
                assert!(
                    !config.lines().any(|line| line.starts_with("proxycommand ")),
                    "{config}"
                );
                for expected in [
                    "hostname 127.0.0.1",
                    "user root",
                    "port 42345",
                    "identityfile /test/lab-key",
                    "identityagent none",
                    "stricthostkeychecking true",
                    "userknownhostsfile /test/lab-hosts",
                ] {
                    assert!(
                        config.lines().any(|line| line == expected),
                        "missing {expected}: {config}"
                    );
                }
            } else {
                assert!(config.lines().any(|line| line == "proxycommand /bin/false"), "{config}");
            }
        }
        let output = Command::new(dir.path().join("client/bin/herdr"))
            .args(["workspace", "list"])
            .output()?;
        assert!(!output.status.success());
        assert!(String::from_utf8(output.stderr)?.contains("Herdr is disabled"));
        Ok(())
    }

    #[tokio::test]
    async fn stale_runs_never_start_a_client_or_use_the_desktop_profile() -> Result<()> {
        let artifacts = tempfile::tempdir()?;
        let dir = fixture()?;
        let mut report = serde_json::json!({"status": "passed", "temporary_directory": dir.path()});
        std::fs::write(artifacts.path().join("report.json"), serde_json::to_vec(&report)?)?;
        assert!(
            launch(artifacts.path())
                .await
                .is_err_and(|error| error.to_string().contains("not paused"))
        );
        report["status"] = serde_json::json!("paused");
        std::fs::write(artifacts.path().join("report.json"), serde_json::to_vec(&report)?)?;
        assert!(
            launch(artifacts.path())
                .await
                .is_err_and(|error| error.to_string().contains("no VM SSH credentials"))
        );
        std::fs::write(dir.path().join("identity"), "unused")?;
        assert!(launch(artifacts.path()).await.is_err());
        assert!(!dir.path().join("client").exists());
        Ok(())
    }
}
