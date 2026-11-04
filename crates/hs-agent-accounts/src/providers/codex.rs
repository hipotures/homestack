use super::*;
use crate::{
    model::now,
    process::{self, Process},
};
use serde_json::json;
pub struct Codex;
async fn start(ctx: &Context, p: &Profile, binary: &Path) -> Result<Process> {
    let path = process::prepare(&ctx.root, &p.id)?;
    let mut command = process::command(binary, &path, "codex");
    command.arg("app-server").arg("--listen").arg("stdio://");
    let mut child = Process::spawn(command)?;
    child.rpc("initialize",json!({"clientInfo":{"name":"agent_accounts","title":"Agent Accounts","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await?;
    child.send(&json!({"method":"initialized"})).await?;
    Ok(child)
}
fn account(value: &Value) -> Result<Account> {
    if value["account"].is_null() {
        return Err(Failure::new(
            "reauthentication_required",
            "Sign in to this Codex profile",
        ));
    }
    if value["account"]["type"] != "chatgpt" {
        return Err(Failure::new(
            "unsupported_capability",
            "Subscription monitoring requires ChatGPT authentication",
        ));
    }
    Ok(Account {
        provider_id: None,
        email: value["account"]["email"].as_str().map(str::to_owned),
        plan: value["account"]["planType"].as_str().map(str::to_owned),
    })
}
impl Provider for Codex {
    fn bundled_version(&self) -> Option<&'static str> {
        Some("0.166.0")
    }
    fn id(&self) -> &'static str {
        "codex"
    }
    fn capabilities(&self) -> Vec<&'static str> {
        vec![
            "authentication",
            "account_information",
            "subscription_limits",
            "account_statistics",
            "runtime_version",
            "runtime_updates",
        ]
    }
    fn probe<'a>(&'a self, ctx: &'a Context, p: &'a Profile, binary: Option<&'a Path>) -> ProbeFuture<'a, Snapshot> {
        Box::pin(async move {
            let binary = binary.ok_or_else(|| Failure::new("runtime_failure", "Managed runtime is unavailable"))?;
            let mut child = start(ctx, p, binary).await?;
            let result = async {
                let mut identity = account(&child.rpc("account/read", json!({"refreshToken":false})).await?)?;
                let value = child.rpc("account/rateLimits/read", json!({})).await?;
                let observed = now();
                identity.provider_id = value["accountId"].as_str().map(str::to_owned);
                let buckets: Vec<(String, Value)> = match value["rateLimitsByLimitId"].as_object() {
                    Some(all) => all.iter().map(|(id, v)| (id.clone(), v.clone())).collect(),
                    None => vec![(
                        value["rateLimits"]["limitId"].as_str().unwrap_or("codex").into(),
                        value["rateLimits"].clone(),
                    )],
                };
                let mut limits = Vec::new();
                let mut credits = serde_json::Map::new();
                for (id, b) in buckets {
                    if identity.plan.is_none() {
                        identity.plan = b["planType"].as_str().map(str::to_owned);
                    }
                    if let Some(c) = b.get("credits").filter(|v| !v.is_null()) {
                        credits.insert(id.clone(), c.clone());
                    }
                    // The reached type names the bucket, not a window: a plain rate limit belongs to its
                    // full windows; credit or workspace exhaustion, or no full window, blocks them all.
                    let reached = b["rateLimitReachedType"].as_str();
                    let full = |win: &Value| win["usedPercent"].as_f64().is_some_and(|v| v >= 100.0);
                    let any_full = ["primary", "secondary"].iter().any(|w| full(&b[*w]));
                    for w in ["primary", "secondary"] {
                        let Some(win) = b.get(w).filter(|v| !v.is_null()) else {
                            continue;
                        };
                        let exhausted = reached.map(|kind| kind != "rate_limit_reached" || !any_full || full(win));
                        limits.push(Limit {
                            id: format!("{id}/{w}"),
                            label: b["limitName"].as_str().unwrap_or(&id).into(),
                            model: b["normalModelSlug"].as_str().map(str::to_owned),
                            window_minutes: win["windowDurationMins"].as_i64(),
                            used_percent: win["usedPercent"].as_f64().filter(|v| v.is_finite() && *v >= 0.0),
                            resets_at: win["resetsAt"].as_i64(),
                            exhausted: exhausted.filter(|e| *e),
                        });
                    }
                }
                if limits.is_empty() {
                    return Err(Failure::new("provider_api_failure", "Codex returned no quota windows"));
                }
                Ok(Snapshot {
                    scope: Scope::Account,
                    observed_at: observed,
                    processed_at: now(),
                    account: identity,
                    limits,
                    credits: (!credits.is_empty()).then_some(Value::Object(credits)),
                    reset_credits: value.get("rateLimitResetCredits").filter(|v| !v.is_null()).cloned(),
                })
            }
            .await;
            child.finish().await;
            result
        })
    }
    fn authenticate<'a>(
        &'a self,
        ctx: Arc<Context>,
        p: Profile,
        op: String,
        _codes: tokio::sync::mpsc::Receiver<String>,
    ) -> ProbeFuture<'a, Account> {
        Box::pin(async move {
            let binary = ctx.binary("codex")?;
            let mut child = start(&ctx, &p, &binary).await?;
            let result = async {
                let value = child
                    .rpc("account/login/start", json!({"type":"chatgptDeviceCode"}))
                    .await?;
                let url = value["verificationUrl"]
                    .as_str()
                    .ok_or_else(|| Failure::new("protocol_failure", "Codex did not return a verification URL"))?;
                let parsed = reqwest::Url::parse(url)
                    .map_err(|_| Failure::new("protocol_failure", "Invalid authorization URL"))?;
                if parsed.scheme() != "https" || parsed.host_str() != Some("auth.openai.com") {
                    return Err(Failure::new("protocol_failure", "Unexpected authorization host"));
                }
                let login = value["loginId"]
                    .as_str()
                    .ok_or_else(|| Failure::new("protocol_failure", "Missing login ID"))?
                    .to_owned();
                ctx.update_operation(&op, |o| {
                    o.status = "waiting_for_authorization".into();
                    o.message = "Open the authorization page and enter the device code".into();
                    o.authorization_url = Some(url.into());
                    o.user_code = value["userCode"].as_str().map(str::to_owned);
                });
                loop {
                    let event = child.read().await?;
                    if event["method"] == "account/login/completed" && event["params"]["loginId"] == login {
                        if event["params"]["success"] != true {
                            return Err(Failure::new("authentication_failed", "Codex sign-in failed or expired"));
                        }
                        break;
                    }
                }
                account(&child.rpc("account/read", json!({"refreshToken":false})).await?)
            }
            .await;
            child.finish().await;
            result
        })
    }
    fn statistics<'a>(&'a self, ctx: &'a Context, p: &'a Profile) -> ProbeFuture<'a, Value> {
        Box::pin(async move {
            let binary = ctx.binary("codex")?;
            let mut child = start(ctx, p, &binary).await?;
            let result=child.rpc("account/usage/read",json!({})).await.map(|v|json!({"scope":"account","observed_at":now(),"summary":v["summary"],"daily_usage_buckets":v["dailyUsageBuckets"]}));
            child.finish().await;
            result
        })
    }
}
