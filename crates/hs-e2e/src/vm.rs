use std::fs::File;
use std::io::Write;
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::Duration;

use tokio::process::Command;

use crate::Result;

const COMMAND_LIMIT: Duration = Duration::from_secs(30);

pub async fn command(program: &str, args: &[&str], artifacts: &Path, label: &str) -> Result<Vec<u8>> {
    let output = tokio::time::timeout(
        COMMAND_LIMIT,
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    if !output.status.success() {
        std::fs::write(artifacts.join("command-error.log"), &output.stderr)?;
        return Err(format!(
            "{label} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(output.stdout)
}

pub struct Vm {
    child: Child,
    dir: PathBuf,
    pub ssh_port: u16,
    pub api_port: u16,
    pub shutdown_confirmed: bool,
}

impl Vm {
    pub fn start(dir: &Path, artifacts: &Path, options: &crate::Options, proxy_port: u16) -> Result<Self> {
        let ssh_port = crate::proxy::unused_port(Ipv4Addr::LOCALHOST)?;
        let mut api_port = if options.web_port == 0 {
            crate::proxy::unused_port(options.web_bind)?
        } else {
            options.web_port
        };
        while api_port == ssh_port {
            if options.web_port != 0 {
                return Err("web and SSH forwarding ports collide".into());
            }
            api_port = crate::proxy::unused_port(options.web_bind)?;
        }
        let public = std::fs::read_to_string(dir.join("host-key.pub"))?;
        std::fs::write(
            dir.join("known_hosts"),
            format!("[127.0.0.1]:{ssh_port} {}\n", public.trim()),
        )?;
        let config_quote = |path: &Path| {
            format!(
                "\"{}\"",
                path.display().to_string().replace('\\', "\\\\").replace('"', "\\\"")
            )
        };
        std::fs::write(
            dir.join("ssh_config"),
            format!(
                "Host pve-e2e\n  HostName 127.0.0.1\n  Port {ssh_port}\n  User root\n  IdentityFile {}\n  UserKnownHostsFile {}\n  GlobalKnownHostsFile /dev/null\n  StrictHostKeyChecking yes\n  IdentitiesOnly yes\n  IdentityAgent none\n  PasswordAuthentication no\n",
                config_quote(&dir.join("identity")),
                config_quote(&dir.join("known_hosts"))
            ),
        )?;
        std::fs::set_permissions(dir.join("ssh_config"), std::fs::Permissions::from_mode(0o600))?;
        let stderr = File::create(artifacts.join("qemu.log"))?;
        let executable = std::env::current_exe()?;
        if executable.to_string_lossy().contains(',') {
            return Err("controller executable path must not contain commas (QEMU argument syntax)".into());
        }
        let proxy_command = format!(
            "'{}' proxy-connect {proxy_port}",
            executable.display().to_string().replace('\'', "'\"'\"'")
        );
        let child = std::process::Command::new("qemu-system-x86_64")
            .args(["-accel", "kvm", "-cpu", "host", "-smp", &options.cpus.to_string(), "-m", &options.memory.to_string(),
                "-display", "none", "-monitor", "none", "-serial", &format!("file:{}", artifacts.join("serial.log").display()),
                "-drive", &format!("file={},format=qcow2,if=virtio", dir.join("disk.qcow2").display()),
                "-drive", &format!("file={},format=raw,if=virtio,readonly=on", dir.join("seed.img").display()),
                "-nic", &format!("user,model=virtio-net-pci,restrict=on,ipv6=off,domainname={0},dnssearch={0},hostfwd=tcp:127.0.0.1:{ssh_port}-:22,hostfwd=tcp:{1}:{api_port}-:8006,guestfwd=tcp:10.0.2.100:3128-cmd:{proxy_command}", crate::domain::SEARCH, options.web_bind)])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr).spawn()?;
        Ok(Self {
            child,
            dir: dir.to_owned(),
            ssh_port,
            api_port,
            shutdown_confirmed: false,
        })
    }

    pub fn running(&mut self) -> Result<bool> {
        Ok(self.child.try_wait()?.is_none())
    }

    fn ssh_command(&self) -> Command {
        let mut command = Command::new("ssh");
        command
            .args([
                "-F",
                "/dev/null",
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "IdentityAgent=none",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "GlobalKnownHostsFile=/dev/null",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "ConnectionAttempts=1",
                "-o",
                "ServerAliveInterval=10",
                "-o",
                "ServerAliveCountMax=2",
                "-o",
                &format!("UserKnownHostsFile={}", self.dir.join("known_hosts").display()),
                "-i",
                &self.dir.join("identity").display().to_string(),
                "-p",
                &self.ssh_port.to_string(),
                "root@127.0.0.1",
                "bash",
                "-se",
            ])
            .stdin(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    pub async fn ssh(&self, script: &str, limit: Duration) -> Result<std::process::Output> {
        let mut child = self
            .ssh_command()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().ok_or("SSH stdin is unavailable")?;
        let result = tokio::time::timeout(limit, async {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(script.as_bytes()).await?;
            stdin.shutdown().await?;
            drop(stdin);
            child.wait_with_output().await
        })
        .await??;
        Ok(result)
    }

    pub async fn logged_ssh(&self, script: &str, limit: Duration, log: &Path) -> Result<()> {
        let file = File::options().create(true).append(true).open(log)?;
        let mut child = self.ssh_command().stdout(file.try_clone()?).stderr(file).spawn()?;
        let mut stdin = child.stdin.take().ok_or("SSH stdin is unavailable")?;
        let status = tokio::time::timeout(limit, async {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(script.as_bytes()).await?;
            stdin.shutdown().await?;
            drop(stdin);
            child.wait().await
        })
        .await??;
        if !status.success() {
            return Err(format!("guest command failed ({status}); see {}", log.display()).into());
        }
        Ok(())
    }

    pub async fn checked_ssh(&self, script: &str, limit: Duration, log: &Path) -> Result<Vec<u8>> {
        let output = self.ssh(script, limit).await?;
        let mut file = File::options().create(true).append(true).open(log)?;
        file.write_all(&output.stdout)?;
        file.write_all(&output.stderr)?;
        if !output.status.success() {
            return Err(format!("guest command failed ({}); see {}", output.status, log.display()).into());
        }
        Ok(output.stdout)
    }

    pub async fn wait_ssh(&mut self, previous_boot: Option<&str>) -> Result<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
        loop {
            if !self.running()? {
                return Err("QEMU exited before guest readiness; inspect qemu.log and serial.log".into());
            }
            if let Ok(output) = self
                .ssh("cat /proc/sys/kernel/random/boot_id\n", Duration::from_secs(10))
                .await
            {
                let boot = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if output.status.success() && !boot.is_empty() && previous_boot != Some(boot.as_str()) {
                    return Ok(boot);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("SSH readiness deadline exceeded".into());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    pub async fn poweroff(&mut self) -> Result<()> {
        let output = self
            .ssh("systemctl poweroff --no-block\n", Duration::from_secs(15))
            .await?;
        if !output.status.success() {
            return Err("guest refused graceful poweroff".into());
        }
        tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                if let Some(status) = self.child.try_wait()? {
                    if !status.success() {
                        return Err(format!("QEMU exited with {status}").into());
                    }
                    self.shutdown_confirmed = true;
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await?
    }

    pub fn terminate(&mut self) -> Result<()> {
        if self.running()? {
            self.child.kill()?;
        }
        self.child.wait()?;
        Ok(())
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if let Err(error) = self.terminate() {
            eprintln!("QEMU cleanup failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_vm_terminates_and_reaps_its_process() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let child = std::process::Command::new("sleep").arg("60").spawn()?;
        let pid = child.id();
        let vm = Vm {
            child,
            dir: dir.path().to_owned(),
            ssh_port: 0,
            api_port: 0,
            shutdown_confirmed: false,
        };
        drop(vm);
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "VM process was not reaped"
        );
        Ok(())
    }
}
