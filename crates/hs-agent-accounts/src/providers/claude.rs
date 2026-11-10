use super::*;
use crate::process::{self, Process};
use tokio::io::AsyncWriteExt;
pub struct Claude;

async fn identity(ctx: &Context, p: &Profile, binary: &Path) -> Result<Account> {
    let root = process::prepare(&ctx.root, &p.id)?;
    let mut command = process::command(binary, &root, "claude");
    command.args(["auth", "status", "--json"]);
    let mut child = Process::spawn(command)?;
    let result = child.document().await;
    child.finish().await;
    let value = result?;
    if value["configDirectory"].as_str() != root.join("claude").to_str() {
        return Err(Failure::new(
            "profile_isolation_failure",
            "Claude selected an unexpected configuration directory",
        ));
    }
    if value["loggedIn"] != true {
        return Err(Failure::new(
            "reauthentication_required",
            "Sign in to this Claude profile",
        ));
    }
    if value["authMethod"].as_str() != Some("claude.ai") {
        return Err(Failure::new(
            "unsupported_capability",
            "Claude subscription monitoring requires a claude.ai login",
        ));
    }
    Ok(Account {
        provider_id: value["orgId"].as_str().map(str::to_owned),
        email: value["email"].as_str().map(str::to_owned),
        plan: value["subscriptionType"].as_str().map(str::to_owned),
        period_start: None,
        period_end: None,
    })
}
fn timestamp(v: &Value) -> Option<i64> {
    v.as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp())
}
fn percent(v: &Value) -> Option<f64> {
    v.as_f64().filter(|v| v.is_finite() && *v >= 0.0)
}
fn window(id: &str, label: &str, minutes: Option<i64>, v: &Value, model: Option<String>) -> Limit {
    Limit {
        id: id.into(),
        label: label.into(),
        model,
        window_minutes: minutes,
        used_percent: percent(&v["utilization"]),
        resets_at: timestamp(&v["resets_at"]),
        exhausted: None,
    }
}
/// Runs one SDK helper request: `usage` reads quota windows without a model turn, `start` sends one minimal turn.
async fn helper(ctx: &Context, p: &Profile, binary: &Path, mode: &str) -> Result<Value> {
    let root = process::prepare(&ctx.root, &p.id)?;
    let mut command = process::command(Path::new("/usr/bin/node"), &root, "claude");
    command.arg(
        std::env::var("AGENT_ACCOUNTS_CLAUDE_HELPER")
            .unwrap_or_else(|_| "/opt/agent-accounts/claude-probe/probe.mjs".into()),
    );
    let mut child = Process::spawn(command)?;
    let request = uuid::Uuid::new_v4().to_string();
    let result = async {
        child
            .send(&json!({"version":1,"id":request,"binary":binary,"mode":mode}))
            .await?;
        let result = child.read().await?;
        if result["version"] != 1 || result["id"] != request {
            return Err(Failure::new("protocol_failure", "Claude helper protocol mismatch"));
        }
        if result["ok"] != true {
            let code = match result["error"]["code"].as_str() {
                Some("reauthentication_required") => "reauthentication_required",
                Some("unsupported_capability") => "unsupported_capability",
                Some("network_failure") => "network_failure",
                Some("probe_timeout") => "probe_timeout",
                _ => "provider_api_failure",
            };
            let message = if mode == "start" {
                "Claude could not start the usage window"
            } else {
                "Claude SDK usage probe could not complete"
            };
            return Err(Failure::new(code, message));
        }
        Ok(result)
    }
    .await;
    child.finish().await;
    result
}
impl Provider for Claude {
    fn bundled_version(&self) -> Option<&'static str> {
        Some("2.1.318")
    }
    fn capabilities(&self) -> Vec<&'static str> {
        vec![
            "authentication",
            "account_information",
            "subscription_limits",
            "start_window",
            "runtime_version",
            "runtime_updates",
        ]
    }
    fn id(&self) -> &'static str {
        "claude"
    }
    fn probe<'a>(&'a self, ctx: &'a Context, p: &'a Profile, binary: Option<&'a Path>) -> ProbeFuture<'a, Snapshot> {
        Box::pin(async move {
            let binary = binary.ok_or_else(|| Failure::new("runtime_failure", "Managed runtime is unavailable"))?;
            let mut account = identity(ctx, p, binary).await?;
            let result = helper(ctx, p, binary, "usage").await?;
            {
                let data = &result["data"];
                if data["rate_limits_available"] != true {
                    return Err(Failure::new(
                        "unsupported_capability",
                        "Claude did not make subscription limits available for this login",
                    ));
                }
                let rates = &data["rate_limits"];
                if rates.is_null() {
                    return Err(Failure::new(
                        "provider_api_failure",
                        "Claude returned no current usage measurement",
                    ));
                }
                account.email = data["account"]["email"].as_str().map(str::to_owned).or(account.email);
                account.plan = data["account"]["plan"].as_str().map(str::to_owned).or(account.plan);
                let mut limits = Vec::new();
                for (id, label, minutes) in [
                    ("five_hour", "5-hour", 300),
                    ("seven_day", "7-day", 10080),
                    ("seven_day_oauth_apps", "7-day OAuth applications", 10080),
                    ("seven_day_opus", "7-day Opus", 10080),
                    ("seven_day_sonnet", "7-day Sonnet", 10080),
                ] {
                    if let Some(v) = rates.get(id).filter(|v| v.is_object()) {
                        limits.push(window(id, label, Some(minutes), v, None));
                    }
                }
                if let Some(scoped) = rates["model_scoped"].as_array() {
                    for (i, v) in scoped.iter().enumerate() {
                        if let Some(name) = v["display_name"].as_str() {
                            limits.push(window(
                                &format!("model_scoped/{i}"),
                                name,
                                Some(10080),
                                v,
                                Some(name.into()),
                            ));
                        }
                    }
                }
                let mut spending = None;
                if let Some(extra) = rates.get("extra_usage").filter(|v| v["is_enabled"] == true) {
                    let used = percent(&extra["used_credits"]);
                    let cap = percent(&extra["monthly_limit"]).filter(|v| *v > 0.0);
                    let pct = used
                        .zip(cap)
                        .map(|(u, l)| 100.0 * u / l)
                        .or_else(|| percent(&extra["utilization"]));
                    limits.push(Limit {
                        id: "monthly_spend".into(),
                        label: "Monthly spending".into(),
                        model: None,
                        window_minutes: None,
                        used_percent: pct,
                        resets_at: None,
                        exhausted: None,
                    });
                    spending =
                        Some(json!({"used_credits":used,"monthly_limit":cap,"currency":extra["currency"].as_str()}));
                }
                if limits.is_empty() {
                    return Err(Failure::new("provider_api_failure", "Claude returned no quota windows"));
                }
                let observed = result["observed_at"]
                    .as_i64()
                    .filter(|v| *v > 0 && *v <= now() + 60)
                    .ok_or_else(|| {
                        Failure::new("protocol_failure", "Claude returned an invalid observation timestamp")
                    })?;
                Ok(Snapshot {
                    scope: Scope::Account,
                    observed_at: observed,
                    processed_at: now(),
                    account,
                    limits,
                    credits: spending,
                    reset_credits: None,
                })
            }
        })
    }
    fn start_window<'a>(&'a self, ctx: &'a Context, p: &'a Profile, binary: &'a Path) -> ProbeFuture<'a, Value> {
        Box::pin(async move {
            identity(ctx, p, binary).await?;
            Ok(helper(ctx, p, binary, "start").await?["data"].take())
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
            let binary = ctx.binary("claude")?;
            let mut command = process::command(&binary, &root, "claude");
            command.args(["auth", "login", "--claudeai"]);
            let mut child = Process::spawn(command)?;
            let result=async {
            let mut output=String::new();
            loop {
                tokio::select! {
                    chunk=child.chunk()=>{
                        let chunk=chunk?;
                        if chunk.is_empty() {break;}
                        if output.len()+chunk.len()>64*1024 {return Err(Failure::new("protocol_failure","Claude authentication output exceeded the size limit"));}
                        output.push_str(&String::from_utf8_lossy(&chunk));
                        if let Some(start)=output.find("https://") {
                            let tail=&output[start..];
                            let end=tail.find(|c:char|c.is_whitespace()||c.is_control()).unwrap_or(tail.len());
                            let url=&tail[..end];
                            if let Ok(parsed)=reqwest::Url::parse(url)
                                && parsed.scheme()=="https" && matches!(parsed.host_str(),Some("claude.com"|"claude.ai"|"platform.claude.com"))
                                && parsed.query_pairs().any(|(k,v)|k=="code_challenge"&&!v.is_empty()) {
                                ctx.update_operation(&op,|o|{o.authorization_url=Some(url.into());o.status="waiting_for_authorization".into();o.message="Open the authorization page, then paste the returned code when prompted".into();o.accepts_code=output.contains("Paste code");});
                            }
                        }
                    },
                    code=codes.recv()=>{
                        let code=code.ok_or_else(||Failure::new("cancelled","Authentication input closed"))?;
                        child.input.write_all(format!("{code}\n").as_bytes()).await.map_err(|_|Failure::new("authentication_failed","Claude login input closed"))?;
                        ctx.update_operation(&op,|o|{o.accepts_code=false;o.message="Verifying authorization code".into();});
                    }
                }
            }
            let status=child.child.wait().await.map_err(|_|Failure::new("authentication_failed","Claude login process failed"))?;
            if !status.success() {return Err(Failure::new("authentication_failed","Claude sign-in failed or the authorization code was rejected"));}
            identity(&ctx,&p,&binary).await
        }.await;
            child.finish().await;
            result
        })
    }
}
