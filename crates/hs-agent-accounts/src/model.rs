use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Account,
    Instance,
    Session,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    pub provider_id: Option<String>,
    pub email: Option<String>,
    pub plan: Option<String>,
    /// Current paid billing period, as Unix seconds, when the provider reports it.
    #[serde(default)]
    pub period_start: Option<i64>,
    #[serde(default)]
    pub period_end: Option<i64>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Limit {
    pub id: String,
    pub label: String,
    pub model: Option<String>,
    pub window_minutes: Option<i64>,
    pub used_percent: Option<f64>,
    pub resets_at: Option<i64>,
    pub exhausted: Option<bool>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub scope: Scope,
    pub observed_at: i64,
    pub processed_at: i64,
    pub account: Account,
    pub limits: Vec<Limit>,
    pub credits: Option<Value>,
    pub reset_credits: Option<Value>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub provider: String,
    pub display_name: String,
    pub enabled: bool,
    pub deleting: bool,
    pub auth_status: String,
    pub account: Option<Account>,
    pub last_success: Option<i64>,
    pub last_attempt: Option<i64>,
    pub error: Option<Failure>,
    pub failures: u64,
    #[serde(default)]
    pub total_failures: u64,
    #[serde(default)]
    pub probe_duration_seconds: Option<f64>,
    /// When a start_window request last completed; tells a just-started window from a not-started one.
    #[serde(default)]
    pub window_started_at: Option<i64>,
    /// Owner-entered time of the latest subscription payment, for providers that do not report a billing period.
    #[serde(default)]
    pub last_payment: Option<i64>,
    pub snapshot: Option<Snapshot>,
    #[serde(default)]
    pub notifications: Vec<crate::telegram::Notification>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub stale: bool,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Failure {
    pub code: String,
    pub message: String,
}
impl Failure {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
    pub fn unsupported() -> Self {
        Self::new(
            "unsupported_capability",
            "This provider does not support this operation",
        )
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for Failure {}
pub type Result<T> = std::result::Result<T, Failure>;

#[derive(Clone, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub profile_id: Option<String>,
    pub provider: String,
    pub kind: String,
    pub status: String,
    pub message: String,
    pub authorization_url: Option<String>,
    pub user_code: Option<String>,
    pub accepts_code: bool,
    pub error: Option<Failure>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    #[serde(default)]
    pub result: Option<Value>,
}
impl Operation {
    pub fn terminal(&self) -> bool {
        self.finished_at.is_some()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Runtime {
    pub provider: String,
    pub version: String,
    pub previous_version: Option<String>,
    pub latest_version: Option<String>,
    pub checked_at: Option<i64>,
    pub last_result: Option<String>,
    #[serde(default)]
    pub last_error: Option<Failure>,
    #[serde(default)]
    pub verified_at: Option<i64>,
}
