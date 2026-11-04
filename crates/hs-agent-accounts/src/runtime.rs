//! Shared, pinned vendor installations. Candidate verification never changes the active pin.
use crate::{
    app::{Context, Job},
    model::*,
    process,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::{Arc, PoisonError},
    time::Duration,
};
use tokio::io::AsyncWriteExt;

fn failure(message: &str) -> Failure {
    Failure::new("runtime_verification_failure", message)
}
fn version(value: &str) -> bool {
    value.len() <= 32
        && value.split('.').count() == 3
        && value
            .split('.')
            .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}
fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("homestack-agent-accounts")
        .timeout(Duration::from_secs(180))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| failure("Cannot initialize runtime download client"))
}
async fn response(url: &str) -> Result<reqwest::Response> {
    client()?
        .get(url)
        .send()
        .await
        .map_err(|_| Failure::new("network_failure", "Cannot contact the official runtime release service"))?
        .error_for_status()
        .map_err(|_| failure("Official runtime release service rejected the requested version"))
}
async fn text(url: &str) -> Result<String> {
    let response = response(url).await?;
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| failure("Release metadata download failed"))?;
        if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
            return Err(failure("Release metadata is too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| failure("Release metadata is not UTF-8"))
}
async fn metadata(provider: &str, selected: Option<&str>) -> Result<Value> {
    match provider {
        "antigravity" => {
            let tail = selected.map_or("latest".into(), |v| format!("tags/{v}"));
            serde_json::from_str(
                &text(&format!(
                    "https://api.github.com/repos/google-antigravity/antigravity-cli/releases/{tail}"
                ))
                .await?,
            )
            .map_err(|_| failure("Invalid Antigravity release metadata"))
        }
        "codex" => {
            let tail = selected.map_or("latest".into(), |v| format!("tags/rust-v{v}"));
            serde_json::from_str(&text(&format!("https://api.github.com/repos/openai/codex/releases/{tail}")).await?)
                .map_err(|_| failure("Invalid Codex release metadata"))
        }
        "claude" => {
            let selected = match selected {
                Some(v) => v.to_owned(),
                None => text("https://downloads.claude.ai/claude-code-releases/latest")
                    .await?
                    .trim()
                    .to_owned(),
            };
            if !version(&selected) {
                return Err(failure("Invalid Claude release version"));
            }
            let mut value: Value = serde_json::from_str(
                &text(&format!(
                    "https://downloads.claude.ai/claude-code-releases/{selected}/manifest.json"
                ))
                .await?,
            )
            .map_err(|_| failure("Invalid Claude release manifest"))?;
            value["version"] = json!(selected);
            Ok(value)
        }
        _ => Err(Failure::unsupported()),
    }
}
fn release_version(provider: &str, value: &Value) -> Result<String> {
    let v = if provider == "codex" {
        value["tag_name"].as_str().and_then(|s| s.strip_prefix("rust-v"))
    } else if provider == "antigravity" {
        value["tag_name"].as_str()
    } else {
        value["version"].as_str()
    }
    .ok_or_else(|| failure("Release has no stable version"))?;
    if !version(v) || value["prerelease"] == true || value["draft"] == true {
        return Err(failure("Only stable runtime releases are supported"));
    }
    Ok(v.into())
}
async fn download(url: &str, path: &Path, expected: &str) -> Result<()> {
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(failure("Official release has no valid SHA-256 digest"));
    }
    let mut stream = response(url).await?.bytes_stream();
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|_| failure("Cannot create runtime staging file"))?;
    let mut digest = Sha256::new();
    let mut count = 0usize;
    while let Some(bytes) = stream.next().await {
        let bytes = bytes.map_err(|_| Failure::new("network_failure", "Runtime download was interrupted"))?;
        count += bytes.len();
        if count > 512 * 1024 * 1024 {
            return Err(failure("Runtime download exceeds the size limit"));
        }
        digest.update(&bytes);
        file.write_all(&bytes)
            .await
            .map_err(|_| failure("Cannot write downloaded runtime"))?;
    }
    file.sync_all()
        .await
        .map_err(|_| failure("Cannot flush downloaded runtime"))?;
    if digest.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>() != expected.to_lowercase() {
        return Err(failure("Runtime SHA-256 verification failed"));
    }
    Ok(())
}
async fn candidate(ctx: &Context, provider: &str, selected: &str, stage: &Path) -> Result<std::path::PathBuf> {
    let data = metadata(provider, Some(selected)).await?;
    if release_version(provider, &data)? != selected {
        return Err(failure("Downloaded release does not match the selected version"));
    }
    let path = stage.join(provider);
    if matches!(provider, "codex" | "antigravity") {
        let asset_name = if provider == "codex" {
            "codex-x86_64-unknown-linux-musl.tar.gz"
        } else {
            "agy_cli_linux_x64_musl.tar.gz"
        };
        let extracted = if provider == "codex" {
            "codex-x86_64-unknown-linux-musl"
        } else {
            "antigravity"
        };
        let asset = data["assets"]
            .as_array()
            .and_then(|a| a.iter().find(|v| v["name"] == asset_name))
            .ok_or_else(|| failure("Release has no official musl archive"))?;
        let digest = asset["digest"]
            .as_str()
            .and_then(|s| s.strip_prefix("sha256:"))
            .ok_or_else(|| failure("Runtime archive digest is unavailable"))?;
        let url = if provider == "codex" {
            format!("https://github.com/openai/codex/releases/download/rust-v{selected}/{asset_name}")
        } else {
            format!("https://github.com/google-antigravity/antigravity-cli/releases/download/{selected}/{asset_name}")
        };
        let archive = stage.join("runtime.tar.gz");
        download(&url, &archive, digest).await?;
        let mut command = tokio::process::Command::new("/bin/tar");
        command
            .env_clear()
            .args(["-xzf"])
            .arg(&archive)
            .arg("-C")
            .arg(stage)
            .arg(extracted)
            .kill_on_drop(true)
            .process_group(0);
        let mut child = process::Process::spawn(command)?;
        let status = child
            .child
            .wait()
            .await
            .map_err(|_| failure("Cannot extract runtime archive"))?;
        child.finish().await;
        if !status.success() {
            return Err(failure("Runtime archive extraction failed"));
        }
        std::fs::rename(stage.join(extracted), &path).map_err(|_| failure("Cannot stage runtime binary"))?;
        std::fs::remove_file(archive).map_err(|_| failure("Cannot remove downloaded archive"))?;
    } else {
        let digest = data["platforms"]["linux-x64-musl"]["checksum"]
            .as_str()
            .ok_or_else(|| failure("Claude musl digest is unavailable"))?;
        download(
            &format!("https://downloads.claude.ai/claude-code-releases/{selected}/linux-x64-musl/claude"),
            &path,
            digest,
        )
        .await?;
    }
    use std::os::unix::fs::PermissionsExt;
    if !path.is_file() || path.is_symlink() {
        return Err(failure("Downloaded runtime is not a regular executable"));
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| failure("Cannot set runtime permissions"))?;
    verify_version(ctx, provider, &path, selected).await?;
    Ok(path)
}

pub async fn verify_version(ctx: &Context, provider: &str, path: &Path, selected: &str) -> Result<()> {
    let home = ctx.root.join("state/runtime-check");
    for sub in ["", "home", "work", "tmp"] {
        process::directory(&home.join(sub))?;
    }
    let mut command = process::command(path, &home, provider);
    command.arg("--version");
    let mut child = process::Process::spawn(command)?;
    let mut output = Vec::new();
    loop {
        let bytes = child.chunk().await?;
        if bytes.is_empty() {
            break;
        }
        if output.len() + bytes.len() > 4096 {
            return Err(failure("Runtime version output exceeds size limit"));
        }
        output.extend(bytes);
    }
    let status = child
        .child
        .wait()
        .await
        .map_err(|_| failure("Runtime version check failed"))?;
    child.finish().await;
    if !status.success()
        || !String::from_utf8_lossy(&output)
            .split_whitespace()
            .any(|v| v == selected)
    {
        return Err(failure("Runtime did not report the expected version"));
    }
    Ok(())
}

impl Context {
    pub fn runtime_operation(self: &Arc<Self>, provider: &str, selected: Option<String>) -> Result<Operation> {
        if !self.provider(provider)?.capabilities().contains(&"runtime_updates") {
            return Err(Failure::unsupported());
        }
        if selected.as_ref().is_some_and(|v| !version(v)) {
            return Err(Failure::new(
                "invalid_input",
                "Select an explicit numeric runtime version",
            ));
        }
        let kind = if selected.is_some() { "upgrade" } else { "update_check" };
        let mut jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        if jobs
            .values()
            .any(|j| j.value.provider == provider && !j.value.terminal())
        {
            return Err(Failure::new(
                "busy",
                "Wait for this provider's current operation to finish",
            ));
        }
        jobs.retain(|_, j| !j.value.terminal() || now() - j.value.finished_at.unwrap_or(now()) < 600);
        if jobs.len() >= 256 || jobs.values().filter(|j| !j.value.terminal()).count() >= 32 {
            return Err(Failure::new("busy", "Operation queue is full"));
        }
        let op = Operation {
            id: uuid::Uuid::new_v4().to_string(),
            profile_id: None,
            provider: provider.into(),
            kind: kind.into(),
            status: "queued".into(),
            message: "Waiting to manage provider runtime".into(),
            authorization_url: None,
            user_code: None,
            accepts_code: false,
            error: None,
            started_at: now(),
            finished_at: None,
            result: None,
        };
        let cancel = self.shutdown.child_token();
        let (codes, _) = tokio::sync::mpsc::channel(1);
        jobs.insert(
            op.id.clone(),
            Job {
                value: op.clone(),
                cancel: cancel.clone(),
                codes,
            },
        );
        drop(jobs);
        if let Err(e) = self.db.operation(&op) {
            self.jobs.lock().unwrap_or_else(PoisonError::into_inner).remove(&op.id);
            return Err(e);
        }
        let ctx = self.clone();
        let id = op.id.clone();
        let provider = provider.to_owned();
        tokio::spawn(async move {
            let stage = ctx.root.join("state").join(format!("runtime-{}", uuid::Uuid::new_v4()));
            let result=process::bounded(&cancel,600,async {
                let gate=ctx.gates.get(&provider).ok_or_else(Failure::unsupported)?;let _exclusive=gate.write().await;
                let _slot=ctx.slots.acquire().await.map_err(|_|failure("Service is stopping"))?;
                let mut runtime=ctx.db.runtime(&provider)?.ok_or_else(Failure::unsupported)?;
                ctx.update_operation(&id,|o|{o.status="running".into();o.message=if selected.is_some(){"Preparing a verified runtime candidate"}else{"Checking official stable releases"}.into();});
                if let Some(selected)=&selected {
                    // Fail before downloading if no real account can verify this candidate.
                    let p=ctx.db.profiles()?.into_iter().find(|p|p.provider==provider&&p.enabled&&!p.deleting&&p.auth_status=="authenticated").ok_or_else(||Failure::new("verification_blocked","Enable and authenticate a profile before upgrading; the previous pin is retained"))?;
                    if *selected==runtime.version {return Err(Failure::new("invalid_input","This runtime version is already pinned"));}
                    let lock=ctx.lock(&p.id);let _profile=lock.lock().await;
                    let p=ctx.db.profile(&p.id)?;
                    if !p.enabled||p.deleting||p.auth_status!="authenticated" {return Err(Failure::new("verification_blocked","The verification profile is no longer available"));}
                    process::directory(&stage)?;
                    let binary=candidate(&ctx,&provider,selected,&stage).await?;
                    ctx.update_operation(&id,|o|o.message="Candidate starts correctly; verifying real account limits".into());
                    let snapshot=process::bounded(&cancel,60,ctx.provider(&provider)?.probe(&ctx,&p,Some(&binary))).await?;
                    if cancel.is_cancelled() {return Err(Failure::new("cancelled","Upgrade cancelled before activation"));}
                    let parent=ctx.root.join("runtimes").join(&provider);process::directory(&parent)?;
                    let destination=parent.join(selected);
                    if destination.exists() {return Err(failure("Candidate version directory already exists; previous pin retained"));}
                    std::fs::rename(&stage,&destination).map_err(|_|failure("Cannot activate verified runtime installation"))?;
                    runtime.previous_version=Some(runtime.version.clone());runtime.version=selected.clone();runtime.verified_at=Some(now());runtime.last_error=None;runtime.last_result=Some("Upgrade verified with a real account-limits probe".into());
                    // One database transaction publishes both the new pin and its real observation.
                    ctx.db.activate_runtime(&runtime,&p,&snapshot)?;
                    // Retain the active and previous working installation, after activation only.
                    if let Ok(entries) = std::fs::read_dir(&parent) {
                        for entry in entries.flatten() {
                            let name = entry.file_name().to_string_lossy().into_owned();
                            if version(&name) && name != runtime.version && runtime.previous_version.as_deref() != Some(&name)
                                && entry.file_type().is_ok_and(|t|t.is_dir()&&!t.is_symlink())
                                && std::fs::remove_dir_all(entry.path()).is_err() {
                                eprintln!("Runtime retirement failed; keeping an older installation");
                            }
                        }
                    }
                    Ok(json!({"version":runtime.version,"verified_at":runtime.verified_at,"previous_version":runtime.previous_version}))
                } else {
                    let data=metadata(&provider,None).await?;runtime.latest_version=Some(release_version(&provider,&data)?);runtime.checked_at=Some(now());ctx.db.save_runtime(&runtime)?;Ok(json!({"latest_version":runtime.latest_version,"checked_at":runtime.checked_at}))
                }
            }).await;
            if stage.exists() {
                let _ = tokio::fs::remove_dir_all(&stage).await;
            }
            if let Err(e) = &result
                && selected.is_some()
                && let Ok(Some(mut runtime)) = ctx.db.runtime(&provider)
            {
                runtime.last_error = Some(e.clone());
                runtime.last_result = Some(format!("Upgrade not activated; previous pin retained. {}", e.message));
                let _ = ctx.db.save_runtime(&runtime);
            }
            ctx.update_operation(&id, |o| {
                o.finished_at = Some(now());
                match &result {
                    Ok(value) => {
                        o.status = "succeeded".into();
                        o.message = if selected.is_some() {
                            "Upgrade activated after account-limits verification"
                        } else {
                            "Available version checked; nothing installed"
                        }
                        .into();
                        o.result = Some(value.clone());
                    }
                    Err(e) => {
                        o.status = if selected.is_some() { "rolled_back" } else { "failed" }.into();
                        o.message = if selected.is_some() {
                            format!("Previous runtime pin retained. {}", e.message)
                        } else {
                            e.message.clone()
                        };
                        o.error = Some(e.clone());
                    }
                }
            });
            if let Ok(op) = ctx.operation(&id) {
                let _ = ctx.db.operation(&op);
            }
        });
        Ok(op)
    }
}
