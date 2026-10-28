//! The QEMU guest agent (mapping T6, D1): commands, pings and file reads.
//!
//! `guest_exec` ports `guest._complete_guest_exec` onto `agent/exec` and
//! `agent/exec-status`, which need `VM.GuestAgent.Unrestricted`. Native
//! filesystem statistics use audit commands and `file-read`; virtiofs
//! statistics use a bounded read-only guest command because `get-fsinfo`
//! does not report those mounts.

use std::time::Duration;

use base64::Engine;
use hs_pve::PveError;
use serde_json::Value;
use tokio::time::Instant;

use crate::error::CoreError;
use crate::inventory::truthy;
use crate::pve::Pve;

/// Bound of one ping (Python: `timeout -k 2s 5s qm guest cmd VMID ping`).
pub const QGA_PING_TIMEOUT: Duration = Duration::from_secs(5);

/// Default bound of one guest command (`_complete_guest_exec`).
pub const GUEST_EXEC_TIMEOUT: Duration = Duration::from_secs(300);

/// Pause between two `exec-status` reads.
const EXEC_POLL: Duration = Duration::from_millis(200);

/// Pause between two pings while waiting for the agent.
const QGA_RETRY: Duration = Duration::from_secs(2);

/// The final `exec-status` of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    pub exited: bool,
    pub exitcode: i64,
    pub out: String,
    pub err: String,
}

fn agent(node: &str, vmid: u32, command: &str) -> String {
    format!("/nodes/{node}/qemu/{vmid}/agent/{command}")
}

/// One ping, bounded by [`QGA_PING_TIMEOUT`].
pub async fn qga_ping(pve: &impl Pve, node: &str, vmid: u32) -> Result<(), CoreError> {
    match tokio::time::timeout(QGA_PING_TIMEOUT, pve.post(&agent(node, vmid, "ping"), &[])).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Err(CoreError::Failed(format!(
            "QEMU Guest Agent ping did not answer within {}s",
            QGA_PING_TIMEOUT.as_secs()
        ))),
    }
}

/// `guest.wait_for_qga_on_node`: ping every two seconds until the agent
/// answers or `timeout` has passed; any failure counts as one attempt.
pub async fn wait_for_qga(pve: &impl Pve, node: &str, vmid: u32, timeout: Duration) -> Result<(), CoreError> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    while Instant::now() < deadline {
        match qga_ping(pve, node, vmid).await {
            Ok(()) => return Ok(()),
            Err(error) => last = Some(error),
        }
        tokio::time::sleep(QGA_RETRY).await;
    }
    let mut message = format!(
        "VM {vmid} on {node} did not expose a working QEMU Guest Agent within {}s",
        timeout.as_secs()
    );
    if let Some(error) = last {
        message.push_str(": ");
        message.push_str(&error.to_string());
        Err(CoreError::Context {
            message,
            causes: vec![error],
        })
    } else {
        Err(CoreError::Failed(message))
    }
}

fn integer(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

fn text(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// `shlex.quote`
#[must_use]
pub fn shell_quote(word: &str) -> String {
    let safe = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&b));
    if safe {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r#"'"'"'"#))
    }
}

/// `shlex.join`
#[must_use]
pub fn shell_join(words: &[&str]) -> String {
    words.iter().map(|word| shell_quote(word)).collect::<Vec<_>>().join(" ")
}

/// `guest._complete_guest_exec`: run `/bin/sh -lc COMMAND`, poll
/// `exec-status` every 0.2 s until the command has exited or `timeout` has
/// passed, and check the answer. `check` controls only actual non-zero guest
/// exits. Invocation/observation failures always retain their typed PVE cause.
pub async fn guest_exec(
    pve: &impl Pve,
    node: &str,
    vmid: u32,
    command: &str,
    check: bool,
    timeout: Duration,
) -> Result<ExecResult, CoreError> {
    guest_exec_with_input(pve, node, vmid, command, None, check, timeout).await
}

/// [`guest_exec`] with `input` on the command's standard input
/// (`input-data`), e.g. a LUKS passphrase for `cryptsetup --key-file -`
/// (decision 6). The input is never part of an error message.
pub async fn guest_exec_with_input(
    pve: &impl Pve,
    node: &str,
    vmid: u32,
    command: &str,
    input: Option<&str>,
    check: bool,
    timeout: Duration,
) -> Result<ExecResult, CoreError> {
    let context = format!("VM {vmid} on {node}");
    let deadline = Instant::now() + timeout;
    let failed_call = |what: &str, error: PveError| -> Result<ExecResult, CoreError> {
        Err(CoreError::Context {
            message: format!("QEMU Guest Agent {what} failed for {context}"),
            causes: vec![error.into()],
        })
    };
    let mut form = vec![("command", "/bin/sh"), ("command", "-lc"), ("command", command)];
    if let Some(input) = input {
        form.push(("input-data", input));
    }
    let mut parsed = match pve.post(&agent(node, vmid, "exec"), &form).await {
        Ok(parsed) => parsed,
        Err(error) => return failed_call("exec", error),
    };
    if parsed.get("pid").is_some() && !truthy(parsed.get("exited")) {
        let pid = integer(parsed.get("pid")).ok_or_else(|| {
            CoreError::Pve(PveError::Response(format!(
                "QEMU Guest Agent returned an invalid pid for {context}"
            )))
        })?;
        let pid = pid.to_string();
        loop {
            if Instant::now() >= deadline {
                return Err(CoreError::Failed(format!(
                    "Guest command did not finish in {context} within {}s: {command}",
                    timeout.as_secs()
                )));
            }
            parsed = match pve.get(&agent(node, vmid, "exec-status"), &[("pid", &pid)]).await {
                Ok(status) => status,
                Err(error) => return failed_call("status", error),
            };
            if truthy(parsed.get("exited")) {
                break;
            }
            tokio::time::sleep(EXEC_POLL).await;
        }
    }
    if !truthy(parsed.get("exited")) || parsed.get("exitcode").is_none() {
        return Err(PveError::Response(format!(
            "QEMU Guest Agent returned an incomplete response for {context}"
        ))
        .into());
    }
    let exitcode = integer(parsed.get("exitcode")).ok_or_else(|| {
        CoreError::Pve(PveError::Response(format!(
            "QEMU Guest Agent returned an invalid exit code for {context}"
        )))
    })?;
    let result = ExecResult {
        exited: true,
        exitcode,
        out: text(parsed.get("out-data")),
        err: text(parsed.get("err-data")),
    };
    if check && exitcode != 0 {
        let mut message = format!("Guest command failed in {context} (exit={exitcode}): {command}");
        let detail = [result.err.trim(), result.out.trim()]
            .into_iter()
            .find(|part| !part.is_empty());
        if let Some(detail) = detail {
            message.push('\n');
            message.push_str(detail);
        }
        return Err(CoreError::Failed(message));
    }
    Ok(result)
}

/// `guest.guest_out_on_node`: the command's output, stripped.
pub async fn guest_out(
    pve: &impl Pve,
    node: &str,
    vmid: u32,
    command: &str,
    check: bool,
    timeout: Duration,
) -> Result<String, CoreError> {
    let result = guest_exec(pve, node, vmid, command, check, timeout).await?;
    Ok(result.out.trim().to_owned())
}

/// `agent/<command>` with an object answer; `result` of it.
pub async fn agent_get(pve: &impl Pve, node: &str, vmid: u32, command: &str) -> Result<Value, CoreError> {
    let answer = pve.get(&agent(node, vmid, command), &[]).await?;
    Ok(answer.get("result").cloned().unwrap_or(Value::Null))
}

/// Whether a `file-read` error is the guest's (no such file, a directory,
/// unreadable), as opposed to Proxmox's or the agent's.
fn guest_file_error(error: &PveError) -> bool {
    matches!(error, PveError::Api { status: 500, message }
        if message.starts_with("Agent error: failed to open file")
            || message.starts_with("Agent error: failed to read file"))
}

/// `agent/file-read` of `count` bytes from `offset` (`VM.GuestAgent.FileRead`):
/// the bytes, or `None` when the guest cannot open or read the path.
pub async fn read_file(
    pve: &impl Pve,
    node: &str,
    vmid: u32,
    path: &str,
    offset: u64,
    count: u64,
) -> Result<Option<Vec<u8>>, CoreError> {
    let offset = offset.to_string();
    let count = count.to_string();
    let query = [("file", path), ("offset", &offset), ("count", &count), ("decode", "0")];
    match pve.get(&agent(node, vmid, "file-read"), &query).await {
        Ok(answer) => {
            let content = answer.get("content").and_then(Value::as_str).unwrap_or_default();
            base64::engine::general_purpose::STANDARD
                .decode(content)
                .map(Some)
                .map_err(|_| CoreError::Unusable(format!("file-read of {path} in VM {vmid} returned invalid base64")))
        }
        Err(error) if guest_file_error(&error) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Why the guest could not read a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileError {
    /// No such file or directory.
    Missing,
    /// Any other reason, as the agent gave it.
    Unreadable(String),
}

/// The first [`FILE_LIMIT`] bytes of a file as text, or why the guest could
/// not read it (Python: `open()` raising `FileNotFoundError` or another
/// `OSError`).
pub async fn read_text_or_reason(
    pve: &impl Pve,
    node: &str,
    vmid: u32,
    path: &str,
) -> Result<Result<String, FileError>, CoreError> {
    let count = FILE_LIMIT.to_string();
    let query = [
        ("file", path),
        ("offset", "0"),
        ("count", count.as_str()),
        ("decode", "0"),
    ];
    match pve.get(&agent(node, vmid, "file-read"), &query).await {
        Ok(answer) => {
            let content = answer.get("content").and_then(Value::as_str).unwrap_or_default();
            let bytes = base64::engine::general_purpose::STANDARD.decode(content).map_err(|_| {
                CoreError::Unusable(format!("file-read of {path} in VM {vmid} returned invalid base64"))
            })?;
            Ok(Ok(String::from_utf8_lossy(&bytes).into_owned()))
        }
        Err(error) if guest_file_error(&error) => {
            let message = error.to_string();
            if message.contains("No such file or directory") {
                Ok(Err(FileError::Missing))
            } else {
                Ok(Err(FileError::Unreadable(message)))
            }
        }
        Err(error) => Err(error.into()),
    }
}

/// Files read whole are read up to this size.
pub const FILE_LIMIT: u64 = 1024 * 1024;

/// A text file, or `None` when it cannot be read.
pub async fn read_text(pve: &impl Pve, node: &str, vmid: u32, path: &str) -> Result<Option<String>, CoreError> {
    Ok(read_file(pve, node, vmid, path, 0, FILE_LIMIT)
        .await?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
}

/// The label of the ext4 filesystem on `device` (`blkid -s LABEL`), read
/// from its superblock: magic `0xEF53` at byte 56 and a NUL-padded label of
/// 16 bytes at byte 120 of the block at offset 1024. `None` when the device
/// cannot be read or holds no ext4.
pub async fn ext4_label(pve: &impl Pve, node: &str, vmid: u32, device: &str) -> Result<Option<String>, CoreError> {
    let Some(block) = read_file(pve, node, vmid, device, 1024, 256).await? else {
        return Ok(None);
    };
    if block.get(56..58) != Some(&[0x53, 0xEF][..]) {
        return Ok(None);
    }
    let label = block.get(120..136).unwrap_or_default();
    let end = label.iter().position(|byte| *byte == 0).unwrap_or(label.len());
    Ok(Some(String::from_utf8_lossy(&label[..end]).into_owned()))
}

/// The directories `command -v` searches in a login shell as root on the
/// Ubuntu guests (measured on Gold: `/usr/local/sbin:/usr/local/bin:
/// /usr/sbin:/usr/bin`), plus `/sbin` and `/bin`.
pub const PATH_DIRS: [&str; 6] = [
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
];

/// `command -v NAME` through `file-read`: the first readable file of that
/// name on the `PATH`.
pub async fn find_program(pve: &impl Pve, node: &str, vmid: u32, name: &str) -> Result<Option<String>, CoreError> {
    for dir in PATH_DIRS {
        let path = format!("{dir}/{name}");
        if read_file(pve, node, vmid, &path, 0, 1).await?.is_some() {
            return Ok(Some(path));
        }
    }
    Ok(None)
}
