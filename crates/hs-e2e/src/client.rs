//! Run the real TUI with a disposable desktop profile and the lab's SSH key.

use std::io::IsTerminal;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::{Result, phases::shell_quote};

struct TerminalGuard {
    settings: rustix::termios::Termios,
}

impl TerminalGuard {
    fn capture() -> Result<Option<Self>> {
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            Ok(Some(Self {
                settings: rustix::termios::tcgetattr(std::io::stdin())?,
            }))
        } else {
            Ok(None)
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = rustix::termios::tcsetattr(std::io::stdin(), rustix::termios::OptionalActions::Now, &self.settings);
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

fn prepare(dir: &Path, program: &Path, ssh: &Path, image: &str, argv: &[String]) -> Result<Command> {
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
    let guests = root.join("home/.ssh/config.d/homestack");
    std::fs::create_dir_all(&guests)?;
    write(&root.join("guest-connect.py"), include_str!("guest-connect.py"), 0o600)?;
    write(&root.join("guest-ping.py"), include_str!("guest-ping.py"), 0o600)?;
    write(
        &config,
        &format!(
            "Host pve-e2e pve-e2e-pve* pve[123] pve[123].e2e.homestack.app pve.e2e.homestack.app 10.77.0.[123]\n  ProxyCommand none\nHost *\nInclude {}\nHost *\nInclude {}\nHost 10.77.0.*\n  IdentityFile {}\nHost *\n  ProxyCommand python3 {} {} {} %h %p\n  IdentityAgent none\n  IdentitiesOnly yes\n  StrictHostKeyChecking accept-new\n  UserKnownHostsFile {}\n  GlobalKnownHostsFile /dev/null\n  ConnectTimeout 10\n",
            ssh_quote(&guests.join("*.conf")),
            ssh_quote(&dir.join("ssh_config")),
            ssh_quote(&root.join("home/.ssh/e2e-workspace")),
            ssh_quote(&root.join("guest-connect.py")),
            ssh_quote(dir),
            ssh_quote(ssh),
            ssh_quote(&root.join("known_hosts")),
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
    write(
        &root.join("bin/ping"),
        &format!(
            "#!/bin/sh\nexec python3 {} {} {} \"$@\"\n",
            shell_quote(&root.join("guest-ping.py").display().to_string()),
            shell_quote(&dir.display().to_string()),
            shell_quote(&ssh.display().to_string()),
        ),
        0o700,
    )?;
    if let Some(wrapper) = hs_config::profiles::desktop_herdr_wrapper() {
        write(&root.join("bin/herdr"), &wrapper, 0o700)?;
    } else {
        match std::fs::remove_file(root.join("bin/herdr")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let mut paths = vec![root.join("bin")];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    let mut command = Command::new(program);
    if argv.iter().any(|word| matches!(word.as_str(), "--help" | "-h")) {
        command.args(argv);
    } else if argv.is_empty() || argv.first().is_some_and(|word| word == "tui") {
        command.arg("tui").args(argv.iter().skip(1));
        if !root.join("config/homestack/client.toml").is_file() {
            if !argv.iter().any(|word| word == "--ssh") {
                command.args(["--ssh", "root@pve-e2e"]);
            }
            if !argv.iter().any(|word| word == "--server-domain") {
                command.args(["--server-domain", crate::domain::SEARCH]);
            }
        }
        if !argv.iter().any(|word| word == "--server-image") {
            command.args(["--server-image", image]);
        }
    } else {
        command.args(argv);
    }
    command
        .env("HOMESTACK_ACTIVE_PROFILE", "e2e")
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

/// The same isolated profile and SSH routing as inspection, without a pause.
pub fn automation(dir: &Path, program: &str, argv: &[String]) -> Result<tokio::process::Command> {
    let metadata: Value = serde_json::from_slice(&std::fs::read(dir.join("service.json"))?)?;
    let executable = std::env::current_exe()?.with_file_name(program);
    if !executable.is_file() {
        return Err(format!(
            "build the required executable {} through scripts/cargo.py",
            executable.display()
        )
        .into());
    }
    let command = prepare(
        dir,
        &executable,
        &self::executable("ssh")?,
        metadata["image"].as_str().ok_or("missing image")?,
        argv,
    )?;
    let mut command = tokio::process::Command::from(command);
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("HOMESTACK_")
            || name.starts_with("PVE_")
            || name.starts_with("GH_")
            || name.starts_with("GITHUB_")
        {
            command.env_remove(key);
        }
    }
    command.env("HOMESTACK_ACTIVE_PROFILE", "e2e");
    command.kill_on_drop(true);
    Ok(command)
}

fn paused_run(artifacts: &Path) -> Result<Value> {
    let bytes = std::fs::read(artifacts.join("report.json")).map_err(|error| {
        format!(
            "Cannot open E2E run at {}: {error}. Start a new lab with homestack-e2e run --pause-after verify; the profile is registered automatically after preparation",
            artifacts.display()
        )
    })?;
    let report: Value = serde_json::from_slice(&bytes)?;
    if report["status"] != "paused" {
        return Err("the selected run is not paused; start a VM with --pause-after verify".into());
    }
    Ok(report)
}

/// Bind the human-readable name to a live run without copying its secrets.
pub fn register(artifacts: &Path) -> Result<()> {
    register_at(&hs_config::profiles::environment_base()?, artifacts)
}

/// Remove only this run's reference when its temporary VM has been removed.
pub fn unregister(run_id: &str) -> Result<()> {
    unregister_at(&hs_config::profiles::environment_base()?, run_id)
}

fn unregister_at(base: &Path, run_id: &str) -> Result<()> {
    let root = hs_config::profiles::directory(base, "e2e")?;
    if !root.join("run.toml").exists() {
        return Ok(());
    }
    if hs_config::profiles::E2eRun::read(&root)?.run_id == run_id {
        std::fs::remove_file(root.join("run.toml"))?;
    }
    Ok(())
}

fn register_at(base: &Path, artifacts: &Path) -> Result<()> {
    use hs_config::profiles::{self, E2eRun};
    let artifacts = artifacts.canonicalize()?;
    let report = paused_run(&artifacts)?;
    let run_id = uuid::Uuid::parse_str(report["run_id"].as_str().ok_or("run has no ID")?)?.to_string();
    let dir = Path::new(
        report["temporary_directory"]
            .as_str()
            .ok_or("run has no temporary directory")?,
    );
    if !dir.join("service.json").is_file() || !dir.join("identity").is_file() {
        return Err("the lab is not prepared; pause after verify before registering profile e2e".into());
    }
    drop(UnixStream::connect(dir.join("inspection.sock"))?);
    profiles::private_directory(&base.join("profiles"))?;
    let root = profiles::directory(base, profiles::E2E)?;
    profiles::private_directory(&root)?;
    if root.join("run.toml").exists() {
        let previous = E2eRun::read(&root)?;
        if previous.run_id != run_id
            && let Ok(report) = paused_run(&previous.artifacts)
            && let Some(dir) = report["temporary_directory"].as_str()
            && UnixStream::connect(Path::new(dir).join("inspection.sock")).is_ok()
        {
            return Err(
                "profile e2e is already bound to another paused lab; close that lab before starting another".into(),
            );
        }
    }
    E2eRun { artifacts, run_id }.save(&root)?;
    Ok(())
}

pub async fn launch_profile(argv: &[String]) -> Result<()> {
    let root = hs_config::profiles::directory(&hs_config::profiles::environment_base()?, "e2e")?;
    let run = hs_config::profiles::E2eRun::read(&root)?;
    launch(&run.artifacts, &run.run_id, argv).await
}

fn gold_ssh_wrapper(helper: &Path, artifacts: &Path, ssh: &Path, config: &Path) -> String {
    format!(
        "#!/bin/sh\nif [ \"$#\" -eq 3 ] && [ \"$1\" = -o ] && [ \"$2\" = PreferredAuthentications=publickey ] && [ \"$3\" = root@gold-e2e ]; then\n  exec {} gold-shell --artifacts {}\nfi\nexec {} -F {} \"$@\"\n",
        shell_quote(&helper.display().to_string()),
        shell_quote(&artifacts.display().to_string()),
        shell_quote(&ssh.display().to_string()),
        shell_quote(&config.display().to_string()),
    )
}

async fn launch(artifacts: &Path, run_id: &str, argv: &[String]) -> Result<()> {
    let report = paused_run(artifacts)?;
    if report["run_id"] != run_id {
        return Err("profile e2e refers to another run; register the active paused lab again".into());
    }
    if argv
        .iter()
        .any(|word| word == "--config" || word.starts_with("--config="))
    {
        return Err("--config cannot override profile e2e".into());
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
    let mut command = prepare(&dir, &program, &executable("ssh")?, image, argv)?;
    if dir.join("gold.json").is_file() {
        write(
            &dir.join("client/bin/ssh"),
            &gold_ssh_wrapper(
                &std::env::current_exe()?,
                &artifacts.canonicalize()?,
                &executable("ssh")?,
                &dir.join("client/ssh_config"),
            ),
            0o700,
        )?;
        command.env("HOMESTACK_E2E_GOLD_VMID", "100");
    } else {
        command.env_remove("HOMESTACK_E2E_GOLD_VMID");
    }
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
        "Installer defaults: SSH root@pve-e2e; server domain {}. Herdr is available through the desktop session.",
        crate::domain::SEARCH
    );
    eprintln!("This profile and its credentials are removed with the VM. Keep the controller paused.");
    if dir.join("gold.json").is_file() {
        eprintln!("In the TUI, select Gold and SSH root: a stopped Gold starts for inspection and stops on logout.");
    }
    eprintln!("HTTPS uses the lab SSH SOCKS tunnel; TLS hostname and public-CA checks stay enabled.");
    // The TUI owns raw mode; its SSH handover must restore a canonical terminal.
    let _terminal = TerminalGuard::capture()?;
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

    #[test]
    fn public_key_only_gold_ssh_still_opens_the_managed_inspection_session() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let helper = directory.path().join("gold helper");
        let ssh = directory.path().join("real ssh");
        write(&helper, "#!/bin/sh\nprintf 'GOLD\\n'\nprintf '%s\\n' \"$@\"\n", 0o700)?;
        write(&ssh, "#!/bin/sh\nprintf 'SSH\\n'\nprintf '%s\\n' \"$@\"\n", 0o700)?;
        let wrapper = directory.path().join("ssh");
        let config = directory.path().join("ssh config");
        write(
            &wrapper,
            &gold_ssh_wrapper(&helper, directory.path(), &ssh, &config),
            0o700,
        )?;
        for (target, expected) in [("root@gold-e2e", "GOLD\n"), ("test1", "SSH\n")] {
            let result = Command::new(&wrapper)
                .args(["-o", "PreferredAuthentications=publickey", target])
                .output()?;
            assert!(result.status.success());
            let output = String::from_utf8(result.stdout)?;
            assert!(output.starts_with(expected), "{output}");
            if target == "test1" {
                assert!(output.contains("PreferredAuthentications=publickey\ntest1\n"));
            } else {
                assert!(output.contains("gold-shell\n--artifacts\n"));
            }
        }
        Ok(())
    }

    #[test]
    #[ignore = "helper executed by terminal_guard_preserves_child_input_and_restores_settings"]
    fn terminal_guard_child() -> Result<()> {
        let _guard = TerminalGuard::capture()?.ok_or("missing test terminal")?;
        let original = rustix::termios::tcgetattr(std::io::stdin())?;
        assert!(original.local_modes.contains(rustix::termios::LocalModes::ICANON));
        println!("CANONICAL_READY");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        assert_eq!(line, "resume\n");
        // A killed TUI can leave raw mode behind; the parent must still restore it.
        let mut raw = original;
        raw.make_raw();
        rustix::termios::tcsetattr(std::io::stdin(), rustix::termios::OptionalActions::Now, &raw)?;
        Ok(())
    }

    #[test]
    fn terminal_guard_preserves_child_input_and_restores_settings() -> Result<()> {
        let script = r#"
import os, pty, select, subprocess, sys, termios, time
master, slave = pty.openpty()
try:
    original = termios.tcgetattr(slave)
    process = subprocess.Popen([sys.argv[1], '--exact', 'client::tests::terminal_guard_child', '--ignored', '--nocapture'],
                               stdin=slave, stdout=slave, stderr=slave)
    try:
        output = b''
        deadline = time.monotonic() + 10
        while b'CANONICAL_READY' not in output:
            assert time.monotonic() < deadline and process.poll() is None, output
            if select.select([master], [], [], .1)[0]: output += os.read(master, 65536)
        assert termios.tcgetattr(slave) == original, 'wrapper changed child input mode'
        os.write(master, b'resume\n')
        assert process.wait(timeout=10) == 0
        assert termios.tcgetattr(slave) == original, 'terminal settings were not restored'
    finally:
        if process.poll() is None: process.kill()
        process.wait()
finally:
    os.close(master)
    os.close(slave)
"#;
        let result = Command::new("python3")
            .args(["-c", script])
            .arg(std::env::current_exe()?)
            .output()?;
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        Ok(())
    }

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
            &[],
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
            &[],
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
    fn ssh_uses_the_lab_config_and_herdr_remains_available() -> Result<()> {
        let dir = fixture()?;
        let ssh = executable("ssh")?;
        prepare(
            dir.path(),
            Path::new("/bin/true"),
            &ssh,
            "ghcr.io/test/server:test",
            &[],
        )?;
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
                assert!(config.contains("guest-connect.py"), "{config}");
            }
        }
        if hs_config::profiles::desktop_herdr_wrapper().is_some() {
            let wrapper = std::fs::read_to_string(dir.path().join("client/bin/herdr"))?;
            assert!(!wrapper.contains("Herdr is disabled"));
            assert!(wrapper.contains("export HOME=") && wrapper.contains("export XDG_CONFIG_HOME="));
        }
        Ok(())
    }

    #[test]
    fn guest_ssh_reads_private_aliases_and_rejects_destinations_outside_the_lab() -> Result<()> {
        let dir = fixture()?;
        let ssh = executable("ssh")?;
        prepare(dir.path(), Path::new("/bin/true"), &ssh, "test", &[])?;
        let root = dir.path().join("client");
        let guest = root.join("home/.ssh/config.d/homestack/vm100-gold.conf");
        write(
            &guest,
            "Host vm100\n  HostName 10.77.0.68\n  User root\n  IdentityFile /test/gold-key\n  StrictHostKeyChecking yes\n",
            0o600,
        )?;
        let resolved = Command::new(root.join("bin/ssh")).args(["-G", "vm100"]).output()?;
        assert!(resolved.status.success());
        let config = String::from_utf8(resolved.stdout)?;
        for expected in [
            "hostname 10.77.0.68",
            "user root",
            "identityfile /test/gold-key",
            "stricthostkeychecking true",
        ] {
            assert!(config.lines().any(|line| line == expected), "{config}");
        }
        assert!(config.contains("guest-connect.py"), "{config}");
        let fake = dir.path().join("fake-ssh");
        write(&fake, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n", 0o700)?;
        let connect = |host: &str, port: &str| {
            Command::new("python3")
                .arg(root.join("guest-connect.py"))
                .arg(dir.path())
                .arg(&fake)
                .args([host, port])
                .output()
        };
        let allowed = connect("10.77.0.68", "22")?;
        assert!(allowed.status.success());
        assert!(String::from_utf8(allowed.stdout)?.ends_with("-W\n10.77.0.68:22\npve-e2e\n"));
        for (host, port) in [
            ("production.invalid", "22"),
            ("127.0.0.1", "22"),
            ("192.168.1.1", "22"),
            ("10.77.1.68", "22"),
            ("1.1.1.1", "22"),
            ("::1", "22"),
            ("10.77.0.0", "22"),
            ("10.77.0.255", "22"),
            ("10.77.0.68", "-1"),
            ("10.77.0.68", "65536"),
        ] {
            let rejected = connect(host, port)?;
            assert!(!rejected.status.success(), "accepted {host}:{port}");
            assert!(rejected.stdout.is_empty(), "SSH ran for {host}:{port}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn stale_runs_never_start_a_client_or_use_the_desktop_profile() -> Result<()> {
        let artifacts = tempfile::tempdir()?;
        let dir = fixture()?;
        let mut report =
            serde_json::json!({"status": "passed", "temporary_directory": dir.path(), "run_id": "test-run"});
        std::fs::write(artifacts.path().join("report.json"), serde_json::to_vec(&report)?)?;
        assert!(
            launch(artifacts.path(), "test-run", &[])
                .await
                .is_err_and(|error| error.to_string().contains("not paused"))
        );
        report["status"] = serde_json::json!("paused");
        std::fs::write(artifacts.path().join("report.json"), serde_json::to_vec(&report)?)?;
        assert!(
            launch(artifacts.path(), "test-run", &[])
                .await
                .is_err_and(|error| error.to_string().contains("no VM SSH credentials"))
        );
        std::fs::write(dir.path().join("identity"), "unused")?;
        assert!(launch(artifacts.path(), "test-run", &[]).await.is_err());
        assert!(!dir.path().join("client").exists());
        Ok(())
    }

    #[test]
    fn cleanup_removes_only_its_own_profile_reference() -> Result<()> {
        let base = tempfile::tempdir()?;
        let profile = hs_config::profiles::directory(base.path(), "e2e")?;
        hs_config::profiles::private_directory(&profile)?;
        hs_config::profiles::E2eRun {
            artifacts: base.path().join("artifacts"),
            run_id: "new-run".into(),
        }
        .save(&profile)?;
        unregister_at(base.path(), "old-run")?;
        assert_eq!(hs_config::profiles::E2eRun::read(&profile)?.run_id, "new-run");
        unregister_at(base.path(), "new-run")?;
        assert!(!profile.join("run.toml").exists());
        unregister_at(base.path(), "new-run")?;
        Ok(())
    }

    #[test]
    fn profile_registration_never_replaces_another_live_lab() -> Result<()> {
        let base = tempfile::tempdir()?;
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        let first_vm = fixture()?;
        let second_vm = fixture()?;
        let first_id = uuid::Uuid::new_v4().to_string();
        let second_id = uuid::Uuid::new_v4().to_string();
        let mut sockets = Vec::new();
        for (artifacts, vm, id) in [(&first, &first_vm, &first_id), (&second, &second_vm, &second_id)] {
            write(&vm.path().join("identity"), "lab key sentinel", 0o600)?;
            write(&vm.path().join("service.json"), "{}", 0o600)?;
            sockets.push(std::os::unix::net::UnixListener::bind(
                vm.path().join("inspection.sock"),
            )?);
            let report = serde_json::json!({"status": "paused", "temporary_directory": vm.path(), "run_id": id});
            std::fs::write(artifacts.path().join("report.json"), serde_json::to_vec(&report)?)?;
        }
        register_at(base.path(), first.path())?;
        register_at(base.path(), first.path())?;
        let profile = hs_config::profiles::directory(base.path(), "e2e")?;
        assert_eq!(hs_config::profiles::E2eRun::read(&profile)?.run_id, first_id);
        assert!(
            register_at(base.path(), second.path())
                .is_err_and(|error| error.to_string().contains("another paused lab"))
        );
        assert_eq!(hs_config::profiles::E2eRun::read(&profile)?.run_id, first_id);
        // A stopped run may be replaced. The previous VM's credentials are not copied.
        std::fs::write(first.path().join("report.json"), "{\"status\":\"passed\"}")?;
        register_at(base.path(), second.path())?;
        assert_eq!(hs_config::profiles::E2eRun::read(&profile)?.run_id, second_id);
        assert_eq!(std::fs::read_dir(&profile)?.count(), 1);
        assert_eq!(
            std::fs::metadata(profile.join("run.toml"))?.permissions().mode() & 0o777,
            0o600
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_replaced_report_cannot_redirect_the_named_client() -> Result<()> {
        let artifacts = tempfile::tempdir()?;
        std::fs::write(
            artifacts.path().join("report.json"),
            "{\"status\":\"paused\",\"run_id\":\"another-run\"}",
        )?;
        assert!(
            launch(artifacts.path(), "original-run", &[])
                .await
                .is_err_and(|error| error.to_string().contains("another run"))
        );
        Ok(())
    }

    #[test]
    fn named_profile_forwards_cli_commands_without_opening_the_installer() -> Result<()> {
        let dir = fixture()?;
        let args = vec!["--json".to_owned(), "status".to_owned()];
        let command = prepare(
            dir.path(),
            Path::new("/bin/true"),
            Path::new("/usr/bin/ssh"),
            "image:test",
            &args,
        )?;
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["--json", "status"]);
        Ok(())
    }
}
