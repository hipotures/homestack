//! Terminal progress from APT's machine-readable status stream.

use std::fs::File;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::Command;

use crate::Result;

pub async fn run(mut command: Command, script: &str, limit: Duration, log: &Path) -> Result<()> {
    let mut file = File::options().create(true).append(true).open(log)?;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(file.try_clone()?)
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("installer stdin is unavailable")?;
    let stdout = child.stdout.take().ok_or("installer stdout is unavailable")?;
    let mut progress = Progress::new();
    let status = tokio::time::timeout(limit, async {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        // FD 3 carries only APT status. All ordinary guest output and SSH
        // diagnostics go directly to the artifact; no terminal is allocated.
        stdin.write_all(b"exec 3>&1\nexec 1>&2\n").await?;
        stdin.write_all(script.as_bytes()).await?;
        stdin.shutdown().await?;
        drop(stdin);
        let mut stdout = BufReader::new(stdout);
        let mut line = Vec::new();
        while stdout.read_until(b'\n', &mut line).await? != 0 {
            file.write_all(&line)?;
            progress.update(&String::from_utf8_lossy(&line));
            line.clear();
        }
        child.wait().await
    })
    .await??;
    if !status.success() {
        return Err(format!("guest command failed ({status}); see {}", log.display()).into());
    }
    Ok(())
}

struct Progress {
    terminal: bool,
    width: usize,
    stage: Option<&'static str>,
    percent: f64,
    updated: Instant,
}

impl Progress {
    fn new() -> Self {
        Self {
            terminal: std::io::stderr().is_terminal(),
            width: 0,
            stage: None,
            percent: 0.0,
            updated: Instant::now(),
        }
    }

    fn update(&mut self, line: &str) {
        let Some((stage, percent, message)) = status(line) else {
            return;
        };
        let changed = self.stage != Some(stage);
        let interval = if self.terminal {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(5)
        };
        if !changed && !(percent == 100.0 && self.percent < 100.0) && self.updated.elapsed() < interval {
            return;
        }
        let filled = (percent / 100.0 * 24.0) as usize;
        let message: String = message
            .chars()
            .filter(|ch| !ch.is_control())
            .take(if self.terminal { 28 } else { 72 })
            .collect();
        let line = format!(
            "{stage} [{}{}] {percent:5.1}% {message}",
            "=".repeat(filled),
            "-".repeat(24 - filled)
        );
        let mut stderr = std::io::stderr().lock();
        if self.terminal {
            if changed && self.width > 0 {
                let _ = writeln!(stderr);
                self.width = 0;
            }
            self.width = self.width.max(line.len());
            let _ = write!(stderr, "\r{line:width$}", width = self.width);
            let _ = stderr.flush();
        } else {
            let _ = writeln!(stderr, "{line}");
        }
        self.stage = Some(stage);
        self.percent = percent;
        self.updated = Instant::now();
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if self.terminal && self.width > 0 {
            let _ = writeln!(std::io::stderr());
        }
    }
}

fn status(line: &str) -> Option<(&'static str, f64, &str)> {
    let mut fields = line.trim().splitn(4, ':');
    let stage = match fields.next()? {
        "dlstatus" => "Package download",
        "pmstatus" => "Package install",
        _ => return None,
    };
    fields.next()?;
    let percent: f64 = fields.next()?.parse().ok()?;
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return None;
    }
    Some((stage, percent, fields.next()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apt_download_and_install_status_preserve_percent_and_description() {
        assert_eq!(
            status("dlstatus:1:9.05654:Downloading file 1 of 3 (4m40s remaining)\n"),
            Some(("Package download", 9.05654, "Downloading file 1 of 3 (4m40s remaining)"))
        );
        assert_eq!(
            status("pmstatus:proxmox-ve:80.0000:Configuring proxmox-ve:amd64\n"),
            Some(("Package install", 80.0, "Configuring proxmox-ve:amd64"))
        );
        for line in [
            "Unpacking proxmox-ve...",
            "pmstatus:pkg:NaN:bad",
            "pmstatus:pkg:inf:bad",
            "pmstatus:pkg:101:bad",
            "pmstatus:pkg:-1:bad",
            "pmstatus:pkg:50",
            "pmerror:pkg:50:failed",
        ] {
            assert!(status(line).is_none(), "{line}");
        }
    }

    #[tokio::test]
    async fn progress_keeps_normal_output_and_preserves_installer_failure() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = dir.path().join("install.log");
        let mut command = Command::new("bash");
        command.arg("-se");
        let result = run(command, "printf 'ordinary output\\n'\nprintf 'diagnostic output\\n' >&2\nprintf 'dlstatus:1:50:Retrieving packages\\n' >&3\nprintf 'pmstatus:proxmox-ve:75:Configuring proxmox-ve\\n' >&3\nexit 7\n",
            Duration::from_secs(5), &log).await;
        assert!(result.is_err());
        let contents = std::fs::read_to_string(&log)?;
        for expected in [
            "ordinary output",
            "diagnostic output",
            "dlstatus:1:50:Retrieving packages",
            "pmstatus:proxmox-ve:75:Configuring proxmox-ve",
        ] {
            assert!(contents.contains(expected), "missing {expected}");
        }
        Ok(())
    }
}
