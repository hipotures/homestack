//! Explicit installation phases and bounded inspection over a private socket.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::{Result, save_report};

pub const NAMES: [&str; 8] = [
    "preflight",
    "domain",
    "image",
    "boot",
    "kernel",
    "proxmox",
    "verify",
    "shutdown",
];

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

pub fn begin(report: &mut Value, phase: &str) {
    report["phase"] = json!(phase);
    report["phase_status"] = json!("running");
    report["phases"][phase] = json!({"status": "running", "started_at_ms": now_ms()});
    eprintln!("Phase: {phase}");
}

pub fn finish<T>(report: &mut Value, phase: &str, outcome: &Result<T>, elapsed: Duration) {
    let status = if outcome.is_ok() { "passed" } else { "failed" };
    report["phase_status"] = json!(status);
    report["phases"][phase]["status"] = json!(status);
    report["phases"][phase]["finished_at_ms"] = json!(now_ms());
    report["phases"][phase]["duration_seconds"] = json!(elapsed.as_secs_f64());
    report["phases"][phase]["error"] = json!(outcome.as_ref().err().map(|error| crate::describe(error.as_ref())));
}

pub struct Progress<'a> {
    options: &'a crate::Options,
    dir: &'a Path,
    artifacts: &'a Path,
    remaining: Duration,
}

impl<'a> Progress<'a> {
    pub fn new(options: &'a crate::Options, dir: &'a Path, artifacts: &'a Path) -> Self {
        Self {
            options,
            dir,
            artifacts,
            remaining: Duration::from_secs(options.timeout),
        }
    }

    pub async fn execute<T>(
        &mut self,
        phase: &str,
        report: &mut Value,
        action: impl AsyncFnOnce(&mut Value) -> Result<T>,
    ) -> Result<T> {
        begin(report, phase);
        save_report(self.artifacts, report)?;
        let started = tokio::time::Instant::now();
        let outcome = tokio::time::timeout(self.remaining, action(report))
            .await
            .unwrap_or_else(|_| Err("installation deadline exceeded".into()));
        let elapsed = started.elapsed();
        self.remaining = self.remaining.saturating_sub(elapsed);
        finish(report, phase, &outcome, elapsed);
        save_report(self.artifacts, report)?;
        let inspect = (outcome.is_err() && self.options.pause_on_failure)
            || (outcome.is_ok() && self.options.pause_after.iter().any(|name| name == phase));
        if inspect
            && let Err(error) = pause(
                self.dir,
                self.artifacts,
                report,
                Duration::from_secs(self.options.pause_timeout),
            )
            .await
        {
            return Err(match outcome {
                Ok(_) => error,
                Err(original) => format!(
                    "{}; inspection: {}",
                    crate::describe(original.as_ref()),
                    crate::describe(error.as_ref())
                )
                .into(),
            });
        }
        outcome
    }
}

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn pause(dir: &Path, artifacts: &Path, report: &mut Value, timeout: Duration) -> Result<()> {
    let path = dir.join("inspection.sock");
    let listener = UnixListener::bind(&path)?;
    let _guard = SocketGuard(path.clone());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    report["status"] = json!("paused");
    report["control_socket"] = json!(path);
    report["pause"] =
        json!({"phase": report["phase"], "started_at_ms": now_ms(), "timeout_seconds": timeout.as_secs()});
    save_report(artifacts, report)?;
    eprintln!(
        "Paused after phase {} for up to {} seconds.",
        report["phase"],
        timeout.as_secs()
    );
    eprintln!("Inspect the VM, then resume on the runner host:");
    eprintln!(
        "  homestack-e2e resume --artifacts {}",
        shell_quote(&artifacts.display().to_string())
    );
    eprintln!("Ctrl-C or SIGTERM removes the VM. Inspection time does not consume the installation deadline.");
    tokio::time::timeout(timeout, async {
        loop {
            let (mut stream, _) = listener.accept().await?;
            if matches!(
                tokio::time::timeout(Duration::from_secs(3), stream.read_u8()).await,
                Ok(Ok(b'R'))
            ) {
                stream.write_all(b"K").await?;
                break;
            }
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .map_err(|_| "inspection deadline exceeded; removing the VM")??;
    report["status"] = json!("running");
    report["control_socket"] = Value::Null;
    report["pause"]["resumed_at_ms"] = json!(now_ms());
    save_report(artifacts, report)?;
    Ok(())
}

pub async fn resume(artifacts: &Path) -> Result<()> {
    let report: Value = serde_json::from_slice(&std::fs::read(artifacts.join("report.json"))?)?;
    if report["status"] != "paused" {
        return Err("the selected run is not paused".into());
    }
    let path = report["control_socket"]
        .as_str()
        .ok_or("paused run has no control socket")?;
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = UnixStream::connect(path).await?;
        stream.write_all(b"R").await?;
        if stream.read_u8().await? != b'K' {
            return Err("controller did not acknowledge resume".into());
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .map_err(|_| "resume request timed out")??;
    eprintln!("Resuming phase {}.", report["phase"]);
    Ok(())
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn inspection_requires_resume_and_removes_its_socket() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let artifacts = tempfile::tempdir()?;
        let mut report = json!({"phase": "boot", "phases": {}});
        let paused = pause(dir.path(), artifacts.path(), &mut report, Duration::from_secs(2));
        let resume = async {
            for _ in 0..100 {
                if artifacts.path().join("report.json").exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            super::resume(artifacts.path()).await
        };
        let (paused, resumed) = tokio::join!(paused, resume);
        paused?;
        resumed?;
        assert_eq!(report["status"], "running");
        assert!(!dir.path().join("inspection.sock").exists());
        assert!(super::resume(artifacts.path()).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn inspection_deadline_keeps_failure_visible_and_removes_socket() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let artifacts = tempfile::tempdir()?;
        let mut report = json!({"phase": "proxmox", "phase_status": "failed"});
        assert!(
            pause(dir.path(), artifacts.path(), &mut report, Duration::from_millis(5))
                .await
                .is_err()
        );
        assert_eq!(report["phase_status"], "failed");
        assert!(!dir.path().join("inspection.sock").exists());
        Ok(())
    }

    #[tokio::test]
    async fn a_failed_phase_stays_failed_after_manual_resume() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let artifacts = tempfile::tempdir()?;
        let mut options = crate::Options::parse(&["run".into()], "test")?;
        options.pause_on_failure = true;
        let mut progress = Progress::new(&options, dir.path(), artifacts.path());
        let mut report = json!({"phases": {}});
        let phase = progress.execute("proxmox", &mut report, async |_| {
            Err::<(), _>("installation failed".into())
        });
        let resume = async {
            loop {
                let path = artifacts.path().join("report.json");
                if let Ok(bytes) = std::fs::read(path)
                    && let Ok(report) = serde_json::from_slice::<Value>(&bytes)
                    && report["status"] == "paused"
                {
                    return super::resume(artifacts.path()).await;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let (phase, resumed) =
            tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(phase, resume) }).await?;
        resumed?;
        assert!(phase.is_err_and(|error| error.to_string() == "installation failed"));
        assert_eq!(report["phases"]["proxmox"]["status"], "failed");
        assert!(report["pause"]["resumed_at_ms"].is_number());
        Ok(())
    }

    #[tokio::test]
    async fn inspection_time_is_excluded_from_the_active_budget() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let artifacts = tempfile::tempdir()?;
        let mut options = crate::Options::parse(&["run".into()], "test")?;
        options.timeout = 1;
        options.pause_after = vec!["boot".into()];
        let mut progress = Progress::new(&options, dir.path(), artifacts.path());
        let mut report = json!({"phases": {}});
        let phase = progress.execute("boot", &mut report, async |_| Ok(()));
        let resume = async {
            while !dir.path().join("inspection.sock").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            super::resume(artifacts.path()).await
        };
        let (phase, resumed) =
            tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(phase, resume) }).await?;
        phase?;
        resumed?;
        assert!(progress.remaining > Duration::from_millis(900));
        Ok(())
    }
}
