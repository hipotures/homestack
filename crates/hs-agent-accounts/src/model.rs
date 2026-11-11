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

/// The session window: the shortest window the provider reports.
fn session_limit(s: &Snapshot) -> Option<(&Limit, i64)> {
    s.limits
        .iter()
        .filter_map(|l| l.window_minutes.filter(|m| *m > 0).map(|m| (l, m * 60)))
        .min_by_key(|(_, span)| *span)
}

/// Whether the session window is not running, as the dashboard decides it: a start request
/// counts until its window would end; otherwise zero usage with no reset, or a reset a full
/// window away, has not started, and a reported reset already passed has ended.
pub fn window_idle(s: &Snapshot, started: Option<i64>, now: i64) -> bool {
    let Some((l, span)) = session_limit(s) else {
        return false;
    };
    if started.is_some_and(|t| t + span > now) {
        return false;
    }
    if l.resets_at.is_some_and(|t| t <= now) {
        return true;
    }
    l.used_percent == Some(0.0) && l.resets_at.is_none_or(|t| t - s.observed_at >= span - 120)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoStart {
    /// Local `HH:MM` in `timezone`.
    pub time: String,
    /// How long after `time` a start may still happen.
    pub tolerance_minutes: i64,
    /// IANA time zone of the owner's browser.
    pub timezone: String,
}
impl AutoStart {
    fn parts(&self) -> Option<(chrono_tz::Tz, u32, u32)> {
        let (h, m) = self.time.split_once(':')?;
        let digits = |s: &str, len: std::ops::RangeInclusive<usize>| {
            len.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
        };
        (digits(h, 1..=2) && digits(m, 2..=2)).then_some(())?;
        let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
        (h < 24 && m < 60).then_some(())?;
        Some((self.timezone.parse().ok()?, h, m))
    }
    pub fn normalized(mut self) -> Result<Self> {
        let (_, h, m) = self
            .parts()
            .ok_or_else(|| Failure::new("invalid_input", "Auto-start needs a HH:MM time and a known time zone"))?;
        if !(1..=720).contains(&self.tolerance_minutes) {
            return Err(Failure::new("invalid_input", "Tolerance must be 1–720 minutes"));
        }
        self.time = format!("{h:02}:{m:02}");
        Ok(self)
    }
    /// The latest scheduled moment at or before `now`, as Unix seconds.
    pub fn latest(&self, now: i64) -> Option<i64> {
        use chrono::TimeZone;
        let (tz, h, m) = self.parts()?;
        let today = chrono::DateTime::from_timestamp(now, 0)?
            .with_timezone(&tz)
            .date_naive();
        let at = |date: chrono::NaiveDate| {
            tz.from_local_datetime(&date.and_hms_opt(h, m, 0)?)
                .earliest()
                .map(|t| t.timestamp())
        };
        match at(today) {
            Some(t) if t <= now => Some(t),
            _ => at(today.pred_opt()?),
        }
    }
    /// Due within the tolerance after the scheduled time, once per scheduled time, while the
    /// session window is not running.
    pub fn due(&self, p: &Profile, now: i64) -> bool {
        self.latest(now).is_some_and(|at| {
            now - at < self.tolerance_minutes * 60
                && p.window_started_at.is_none_or(|t| t < at)
                && p.snapshot
                    .as_ref()
                    .is_some_and(|s| window_idle(s, p.window_started_at, now))
        })
    }
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
    /// Owner-chosen daily time to start the shortest usage window when it is not running.
    #[serde(default)]
    pub auto_start: Option<AutoStart>,
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
