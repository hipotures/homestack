use crate::model::*;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};
use tokio_util::sync::CancellationToken;

pub fn directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if path.exists() {
        let m = std::fs::symlink_metadata(path)
            .map_err(|_| Failure::new("storage_failure", "Cannot inspect storage directory"))?;
        if !m.is_dir() || m.file_type().is_symlink() {
            return Err(Failure::new(
                "storage_failure",
                "Storage paths must be real directories",
            ));
        }
    } else {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|_| Failure::new("storage_failure", "Cannot create storage directory"))?;
    }
    Ok(())
}
pub fn profile_dir(root: &Path, id: &str) -> Result<PathBuf> {
    let uuid = uuid::Uuid::parse_str(id).map_err(|_| Failure::new("invalid_id", "Invalid profile ID"))?;
    if uuid.to_string() != id {
        return Err(Failure::new("invalid_id", "Invalid profile ID"));
    }
    let path = root.join("profiles").join(id);
    if path.exists() {
        directory(&path)?;
    }
    Ok(path)
}
pub fn command(binary: &Path, profile: &Path, provider: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(binary);
    c.env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", profile.join("home"))
        .env("XDG_CONFIG_HOME", profile.join("home/.config"))
        .env("XDG_CACHE_HOME", profile.join("home/.cache"))
        .env("XDG_DATA_HOME", profile.join("home/.local/share"))
        .env("TMPDIR", profile.join("tmp"))
        .env("LANG", "C.UTF-8")
        .env("TERM", "dumb")
        .env("CODEX_HOME", profile.join("codex"))
        .env("CLAUDE_CONFIG_DIR", profile.join("claude"))
        .env("DISABLE_AUTOUPDATER", "1")
        .env("DISABLE_UPDATES", "1")
        .env("ENABLE_CLAUDEAI_MCP_SERVERS", "false")
        .env("CLAUDE_CODE_AUTO_CONNECT_IDE", "0")
        .env("CLAUDE_CODE_IDE_SKIP_AUTO_INSTALL", "1")
        .env("USE_BUILTIN_RIPGREP", "0")
        .current_dir(profile.join("work"))
        .kill_on_drop(true)
        .process_group(0);
    if provider == "codex" {
        c.arg("-c")
            .arg("check_for_update_on_startup=false")
            .arg("-c")
            .arg("cli_auth_credentials_store=\"file\"");
    }
    c
}
pub fn prepare(root: &Path, id: &str) -> Result<PathBuf> {
    let p = profile_dir(root, id)?;
    for sub in [
        "",
        "home",
        "home/.config",
        "home/.cache",
        "home/.local/share",
        "codex",
        "claude",
        "work",
        "tmp",
    ] {
        directory(&p.join(sub))?;
    }
    Ok(p)
}

pub struct Process {
    pub child: Child,
    pub input: ChildStdin,
    output: BufReader<ChildStdout>,
    sequence: u64,
}
impl Drop for Process {
    fn drop(&mut self) {
        self.kill_group();
    }
}
impl Process {
    pub fn spawn(mut c: tokio::process::Command) -> Result<Self> {
        let mut child = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| Failure::new("runtime_failure", "Cannot start the managed runtime"))?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| Failure::new("runtime_failure", "Runtime input unavailable"))?;
        let output = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| Failure::new("runtime_failure", "Runtime output unavailable"))?,
        );
        Ok(Self {
            child,
            input,
            output,
            sequence: 0,
        })
    }
    fn kill_group(&self) {
        if let Some(id) = self.child.id().and_then(|id| rustix::process::Pid::from_raw(id as i32)) {
            let _ = rustix::process::kill_process_group(id, rustix::process::Signal::KILL);
        }
    }
    pub async fn finish(&mut self) {
        self.kill_group();
        let _ = self.child.wait().await;
    }
    pub async fn send(&mut self, value: &Value) -> Result<()> {
        let mut bytes =
            serde_json::to_vec(value).map_err(|_| Failure::new("protocol_failure", "Cannot encode runtime request"))?;
        bytes.push(b'\n');
        self.input
            .write_all(&bytes)
            .await
            .map_err(|_| Failure::new("runtime_failure", "Runtime input closed"))
    }
    pub async fn read(&mut self) -> Result<Value> {
        // Bound a single line without allowing read_line to allocate without limit.
        let mut bytes = Vec::new();
        loop {
            let buf = self
                .output
                .fill_buf()
                .await
                .map_err(|_| Failure::new("runtime_failure", "Runtime output failed"))?;
            if buf.is_empty() {
                return Err(Failure::new(
                    "runtime_failure",
                    "Runtime exited before returning a result",
                ));
            }
            let take = buf.iter().position(|b| *b == b'\n').map_or(buf.len(), |i| i + 1);
            if bytes.len() + take > 512 * 1024 {
                return Err(Failure::new("protocol_failure", "Runtime response exceeds size limit"));
            }
            bytes.extend_from_slice(&buf[..take]);
            self.output.consume(take);
            if bytes.last() == Some(&b'\n') {
                break;
            }
        }
        serde_json::from_slice(&bytes).map_err(|_| Failure::new("protocol_failure", "Invalid runtime JSON response"))
    }
    pub async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        self.sequence += 1;
        let id = self.sequence;
        self.send(&serde_json::json!({"id":id,"method":method,"params":params}))
            .await?;
        loop {
            let value = self.read().await?;
            if value["id"].as_u64() == Some(id) {
                if value.get("error").is_some() {
                    let msg = value["error"]["message"].as_str().unwrap_or("").to_lowercase();
                    return Err(if value["error"]["code"].as_i64() == Some(-32601) {
                        Failure::unsupported()
                    } else if msg.contains("auth") || msg.contains("login") || msg.contains("401") {
                        Failure::new("reauthentication_required", "Provider requires authentication")
                    } else {
                        Failure::new("provider_api_failure", "Provider could not complete the request")
                    });
                }
                return value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| Failure::new("protocol_failure", "Runtime response has no result"));
            }
        }
    }
}
pub async fn bounded<T>(
    cancel: &CancellationToken,
    seconds: u64,
    f: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        _=cancel.cancelled()=>Err(Failure::new("cancelled","Operation cancelled")),
        result=tokio::time::timeout(Duration::from_secs(seconds),f)=>result.unwrap_or_else(|_|Err(Failure::new("probe_timeout","Provider operation timed out")))
    }
}
