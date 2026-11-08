//! Weekly quota reset detection and persistent, asynchronous Telegram delivery.
use crate::{app::Context, model::*};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id: String,
    pub text: String,
    pub attempts: u32,
    pub next_attempt: i64,
}

const PENDING: u8 = 0;
const VERIFIED: u8 = 1;
const FAILED: u8 = 2;

pub struct Telegram {
    client: reqwest::Client,
    url: String,
    chat: String,
    state: AtomicU8,
}
impl Telegram {
    pub fn from_env() -> Result<Option<Self>> {
        let enabled = std::env::var("AGENT_ACCOUNTS_TELEGRAM_ENABLED").unwrap_or_else(|_| "false".into());
        match enabled.as_str() {
            "false" => return Ok(None),
            "true" => {}
            _ => {
                return Err(Failure::new(
                    "configuration_failure",
                    "Telegram enabled must be true or false",
                ));
            }
        }
        let token = std::env::var("AGENT_ACCOUNTS_TELEGRAM_BOT_TOKEN").unwrap_or_default();
        let chat = std::env::var("AGENT_ACCOUNTS_TELEGRAM_CHAT_ID").unwrap_or_default();
        if !token.split_once(':').is_some_and(|(id, secret)| {
            !id.is_empty()
                && id.bytes().all(|c| c.is_ascii_digit())
                && !secret.is_empty()
                && secret.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        }) || chat.is_empty()
            || chat.chars().any(char::is_whitespace)
        {
            return Err(Failure::new(
                "configuration_failure",
                "Enabled Telegram notifications require a valid bot token and chat ID",
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Failure::new("configuration_failure", "Cannot initialize Telegram HTTP client"))?;
        Ok(Some(Self {
            client,
            url: format!("https://api.telegram.org/bot{token}/sendMessage"),
            chat,
            state: AtomicU8::new(PENDING),
        }))
    }

    /// Result of the last completed delivery: `pending`, `verified` or `failed`.
    pub fn status(&self) -> &'static str {
        match self.state.load(Ordering::Relaxed) {
            VERIFIED => "verified",
            FAILED => "failed",
            _ => "pending",
        }
    }

    async fn send(&self, text: &str) -> std::result::Result<(), DeliveryError> {
        let result = self.send_message(text).await;
        self.state
            .store(if result.is_ok() { VERIFIED } else { FAILED }, Ordering::Relaxed);
        result
    }

    async fn send_message(&self, text: &str) -> std::result::Result<(), DeliveryError> {
        let mut response = self
            .client
            .post(&self.url)
            .json(&json!({"chat_id": self.chat, "text": text}))
            .send()
            .await
            .map_err(|_| DeliveryError::Retry(0))?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| DeliveryError::Retry(0))? {
            if bytes.len() + chunk.len() > 64 * 1024 {
                return Err(DeliveryError::Retry(0));
            }
            bytes.extend_from_slice(&chunk);
        }
        let body: Value = serde_json::from_slice(&bytes).map_err(|_| DeliveryError::Retry(0))?;
        if status.is_success() && body["ok"] == true {
            return Ok(());
        }
        let code = body["error_code"].as_u64().unwrap_or(u64::from(status.as_u16()));
        if code == 429 {
            return Err(DeliveryError::Retry(
                body["parameters"]["retry_after"].as_u64().unwrap_or(60),
            ));
        }
        if (400..500).contains(&code) {
            Err(DeliveryError::Rejected)
        } else {
            Err(DeliveryError::Retry(0))
        }
    }
}
enum DeliveryError {
    Retry(u64),
    Rejected,
}

fn retry_delay(attempts: u32, requested: u64) -> u64 {
    let backoff = 10_u64.saturating_mul(1_u64 << attempts.min(5)).min(300);
    requested.max(backoff).min(i64::MAX as u64)
}

fn timestamp(value: Option<i64>) -> String {
    value
        .and_then(|v| chrono::DateTime::from_timestamp(v, 0))
        .map_or_else(|| "unknown".into(), |v| v.to_rfc3339())
}

/// Compare real consecutive observations; timers alone never restore a quota.
pub fn observe(profile: &mut Profile, snapshot: &Snapshot, enabled: bool) {
    if !enabled {
        profile.notifications.clear();
        return;
    }
    let Some(previous) = profile.snapshot.as_ref() else {
        return;
    };
    if !profile.enabled
        || !matches!(snapshot.scope, Scope::Account)
        || !matches!(previous.scope, Scope::Account)
        || snapshot.observed_at <= previous.observed_at
        || previous.account.provider_id != snapshot.account.provider_id
        || previous.account.email != snapshot.account.email
    {
        return;
    }
    let mut lines = Vec::new();
    for limit in snapshot
        .limits
        .iter()
        .filter(|limit| limit.window_minutes == Some(10080))
    {
        let Some(old) = previous
            .limits
            .iter()
            .find(|old| old.id == limit.id && old.model == limit.model && old.window_minutes == Some(10080))
        else {
            continue;
        };
        let Some(used) = limit
            .used_percent
            .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
        else {
            continue;
        };
        let scheduled = match (old.resets_at, limit.resets_at) {
            (Some(before), Some(after)) => {
                previous.observed_at < before && before <= snapshot.observed_at && after > snapshot.observed_at
            }
            _ => false,
        };
        let early = old.used_percent.is_some_and(|before| {
            before.is_finite() && (0.0..=100.0).contains(&before) && used <= 5.0 && before - used >= 5.0
        });
        if scheduled || early {
            lines.push(format!(
                "{}{}: {:.1}% available; next reset {}",
                limit.label,
                limit
                    .model
                    .as_ref()
                    .map_or_else(String::new, |model| format!(" ({model})")),
                100.0 - used,
                timestamp(limit.resets_at)
            ));
        }
    }
    if !lines.is_empty() {
        let text = format!(
            "Weekly quota reset\n{} · {}\nObserved: {}\n{}",
            profile.provider,
            profile.display_name,
            timestamp(Some(snapshot.observed_at)),
            lines.join("\n")
        );
        // Telegram's message limit is 4096 characters; leave room for UTF-16 pairs.
        let mut units = 0;
        let text: String = text
            .chars()
            .take_while(|c| {
                units += c.len_utf16();
                units <= 4000
            })
            .collect();
        profile.notifications.push(Notification {
            id: uuid::Uuid::new_v4().to_string(),
            text,
            attempts: 0,
            next_attempt: now(),
        });
    }
}

async fn cycle(ctx: &Context, telegram: &Telegram) -> Result<()> {
    for profile in ctx.db.profiles()? {
        if !profile.enabled || profile.deleting {
            continue;
        }
        let Some(event) = profile
            .notifications
            .first()
            .filter(|event| event.next_attempt <= now())
        else {
            continue;
        };
        let result = tokio::select! {
            _ = ctx.shutdown.cancelled() => return Ok(()),
            result = telegram.send(&event.text) => result,
        };
        let lock = ctx.lock(&profile.id);
        let _guard = lock.lock().await;
        let mut current = match ctx.db.profile(&profile.id) {
            Ok(profile) => profile,
            Err(error) if error.code == "not_found" => continue,
            Err(error) => return Err(error),
        };
        let Some(index) = current.notifications.iter().position(|pending| pending.id == event.id) else {
            continue;
        };
        match result {
            Ok(()) => {
                current.notifications.remove(index);
            }
            Err(DeliveryError::Rejected) => {
                eprintln!(
                    "Telegram notification rejected for profile {}; check bot token and chat access",
                    profile.id
                );
                current.notifications.remove(index);
            }
            Err(DeliveryError::Retry(after)) => {
                let pending = &mut current.notifications[index];
                pending.attempts = pending.attempts.saturating_add(1);
                pending.next_attempt = now().saturating_add(retry_delay(pending.attempts, after) as i64);
                eprintln!(
                    "Telegram notification delivery failed for profile {}; will retry",
                    profile.id
                );
            }
        }
        ctx.db.save(&current, None)?;
    }
    Ok(())
}

pub async fn run(ctx: Arc<Context>) {
    let Some(telegram) = &ctx.telegram else {
        return;
    };
    let mut announced = false;
    let mut attempts: u32 = 0;
    let mut next_attempt = now();
    let mut timer = tokio::time::interval(Duration::from_secs(10));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ctx.shutdown.cancelled() => break,
            _ = timer.tick() => {
                if !announced {
                    if now() < next_attempt { continue; }
                    let result = tokio::select! {
                        _ = ctx.shutdown.cancelled() => break,
                        result = telegram.send("Agent Accounts started. Weekly quota reset notifications are enabled.") => result,
                    };
                    match result {
                        Ok(()) => {
                            announced = true;
                            eprintln!("Telegram startup notification accepted; configuration verified");
                        }
                        Err(error) => {
                            attempts = attempts.saturating_add(1);
                            let delay = match error {
                                DeliveryError::Rejected => 300,
                                DeliveryError::Retry(after) => retry_delay(attempts, after),
                            };
                            next_attempt = now().saturating_add(delay as i64);
                            eprintln!("Telegram startup verification failed; retrying in {delay} seconds");
                            continue;
                        }
                    }
                }
                if let Err(error) = cycle(&ctx, telegram).await {
                    eprintln!("Telegram notification cycle failed: {}", error.code);
                }
            }
        }
    }
}
