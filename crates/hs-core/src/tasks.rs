//! Proxmox tasks (mapping T4): follow a UPID to its end and turn its log
//! into progress.
//!
//! Python's `qm` commands wait for their task and print its log, which
//! `lifecycle.run_transfer_with_progress` reads with `CLONE_TRANSFER_RE`.
//! Through the API a call returns the UPID at once; [`wait_task`] polls
//! `tasks/{upid}/status`, reads `tasks/{upid}/log` from the last line seen
//! and reports both to an [`Observer`].

use std::time::Duration;

use hs_model::jobs::JobEvent;
use serde_json::Value;
use tokio::time::Instant;

use crate::error::CoreError;
use crate::pve::Pve;

/// Receives what an operation reports while it runs.
pub trait Observer: Send + Sync {
    fn event(&self, event: JobEvent);
}

/// Discards every event.
pub struct NoObserver;

impl Observer for NoObserver {
    fn event(&self, _event: JobEvent) {}
}

/// How to wait for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskWait {
    /// Pause between two status reads.
    pub poll: Duration,
    /// Give up after this long.
    pub timeout: Duration,
}

impl TaskWait {
    /// One status read per second, as long as `timeout`.
    #[must_use]
    pub const fn up_to(timeout: Duration) -> Self {
        Self {
            poll: Duration::from_secs(1),
            timeout,
        }
    }
}

/// How a task ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResult {
    pub upid: String,
    /// `OK`, `WARNINGS: N`, or the error message.
    pub exitstatus: String,
}

impl TaskResult {
    /// Proxmox counts a task with warnings as successful (`qm` exits 0).
    #[must_use]
    pub fn ok(&self) -> bool {
        self.exitstatus == "OK" || self.exitstatus.starts_with("WARNINGS")
    }
}

/// Proxmox's error when a VM's config lock stays taken for its 10 seconds:
/// `can't lock file '/var/lock/qemu-server/lock-<vmid>.conf' - got timeout`.
/// PVE 9.2 holds it for 10–25 s after some starts, with no task to show
/// for it (phase 9, live; the Proxmox UI shows the same).
#[must_use]
pub fn is_lock_timeout(text: &str) -> bool {
    text.contains("can't lock file") && text.contains("got timeout")
}

/// How long a call refused for a taken lock is tried again.
pub const LOCK_RETRY_LIMIT: Duration = Duration::from_secs(120);
/// Pause between two tries.
const LOCK_RETRY_PAUSE: Duration = Duration::from_secs(5);

/// Run `attempt` again while it fails only because the VM's config lock is
/// taken ([`is_lock_timeout`]): every 5 seconds (every `poll` when that is
/// under a second: the tests), for up to [`LOCK_RETRY_LIMIT`]. Such a failure
/// changed nothing, so the same call is made again. Any other outcome is
/// returned as it is.
pub async fn retry_while_locked<T, F, Fut>(poll: Duration, mut attempt: F) -> Result<T, CoreError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, CoreError>>,
{
    let pause = if poll < Duration::from_secs(1) {
        poll
    } else {
        LOCK_RETRY_PAUSE
    };
    let deadline = Instant::now() + LOCK_RETRY_LIMIT;
    loop {
        match attempt().await {
            Err(error) if is_lock_timeout(&error.to_string()) && Instant::now() + pause < deadline => {
                tokio::time::sleep(pause).await;
            }
            outcome => return outcome,
        }
    }
}

/// The fields of `UPID:node:pid:pstart:starttime:type:id:user:`.
fn upid_fields(upid: &str) -> Option<Vec<&str>> {
    let fields: Vec<&str> = upid.split(':').collect();
    (fields.len() >= 8 && fields[0] == "UPID" && !fields[1].is_empty()).then_some(fields)
}

/// The node a task runs on, from its UPID.
#[must_use]
pub fn upid_node(upid: &str) -> Option<String> {
    upid_fields(upid).map(|fields| fields[1].to_owned())
}

/// Lines read per log request.
const LOG_PAGE: usize = 500;

/// Proxmox answers an empty task log (a task that has not written yet) with
/// one made-up line, `{"n": 1, "t": "no content"}` (`PVE::Tools::dump_logfile`,
/// "ExtJS store.guaranteeRange() does not like empty array"; observed on
/// PVE 9.2). It is not part of the log.
fn placeholder(entries: &[Value]) -> bool {
    matches!(entries, [only]
        if only.get("n").and_then(Value::as_u64) == Some(1)
            && only.get("t").and_then(Value::as_str) == Some("no content"))
}

/// Turns new lines of a task's log into progress.
pub trait LogTracker {
    fn progress(&mut self, output: &str) -> Option<TransferProgress>;
}

impl LogTracker for TransferTracker {
    fn progress(&mut self, output: &str) -> Option<TransferProgress> {
        self.update(output)
    }
}

/// Follow a task from its UPID to its end. Every log line is reported once
/// (`TaskLog`), transfer lines also as `Progress`; `TaskStarted` comes first
/// and `TaskFinished` last. A task that has not ended within the timeout is
/// an error; a task that failed is not (see [`TaskResult::ok`]).
pub async fn wait_task(
    pve: &impl Pve,
    upid: &str,
    wait: &TaskWait,
    observer: &impl Observer,
) -> Result<TaskResult, CoreError> {
    wait_task_tracking(pve, upid, wait, observer, &mut TransferTracker::default()).await
}

/// [`wait_task`] with the progress read from the log by `tracker`.
pub async fn wait_task_tracking(
    pve: &impl Pve,
    upid: &str,
    wait: &TaskWait,
    observer: &impl Observer,
    tracker: &mut impl LogTracker,
) -> Result<TaskResult, CoreError> {
    let fields = upid_fields(upid).ok_or_else(|| CoreError::Unusable(format!("invalid task id {upid:?}")))?;
    let node = fields[1];
    observer.event(JobEvent::TaskStarted {
        node: node.to_owned(),
        upid: upid.to_owned(),
        kind: fields[5].to_owned(),
        vmid: fields[6].parse().ok(),
    });
    let deadline = Instant::now() + wait.timeout;
    let mut seen = 0_usize;
    loop {
        let status = pve.get(&format!("/nodes/{node}/tasks/{upid}/status"), &[]).await?;
        loop {
            let start = seen.to_string();
            let limit = LOG_PAGE.to_string();
            let log = pve
                .get(
                    &format!("/nodes/{node}/tasks/{upid}/log"),
                    &[("start", &start), ("limit", &limit)],
                )
                .await?;
            let entries = log.as_array().cloned().unwrap_or_default();
            if placeholder(&entries) {
                break;
            }
            // Lines are numbered from 1; keep only those not seen yet.
            let mut lines = Vec::new();
            for entry in &entries {
                let number = entry.get("n").and_then(Value::as_u64).unwrap_or(0);
                let number = usize::try_from(number).unwrap_or(usize::MAX);
                if number > seen
                    && let Some(text) = entry.get("t").and_then(Value::as_str)
                {
                    lines.push(text.to_owned());
                    seen = number;
                }
            }
            if lines.is_empty() {
                break;
            }
            if let Some(progress) = tracker.progress(&lines.join("\n")) {
                observer.event(JobEvent::Progress {
                    percent: progress.percent,
                    detail: progress.detail,
                });
            }
            let full_page = entries.len() == LOG_PAGE;
            observer.event(JobEvent::TaskLog {
                upid: upid.to_owned(),
                lines,
            });
            if !full_page {
                break;
            }
        }
        if status.get("status").and_then(Value::as_str) == Some("stopped") {
            let exitstatus = status
                .get("exitstatus")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            observer.event(JobEvent::TaskFinished {
                upid: upid.to_owned(),
                exitstatus: exitstatus.clone(),
            });
            return Ok(TaskResult {
                upid: upid.to_owned(),
                exitstatus,
            });
        }
        if Instant::now() >= deadline {
            return Err(CoreError::Failed(format!(
                "Proxmox task {upid} did not finish within {}s",
                wait.timeout.as_secs()
            )));
        }
        tokio::time::sleep(wait.poll).await;
    }
}

/// One reading of a transfer.
#[derive(Debug, Clone, PartialEq)]
pub struct TransferProgress {
    /// Clamped to 0–100.
    pub percent: f64,
    /// `A / B U` when both sizes have the same unit, else `A U / B V`.
    pub detail: String,
}

/// The `update_transfer` callback of `run_transfer_with_progress`: the last
/// `CLONE_TRANSFER_RE` match of new output, reported when the percentage
/// changed.
#[derive(Debug, Default)]
pub struct TransferTracker {
    last: Option<f64>,
}

impl TransferTracker {
    pub fn update(&mut self, output: &str) -> Option<TransferProgress> {
        let (current, current_unit, total, total_unit, percent) = last_transfer(output)?;
        let percent = percent.clamp(0.0, 100.0);
        if self.last == Some(percent) {
            return None;
        }
        self.last = Some(percent);
        let detail = if current_unit == total_unit {
            format!("{current} / {total} {current_unit}")
        } else {
            format!("{current} {current_unit} / {total} {total_unit}")
        };
        Some(TransferProgress { percent, detail })
    }
}

/// `transferred\s+([0-9.]+)\s+([KMGT]?i?B)\s+of\s+([0-9.]+)\s+([KMGT]?i?B)\s+\(([0-9.]+)%\)`,
/// last match.
fn last_transfer(text: &str) -> Option<(&str, &str, &str, &str, f64)> {
    text.match_indices("transferred")
        .filter_map(|(index, word)| transfer_at(&text[index + word.len()..]))
        .last()
}

fn transfer_at(rest: &str) -> Option<(&str, &str, &str, &str, f64)> {
    let mut scanner = Scanner(rest);
    scanner.spaces()?;
    let current = scanner.take(|c| c.is_ascii_digit() || c == '.')?;
    scanner.spaces()?;
    let current_unit = scanner.unit()?;
    scanner.spaces()?;
    scanner.literal("of")?;
    scanner.spaces()?;
    let total = scanner.take(|c| c.is_ascii_digit() || c == '.')?;
    scanner.spaces()?;
    let total_unit = scanner.unit()?;
    scanner.spaces()?;
    scanner.literal("(")?;
    let percent = scanner.take(|c| c.is_ascii_digit() || c == '.')?;
    scanner.literal("%)")?;
    Some((current, current_unit, total, total_unit, percent.parse().ok()?))
}

/// A cursor for the small pattern above.
struct Scanner<'a>(&'a str);

impl<'a> Scanner<'a> {
    /// `\s+`
    fn spaces(&mut self) -> Option<()> {
        let trimmed = self.0.trim_start();
        (trimmed.len() < self.0.len()).then(|| self.0 = trimmed)
    }

    /// One or more characters matching `accept`.
    fn take(&mut self, accept: impl Fn(char) -> bool) -> Option<&'a str> {
        let end = self.0.find(|c: char| !accept(c)).unwrap_or(self.0.len());
        if end == 0 {
            return None;
        }
        let (taken, rest) = self.0.split_at(end);
        self.0 = rest;
        Some(taken)
    }

    fn literal(&mut self, text: &str) -> Option<()> {
        self.0 = self.0.strip_prefix(text)?;
        Some(())
    }

    /// `[KMGT]?i?B`
    fn unit(&mut self) -> Option<&'a str> {
        let start = self.0;
        let mut rest = self.0;
        if let Some(after) = rest.strip_prefix(['K', 'M', 'G', 'T']) {
            rest = after;
        }
        if let Some(after) = rest.strip_prefix('i') {
            rest = after;
        }
        rest = rest.strip_prefix('B')?;
        self.0 = rest;
        Some(&start[..start.len() - rest.len()])
    }
}
