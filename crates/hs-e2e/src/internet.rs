//! Own the rootless Internet gateway for exactly this disposable VM lifecycle.

use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

use crate::Result;

pub struct Internet {
    child: Child,
}

impl Internet {
    pub async fn start(dir: &Path, artifacts: &Path) -> Result<Self> {
        let source = dir.join("internet-gateway.py");
        crate::service::write_private(&source, include_bytes!("internet-gateway.py"))?;
        let log = std::fs::File::options()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(artifacts.join("internet.log"))?;
        let child = Command::new("python3")
            .arg(source)
            .arg(dir)
            .arg("qemu-system-x86_64")
            .arg(std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        let mut gateway = Self { child };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if gateway.child.try_wait()?.is_some() {
                return Err(format!(
                    "Internet gateway stopped; see {}",
                    artifacts.join("internet.log").display()
                )
                .into());
            }
            if dir.join("internet.json").is_file() {
                return Ok(gateway);
            }
            if tokio::time::Instant::now() >= deadline {
                gateway.stop().await?;
                return Err("Internet gateway did not become ready".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn stop(&mut self) -> Result<()> {
        drop(self.child.stdin.take());
        tokio::time::timeout(Duration::from_secs(15), self.child.wait()).await??;
        Ok(())
    }
}

impl Drop for Internet {
    fn drop(&mut self) {
        // EOF also handles errors; pidfd in the helper handles a killed controller.
        drop(self.child.stdin.take());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn boundary_and_process_ownership() -> crate::Result<()> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let output = std::process::Command::new("python3")
            .arg(root.join("internet-test.py"))
            .arg(root.join("internet-gateway.py"))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()?;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        Ok(())
    }
}
