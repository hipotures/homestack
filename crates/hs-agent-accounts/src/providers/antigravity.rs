use super::*;
use crate::process::{self, Process};
use tokio::io::AsyncWriteExt;
pub struct Antigravity;
/// The CLI keeps a log, a conversation database and a brain directory for
/// every start and never removes them. Each run first clears what earlier runs
/// left, so a profile holds only the last run's state and one overwritten log.
const LOG: &str = "antigravity.log";
const PER_RUN: [&str; 3] = ["log", "conversations", "brain"];
fn command(binary: &Path, root: &Path) -> tokio::process::Command {
    let state = root.join("home/.gemini/antigravity-cli");
    for name in PER_RUN {
        let Ok(entries) = std::fs::read_dir(state.join(name)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let removed = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            if let Err(error) = removed {
                eprintln!("Cannot remove Antigravity {name} entry: {error}");
            }
        }
    }
    let mut command = process::command(binary, root, "antigravity");
    command.arg("--log-file").arg(root.join(LOG));
    command
}
async fn identity(ctx: &Context, p: &Profile, binary: &Path) -> Result<Account> {
    let root = process::prepare(&ctx.root, &p.id)?;
    let mut command = command(binary, &root);
    command.arg("acp");
    let mut child = Process::spawn(command)?;
    let result = async {
        child
            .rpc("initialize", json!({"protocolVersion":1,"clientCapabilities":{}}))
            .await?;
        let data = child
            .rpc("session/new", json!({"cwd":root.join("work"),"mcpServers":[]}))
            .await?;
        Ok(Account {
            provider_id: None,
            email: None,
            plan: data["_meta"]["antigravity"]["tier"]["name"].as_str().map(str::to_owned),
            period_start: None,
            period_end: None,
        })
    }
    .await;
    child.finish().await;
    result
}
impl Provider for Antigravity {
    fn id(&self) -> &'static str {
        "antigravity"
    }
    fn bundled_version(&self) -> Option<&'static str> {
        Some("1.5.0")
    }
    fn capabilities(&self) -> Vec<&'static str> {
        vec![
            "authentication",
            "account_information",
            "subscription_limits",
            "runtime_version",
            "runtime_updates",
        ]
    }
    fn probe<'a>(&'a self, ctx: &'a Context, p: &'a Profile, binary: Option<&'a Path>) -> ProbeFuture<'a, Snapshot> {
        Box::pin(async move {
            let binary = binary.ok_or_else(|| Failure::new("runtime_failure", "Managed runtime is unavailable"))?;
            let account = identity(ctx, p, binary).await?;
            let root = process::prepare(&ctx.root, &p.id)?;
            let mut command = command(binary, &root);
            command.args(["-p", "/usage", "--output-format", "json"]);
            let mut child = Process::spawn(command)?;
            let result = child.document().await;
            child.finish().await;
            let data = result?;
            let groups = data["command"]["data"]["groups"].as_array().ok_or_else(|| {
                Failure::new(
                    "reauthentication_required",
                    "Antigravity returned no quota groups; sign in to this profile",
                )
            })?;
            let mut limits = Vec::new();
            for group in groups {
                let name = group["name"]
                    .as_str()
                    .ok_or_else(|| Failure::new("protocol_failure", "Missing quota group name"))?;
                if let Some(buckets) = group["buckets"].as_array() {
                    for bucket in buckets {
                        let id = bucket["id"]
                            .as_str()
                            .ok_or_else(|| Failure::new("protocol_failure", "Missing quota bucket ID"))?;
                        let remaining = bucket["remaining_fraction"]
                            .as_f64()
                            .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                            .ok_or_else(|| Failure::new("protocol_failure", "Invalid remaining quota"))?;
                        limits.push(Limit {
                            id: id.into(),
                            label: name.into(),
                            model: Some(name.into()),
                            window_minutes: match bucket["window"].as_str() {
                                Some("weekly") => Some(10080),
                                Some("5h") => Some(300),
                                _ => None,
                            },
                            used_percent: Some((1.0 - remaining) * 100.0),
                            resets_at: bucket["reset_time"]
                                .as_str()
                                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                                .map(|t| t.timestamp()),
                            exhausted: (remaining == 0.0).then_some(true),
                        });
                    }
                }
            }
            if limits.is_empty() {
                return Err(Failure::new(
                    "provider_api_failure",
                    "Antigravity returned no quota windows",
                ));
            }
            Ok(Snapshot {
                scope: Scope::Account,
                observed_at: now(),
                processed_at: now(),
                account,
                limits,
                credits: None,
                reset_credits: None,
            })
        })
    }
    fn authenticate<'a>(
        &'a self,
        ctx: Arc<Context>,
        p: Profile,
        op: String,
        mut codes: tokio::sync::mpsc::Receiver<String>,
    ) -> ProbeFuture<'a, Account> {
        Box::pin(async move {
            let root = process::prepare(&ctx.root, &p.id)?;
            let binary = ctx.binary("antigravity")?;
            let mut command = process::command(Path::new("/usr/bin/script"), &root, "antigravity");
            // Expand the executable from the environment without interpolating its path into shell code.
            command.env("AGY_LOGIN_BINARY", &binary).args([
                "-q",
                "-e",
                "-f",
                "-E",
                "never",
                "-c",
                "exec \"$AGY_LOGIN_BINARY\" acp --login",
                "/dev/null",
            ]);
            let mut child = Process::spawn(command)?;
            let result=async {
                let mut output=String::new();
                loop {
                    tokio::select! {
                        chunk=child.chunk()=>{
                            let chunk=chunk?; if chunk.is_empty(){break;}
                            if output.len()+chunk.len()>65536{return Err(Failure::new("protocol_failure","Authentication output exceeded the size limit"));}
                            output.push_str(&String::from_utf8_lossy(&chunk));
                            if let Some(start)=output.find("https://accounts.google.com/") {
                                let tail=&output[start..]; let end=tail.find(|c:char|c.is_whitespace()||c.is_control()).unwrap_or(tail.len()); let url=&tail[..end];
                                if let Ok(parsed)=reqwest::Url::parse(url) && parsed.scheme()=="https" && parsed.host_str()==Some("accounts.google.com") && parsed.query_pairs().any(|(k,v)| k=="code_challenge"&&!v.is_empty()) {
                                    ctx.update_operation(&op,|o|{o.authorization_url=Some(url.into());o.status="waiting_for_authorization".into();o.message="Open Google sign-in, then paste the authorization code or final redirect URL".into();o.accepts_code=output.contains("Then paste");});
                                }
                            }
                        },
                        code=codes.recv()=>{
                            let code=code.ok_or_else(||Failure::new("cancelled","Authentication input closed"))?;
                            if code.contains(['\n','\r']){return Err(Failure::new("authentication_failed","Authorization input must be a single line"));}
                            child.input.write_all(format!("{code}\n").as_bytes()).await.map_err(|_|Failure::new("authentication_failed","Login input closed"))?;
                            ctx.update_operation(&op,|o|{o.accepts_code=false;o.message="Verifying Google authorization".into();});
                        }
                    }
                }
                if !child.child.wait().await.map_err(|_|Failure::new("authentication_failed","Login process failed"))?.success(){return Err(Failure::new("authentication_failed","Google sign-in failed"));}
                identity(&ctx,&p,&binary).await
            }.await;
            child.finish().await;
            result
        })
    }
}
