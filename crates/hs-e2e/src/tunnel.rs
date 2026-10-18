//! A client-owned SSH SOCKS tunnel and reverse proxy for the disposable lab.

use std::net::Ipv4Addr;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::Result;

pub struct Tunnel {
    pub port: u16,
    child: Child,
}

impl Tunnel {
    pub async fn start(dir: &Path, registry_port: u16) -> Result<Self> {
        let port = crate::proxy::unused_port(Ipv4Addr::LOCALHOST)?;
        let errors = std::fs::File::create(dir.join("client/tunnel.log"))?;
        let child = Command::new("ssh")
            .args([
                "-F",
                &dir.join("ssh_config").display().to_string(),
                "-N",
                "-o",
                "BatchMode=yes",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=10",
                "-o",
                "ServerAliveCountMax=2",
                "-D",
                &format!("127.0.0.1:{port}"),
                "-R",
                &format!("10.77.0.1:3129:127.0.0.1:{registry_port}"),
                "pve-e2e",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(errors)
            .kill_on_drop(true)
            .spawn()?;
        let mut tunnel = Self { port, child };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if tunnel.child.try_wait()?.is_some() {
                return Err(format!("SSH tunnel stopped; see {}", dir.join("client/tunnel.log").display()).into());
            }
            if let Ok(mut stream) = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await {
                stream.write_all(&[5, 1, 0]).await?;
                let mut reply = [0; 2];
                if tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut reply))
                    .await
                    .is_ok_and(|result| result.is_ok())
                    && reply == [5, 0]
                {
                    return Ok(tunnel);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("SSH SOCKS tunnel did not become ready".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn stop(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            self.child.start_kill()?;
        }
        self.child.wait().await?;
        Ok(())
    }

    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }
}
