//! Explicit installation phases and manual inspection over a private socket.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::{Result, save_report};

pub const NAMES: [&str; 11] = [
    "preflight",
    "domain",
    "image",
    "boot",
    "kernel",
    "proxmox",
    "network",
    "gold",
    "homestack",
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
        if inspect && let Err(error) = pause(self.dir, self.artifacts, report).await {
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

pub async fn pause(dir: &Path, artifacts: &Path, report: &mut Value) -> Result<()> {
    let path = dir.join("inspection.sock");
    let listener = UnixListener::bind(&path)?;
    let _guard = SocketGuard(path.clone());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    report["status"] = json!("paused");
    report["control_socket"] = json!(path);
    report["pause"] = json!({"phase": report["phase"], "started_at_ms": now_ms()});
    save_report(artifacts, report)?;
    eprintln!(
        "Paused after phase {} until you resume. No inspection time limit.",
        report["phase"]
    );
    if dir.join("service.json").is_file() {
        match crate::client::register(artifacts) {
            Ok(()) => {
                eprintln!("Open profile e2e in another terminal on the runner host:");
                eprintln!(
                    "  {} --profil e2e",
                    shell_quote(
                        &std::env::current_exe()?
                            .with_file_name("homestack")
                            .display()
                            .to_string()
                    )
                );
            }
            Err(error) => eprintln!("Cannot register profile e2e: {error}. The lab remains paused."),
        }
        eprintln!(
            "SSH root@pve-e2e and server domain {} are prefilled; the test client does not use the production profile.",
            crate::domain::SEARCH
        );
    }
    eprintln!("Inspect the VM, then resume on the runner host:");
    eprintln!(
        "  homestack-e2e resume --artifacts {}",
        shell_quote(&artifacts.display().to_string())
    );
    eprintln!("Ctrl-C or SIGTERM removes the VM. Inspection time does not consume the installation deadline.");
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
        let paused = pause(dir.path(), artifacts.path(), &mut report);
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

    #[tokio::test(start_paused = true)]
    async fn inspection_waits_for_manual_resume_after_days() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let artifacts = tempfile::tempdir()?;
        let task_dir = dir.path().to_owned();
        let task_artifacts = artifacts.path().to_owned();
        let paused = tokio::spawn(async move {
            let mut report = json!({"phase": "proxmox", "phase_status": "failed"});
            pause(&task_dir, &task_artifacts, &mut report).await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(report)
        });
        tokio::task::yield_now().await;
        assert!(dir.path().join("inspection.sock").exists());
        tokio::time::advance(Duration::from_secs(2 * 24 * 60 * 60)).await;
        tokio::task::yield_now().await;
        assert!(
            !paused.is_finished(),
            "inspection must not expire without manual resume"
        );
        let report: Value = serde_json::from_slice(&std::fs::read(artifacts.path().join("report.json"))?)?;
        assert_eq!(report["status"], "paused");
        assert_eq!(report["phase_status"], "failed");
        assert!(report["pause"].get("timeout_seconds").is_none());
        super::resume(artifacts.path()).await?;
        let report = paused.await??;
        assert_eq!(report["status"], "running");
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
