//! Step 6: the temporary Root bootstrap credential, held only in memory, and
//! read-only verification of its authorization through the owned Step 5 tunnel.
use super::pangolin::{Report, Wizard, next_test};
use super::pangolin_tunnel::{self, Endpoint, State};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use zeroize::{Zeroize, Zeroizing};

/// Actions homestack-bootstrap needs to provision the permanent key in Step 7.
pub const MANAGEMENT: [&str; 5] = [
    "listApiKeys",
    "listApiKeyActions",
    "createApiKey",
    "setApiKeyActions",
    "getOrg",
];
const MAX_ID: usize = 128;
const MAX_SECRET: usize = 512;
const CAPACITY: usize = MAX_ID + 1 + MAX_SECRET;
const PAGE: usize = 100;
const MAX_PAGES: usize = 1000;
const MAX_BODY: usize = 1024 * 1024;
const REQUEST_LIMIT: Duration = Duration::from_secs(10);
const DEADLINE: Duration = Duration::from_secs(60);

/// Pangolin organization and API key identifiers: `[A-Za-z0-9_-]{1,128}`.
pub fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// The secret has no Debug output, serialization or copying clone: a cloned
/// wizard receives an empty credential. The buffer is erased on every removal.
pub struct Credential {
    pub(super) value: Zeroizing<String>,
    overflow: bool,
}
impl Credential {
    fn new() -> Self {
        Self {
            value: Zeroizing::new(String::with_capacity(CAPACITY)),
            overflow: false,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }
    /// Never grows beyond the preallocated capacity, so no reallocation leaves
    /// an unerased copy behind. Unprintable or non-ASCII input becomes a space,
    /// which validation rejects instead of silently repairing the token.
    pub(super) fn insert(&mut self, text: &str) {
        for c in text.chars() {
            if self.value.len() >= CAPACITY {
                self.overflow = true;
                break;
            }
            self.value.push(if c.is_ascii_graphic() { c } else { ' ' });
        }
    }
    pub(super) fn pop(&mut self) {
        if let Some(last) = self.value.len().checked_sub(1) {
            self.value.replace_range(last.., "\0");
            self.value.truncate(last);
        }
    }
    pub(super) fn clear(&mut self) {
        self.value.zeroize();
        self.overflow = false;
    }
    /// The non-secret key ID of a well-formed `ID.secret` credential.
    pub(super) fn key_id(&self) -> Result<&str, Problem> {
        if self.value.is_empty() {
            return Err(Problem::MissingKey);
        }
        if self.overflow {
            return Err(Problem::InvalidKey);
        }
        let (id, secret) = self.value.split_once('.').ok_or(Problem::InvalidKey)?;
        if !valid_identifier(id)
            || secret.is_empty()
            || secret.len() > MAX_SECRET
            || !secret
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        {
            return Err(Problem::InvalidKey);
        }
        Ok(id)
    }
    /// The single copy handed to the verification worker; erased on drop.
    pub(super) fn duplicate(&self) -> Self {
        let mut copy = Self::new();
        copy.value.push_str(&self.value);
        copy.overflow = self.overflow;
        copy
    }
}
impl Default for Credential {
    fn default() -> Self {
        Self::new()
    }
}
impl Clone for Credential {
    fn clone(&self) -> Self {
        Self::new()
    }
}
impl PartialEq for Credential {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credential(redacted)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    Keys,
    Actions,
    Organization,
}
impl Probe {
    fn request(self) -> &'static str {
        match self {
            Self::Keys => "GET /v1/org/{orgId}/api-keys",
            Self::Actions => "GET /v1/org/{orgId}/api-key/{bootstrap key}/actions",
            Self::Organization => "GET /v1/org/{orgId}",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    MissingOrg,
    InvalidOrg,
    MissingKey,
    InvalidKey,
    Disconnected,
    Rejected(Probe),
    Forbidden(Probe),
    MissingActions,
    OrgNotFound,
    Incompatible(Probe),
    Unreachable,
    Lost,
    Timeout,
    Cancelled,
}
impl Problem {
    pub fn message(self) -> &'static str {
        match self {
            Self::MissingOrg => "Enter the Pangolin Organization ID.",
            Self::InvalidOrg => "Invalid Organization ID.",
            Self::MissingKey => "Enter the temporary bootstrap API key.",
            Self::InvalidKey => "Invalid API key format. Enter the complete ID.secret credential.",
            Self::Disconnected => "The API tunnel is disconnected. Reconnect in Step 5.",
            Self::Rejected(_) => "The bootstrap API key was rejected. Check the token.",
            Self::Forbidden(_) => "The bootstrap key lacks the Root authorization or required read access.",
            Self::MissingActions => "The bootstrap key is missing required management actions.",
            Self::OrgNotFound => "Pangolin reports that this organization does not exist.",
            Self::Incompatible(_) => "Unexpected Pangolin API response. Check server compatibility.",
            Self::Unreachable => "Pangolin did not answer through the API tunnel.",
            Self::Lost => "The SSH tunnel was lost. Reconnect in Step 5 and verify again.",
            Self::Timeout => "Bootstrap key verification timed out.",
            Self::Cancelled => "Bootstrap key verification cancelled.",
        }
    }
    /// One suggested next action; empty when the message already says it.
    pub fn action(self) -> &'static str {
        match self {
            Self::InvalidOrg => "Use 1-128 letters, digits, '_' or '-' exactly as shown in Pangolin.",
            Self::MissingKey => "Paste the complete ID.secret credential prepared in Step 2.",
            Self::Rejected(_) => "Paste the complete credential again; F7 shows the failing request.",
            Self::Forbidden(_) => "Check homestack-bootstrap in Pangolin; F7 shows the failing request.",
            Self::MissingActions => "Add the actions listed in F7 to homestack-bootstrap, then verify again.",
            Self::OrgNotFound => "Check the Organization ID in the Pangolin dashboard.",
            Self::Unreachable => "Verify the tunnel in Step 5, then verify again.",
            Self::Timeout => "Verify the tunnel in Step 5, then retry.",
            Self::Cancelled => "Press F6 to verify again.",
            _ => "",
        }
    }
    fn detail(self) -> String {
        match self {
            Self::Rejected(probe) => format!(
                "{} returned HTTP 401: Pangolin did not accept the credential.",
                probe.request()
            ),
            Self::Forbidden(Probe::Keys) => format!(
                "{} returned HTTP 403: the key needs Root authorization and the listApiKeys action.",
                Probe::Keys.request()
            ),
            Self::Forbidden(Probe::Organization) => {
                "Organization not verified. Grant getOrg to the bootstrap key and verify Step 6 again.".into()
            }
            Self::Forbidden(probe) => format!(
                "{} returned HTTP 403: the key needs the listApiKeyActions action.",
                probe.request()
            ),
            Self::Incompatible(probe) => format!(
                "{} returned an unexpected status, envelope, pagination or identifier. Response omitted.",
                probe.request()
            ),
            Self::OrgNotFound => format!("{} returned HTTP 404.", Probe::Organization.request()),
            _ => self.message().to_owned(),
        }
    }
}

/// Sanitized verification evidence; contains no credential material.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Outcome {
    pub problem: Option<Problem>,
    /// Check A: authenticated key listing exercised the Root guard and listApiKeys.
    pub listed: bool,
    pub keys: usize,
    /// Check B: reading the bootstrap key's actions exercised listApiKeyActions.
    pub read: bool,
    /// Required management actions present in the assigned-action metadata.
    pub assigned: Vec<&'static str>,
    /// Non-secret IDs of existing organization keys named `homestack`.
    pub existing: Vec<String>,
    /// `Some(true)` only when getOrg returned this organization.
    pub organization: Option<bool>,
}
impl Outcome {
    pub fn failed(problem: Problem) -> Self {
        Self {
            problem: Some(problem),
            ..Self::default()
        }
    }
    pub fn missing(&self) -> Vec<&'static str> {
        MANAGEMENT.into_iter().filter(|a| !self.assigned.contains(a)).collect()
    }
}

/// Runs on a worker thread. Read-only: only GET requests, never a mutation.
pub fn verify(endpoint: &Endpoint, organization: &str, credential: &Credential, stop: &AtomicBool) -> Outcome {
    let mut outcome = Outcome::default();
    if let Err(problem) = check(endpoint, organization, credential, stop, &mut outcome) {
        outcome.problem = Some(problem);
    }
    outcome
}

fn check(
    endpoint: &Endpoint,
    org: &str,
    credential: &Credential,
    stop: &AtomicBool,
    outcome: &mut Outcome,
) -> Result<(), Problem> {
    if !valid_identifier(org) {
        return Err(Problem::InvalidOrg);
    }
    let key_id = credential.key_id()?;
    let client = Client {
        endpoint,
        credential,
        stop,
        started: Instant::now(),
    };
    let keys = client.collection(&format!("/v1/org/{org}/api-keys"), "apiKeys", Probe::Keys)?;
    for row in &keys {
        let id = row.get("apiKeyId").and_then(serde_json::Value::as_str);
        let name = row.get("name").and_then(serde_json::Value::as_str);
        if !id.is_some_and(valid_identifier)
            || row.get("orgId").and_then(serde_json::Value::as_str) != Some(org)
            || name.is_none()
        {
            return Err(Problem::Incompatible(Probe::Keys));
        }
        if name == Some("homestack")
            && let Some(id) = id
        {
            outcome.existing.push(id.to_owned());
        }
    }
    outcome.listed = true;
    outcome.keys = keys.len();
    let actions = client.collection(
        &format!("/v1/org/{org}/api-key/{key_id}/actions"),
        "actions",
        Probe::Actions,
    )?;
    let mut assigned = std::collections::BTreeSet::new();
    for row in &actions {
        let id = row
            .get("actionId")
            .and_then(serde_json::Value::as_str)
            .filter(|id| valid_identifier(id))
            .ok_or(Problem::Incompatible(Probe::Actions))?;
        assigned.insert(id);
    }
    outcome.read = true;
    outcome.assigned = MANAGEMENT.into_iter().filter(|a| assigned.contains(a)).collect();
    if outcome.assigned.len() != MANAGEMENT.len() {
        return Err(Problem::MissingActions);
    }
    // Organization identity is mandatory for the wizard, including old saved results.
    outcome.organization = match client.get(&format!("/v1/org/{org}"), Probe::Organization)? {
        (200, Some(data)) => {
            if data
                .get("org")
                .and_then(|o| o.get("orgId"))
                .and_then(serde_json::Value::as_str)
                != Some(org)
            {
                return Err(Problem::Incompatible(Probe::Organization));
            }
            Some(true)
        }
        (403, _) => return Err(Problem::Forbidden(Probe::Organization)),
        (404, _) => return Err(Problem::OrgNotFound),
        _ => return Err(Problem::Incompatible(Probe::Organization)),
    };
    if !endpoint.alive() {
        return Err(Problem::Lost);
    }
    Ok(())
}

pub(super) struct Client<'a> {
    pub(super) endpoint: &'a Endpoint,
    pub(super) credential: &'a Credential,
    pub(super) stop: &'a AtomicBool,
    pub(super) started: Instant,
}
impl Client<'_> {
    /// Returns the envelope's `data` for HTTP 200; other statuses are left to
    /// the caller except 401, which always means the credential was rejected.
    pub(super) fn get(&self, path: &str, probe: Probe) -> Result<(u16, Option<serde_json::Value>), Problem> {
        let (status, body) = self.request("GET", path, None, probe)?;
        if !self.endpoint.alive() {
            return Err(Problem::Lost);
        }
        match status {
            200 => {
                let value: serde_json::Value =
                    serde_json::from_slice(&body).map_err(|_| Problem::Incompatible(probe))?;
                if value.get("success") != Some(&serde_json::Value::Bool(true))
                    || value.get("error") != Some(&serde_json::Value::Bool(false))
                    || !value.get("data").is_some_and(serde_json::Value::is_object)
                {
                    return Err(Problem::Incompatible(probe));
                }
                Ok((200, value.get("data").cloned()))
            }
            401 => Err(Problem::Rejected(probe)),
            status => Ok((status, None)),
        }
    }
    /// The raw response is erased on drop, including create responses containing a secret.
    pub(super) fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        probe: Probe,
    ) -> Result<(u16, Zeroizing<Vec<u8>>), Problem> {
        if self.stop.load(Ordering::SeqCst) {
            return Err(Problem::Cancelled);
        }
        if !self.endpoint.alive() {
            return Err(Problem::Lost);
        }
        let limit = DEADLINE
            .checked_sub(self.started.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or(Problem::Timeout)?
            .min(REQUEST_LIMIT);
        let body = body.unwrap_or("");
        let header = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAuthorization: Bearer ",
            self.endpoint.port,
            body.len()
        );
        let mut request = Zeroizing::new(Vec::with_capacity(
            header.len() + self.credential.value.len() + body.len() + 80,
        ));
        request.extend_from_slice(header.as_bytes());
        request.extend_from_slice(self.credential.value.as_bytes());
        request.extend_from_slice(b"\r\nAccept: application/json\r\nConnection: close\r\n\r\n");
        request.extend_from_slice(body.as_bytes());
        let result = pangolin_tunnel::exchange(self.endpoint.port, &request, self.stop, limit, MAX_BODY);
        result.map_err(|error| match error {
            pangolin_tunnel::Error::Cancelled => Problem::Cancelled,
            _ if !self.endpoint.alive() => Problem::Lost,
            pangolin_tunnel::Error::Timeout => Problem::Timeout,
            pangolin_tunnel::Error::Remote => Problem::Unreachable,
            _ => Problem::Incompatible(probe),
        })
    }
    /// Exhausts pages instead of trusting `total`, which may be a page count.
    pub(super) fn collection(
        &self,
        path: &str,
        field: &str,
        probe: Probe,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, Problem> {
        let mut rows = Vec::new();
        for page in 0..MAX_PAGES {
            let offset = page * PAGE;
            let data = match self.get(&format!("{path}?limit={PAGE}&offset={offset}"), probe)? {
                (200, Some(data)) => data,
                (403, _) => return Err(Problem::Forbidden(probe)),
                _ => return Err(Problem::Incompatible(probe)),
            };
            let pagination = data.get("pagination");
            let items = data
                .get(field)
                .and_then(serde_json::Value::as_array)
                .filter(|items| {
                    items.len() <= PAGE
                        && pagination
                            .and_then(|p| p.get("offset"))
                            .and_then(serde_json::Value::as_u64)
                            == Some(offset as u64)
                        && pagination
                            .and_then(|p| p.get("limit"))
                            .and_then(serde_json::Value::as_u64)
                            == Some(PAGE as u64)
                })
                .ok_or(Problem::Incompatible(probe))?;
            for item in items {
                rows.push(item.as_object().ok_or(Problem::Incompatible(probe))?.clone());
            }
            if items.len() < PAGE {
                return Ok(rows);
            }
        }
        Err(Problem::Incompatible(probe))
    }
}

/// Non-secret summary of the last successful verification; never authorizes
/// requests by itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub target: String,
    pub organization: String,
    pub at: String,
    pub assigned: Vec<String>,
    pub existing_homestack_key: bool,
    pub organization_confirmed: bool,
}
impl History {
    pub fn restore(value: &toml::Value, target: &str) -> Option<Self> {
        let history: Self = value.clone().try_into().ok()?;
        (history.target == target
            && valid_identifier(&history.organization)
            && history.assigned.len() <= MANAGEMENT.len()
            && history.assigned.iter().all(|a| MANAGEMENT.contains(&a.as_str()))
            && time::OffsetDateTime::parse(&history.at, &time::format_description::well_known::Rfc3339).is_ok())
        .then_some(history)
    }
}

/// A verification bound to the exact organization, key edit, server and
/// tunnel session it was started for.
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub outcome: Outcome,
    pub organization: String,
    target: String,
    tunnel: u64,
    revision: u64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Bootstrap {
    pub organization: String,
    pub cursor: usize,
    pub key: Credential,
    revision: u64,
    pub running: u64,
    pub check: Option<Check>,
    pub history: Option<History>,
}
impl Bootstrap {
    pub fn restore(history: Option<History>) -> Self {
        let organization = history.as_ref().map(|h| h.organization.clone()).unwrap_or_default();
        Self {
            cursor: organization.len(),
            organization,
            history,
            ..Self::default()
        }
    }
}

impl Wizard {
    pub fn bootstrap_available(&self) -> bool {
        self.tunnel_available()
    }
    pub fn tunnel_connected(&self) -> bool {
        self.tunnel.current.state == State::Connected && self.tunnel_available()
    }
    fn bootstrap_current(&self, check: &Check) -> bool {
        check.organization == self.bootstrap.organization
            && check.revision == self.bootstrap.revision
            && check.target == self.target
            && check.tunnel == self.tunnel.id
    }
    /// The current result, ignoring results for another key, organization,
    /// server or tunnel session.
    pub fn bootstrap_check(&self) -> Option<&Check> {
        self.bootstrap.check.as_ref().filter(|c| self.bootstrap_current(c))
    }
    pub fn bootstrap_verified(&self) -> bool {
        self.bootstrap.running == 0
            && !self.bootstrap.key.is_empty()
            && self.tunnel_connected()
            && self.bootstrap_check().is_some_and(|c| {
                c.outcome.problem.is_none() && c.outcome.organization == Some(true) && c.outcome.missing().is_empty()
            })
    }
    pub fn bootstrap_status(&self) -> &'static str {
        if self.bootstrap.running != 0 {
            "Verifying"
        } else if self.bootstrap_verified() {
            "Verified"
        } else if let Some(problem) = self.bootstrap_check().and_then(|c| c.outcome.problem) {
            if problem == Problem::Cancelled {
                "Cancelled"
            } else {
                "Failed"
            }
        } else if self.bootstrap.check.is_some() {
            "Verification outdated"
        } else {
            "Not verified"
        }
    }
    pub fn begin_bootstrap(&mut self) -> Option<u64> {
        if self.selected_step != 5
            || self.bootstrap.running != 0
            || self.testing
            || self.tunnel.busy()
            || self.help_open
            || self.report_open
            || !self.bootstrap_available()
        {
            return None;
        }
        let organization = &self.bootstrap.organization;
        let problem = if organization.is_empty() {
            Some(Problem::MissingOrg)
        } else if !valid_identifier(organization) {
            Some(Problem::InvalidOrg)
        } else if let Err(problem) = self.bootstrap.key.key_id() {
            Some(problem)
        } else if !self.tunnel_connected() {
            Some(Problem::Disconnected)
        } else {
            None
        };
        self.bootstrap.check = Some(Check {
            outcome: problem.map(Outcome::failed).unwrap_or_default(),
            organization: organization.clone(),
            target: self.target.clone(),
            tunnel: self.tunnel.id,
            revision: self.bootstrap.revision,
        });
        if problem.is_some() {
            return None;
        }
        self.bootstrap.running = next_test();
        self.message.clear();
        Some(self.bootstrap.running)
    }
    /// The organization and one worker copy of the secret for a started run.
    pub(super) fn bootstrap_input(&self, id: u64) -> Option<(String, Credential)> {
        (id != 0 && self.bootstrap.running == id)
            .then(|| (self.bootstrap.organization.clone(), self.bootstrap.key.duplicate()))
    }
    pub fn complete_bootstrap(&mut self, id: u64, mut outcome: Outcome) {
        if id == 0 || self.bootstrap.running != id {
            return;
        }
        self.bootstrap.running = 0;
        let current = self.bootstrap.check.as_ref().is_some_and(|c| self.bootstrap_current(c));
        if !current || !self.tunnel_connected() {
            outcome.problem = Some(Problem::Lost);
        }
        let Some(check) = self.bootstrap.check.as_mut() else {
            return;
        };
        check.outcome = outcome;
        if check.outcome.problem.is_some() {
            return;
        }
        let history = History {
            target: check.target.clone(),
            organization: check.organization.clone(),
            at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            assigned: check.outcome.assigned.iter().map(|a| (*a).to_owned()).collect(),
            existing_homestack_key: !check.outcome.existing.is_empty(),
            organization_confirmed: check.outcome.organization == Some(true),
        };
        if let Err(error) = self.save_bootstrap_history(&history) {
            self.message = error;
        }
        self.bootstrap.history = Some(history);
    }
    pub fn cancel_bootstrap(&mut self, problem: Problem) {
        if self.bootstrap.running == 0 {
            return;
        }
        self.bootstrap.running = 0;
        if let Some(check) = self.bootstrap.check.as_mut() {
            check.outcome = Outcome::failed(problem);
        }
    }
    /// Any edit invalidates the result and abandons a running verification.
    fn bootstrap_edited(&mut self) {
        self.permanent.result = None;
        self.permanent.confirm = false;
        self.bootstrap.revision += 1;
        self.bootstrap.running = 0;
        self.bootstrap.check = None;
    }
    /// F5 / Clear: erase the secret locally; nothing is revoked on Pangolin.
    pub fn clear_bootstrap(&mut self) {
        self.bootstrap.key.clear();
        self.bootstrap_edited();
    }
    /// Server or integration identity changed: the secret is no longer bound
    /// to the endpoint it was entered for.
    pub(super) fn forget_bootstrap(&mut self) {
        self.clear_bootstrap();
    }
    pub(super) fn bootstrap_paste(&mut self, text: &str) {
        self.bootstrap_insert(text.trim());
    }
    fn bootstrap_insert(&mut self, text: &str) {
        match self.focus {
            1 => {
                // Retain one excess character so overlong input fails validation.
                let room = (MAX_ID + 1).saturating_sub(self.bootstrap.organization.chars().count());
                let clean: String = text
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .take(room)
                    .collect();
                if clean.is_empty() {
                    return;
                }
                let at = self
                    .bootstrap
                    .organization
                    .char_indices()
                    .nth(self.bootstrap.cursor)
                    .map_or(self.bootstrap.organization.len(), |(at, _)| at);
                self.bootstrap.organization.insert_str(at, &clean);
                self.bootstrap.cursor += clean.chars().count();
            }
            2 => self.bootstrap.key.insert(text),
            _ => return,
        }
        self.bootstrap_edited();
    }
    /// Step 6 keys; `false` leaves the key to the shared wizard handling.
    pub(super) fn bootstrap_key(&mut self, key: KeyEvent) -> bool {
        let typing = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let clear_line = key.code == KeyCode::Char('u') && key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Tab => self.focus = (self.focus + 1) % 5,
            KeyCode::BackTab => self.focus = (self.focus + 4) % 5,
            KeyCode::Enter | KeyCode::Right if self.focus == 0 => self.focus = 1,
            KeyCode::Enter if self.focus == 1 || self.focus == 2 => self.focus += 1,
            KeyCode::Esc if self.bootstrap.running != 0 => self.cancel_bootstrap(Problem::Cancelled),
            KeyCode::F(5) => self.clear_bootstrap(),
            KeyCode::Enter if self.focus == 4 => self.clear_bootstrap(),
            _ if self.focus == 1 => {
                let org = &mut self.bootstrap.organization;
                let length = org.chars().count();
                let cursor = &mut self.bootstrap.cursor;
                match key.code {
                    KeyCode::Home => *cursor = 0,
                    KeyCode::End => *cursor = length,
                    KeyCode::Left => *cursor = cursor.saturating_sub(1),
                    KeyCode::Right => *cursor = (*cursor + 1).min(length),
                    _ if clear_line => {
                        org.clear();
                        *cursor = 0;
                        self.bootstrap_edited();
                    }
                    KeyCode::Backspace | KeyCode::Delete => {
                        let at = if key.code == KeyCode::Backspace {
                            cursor.checked_sub(1)
                        } else {
                            Some(*cursor)
                        };
                        if let Some(at) = at.filter(|at| *at < length) {
                            let mut chars: Vec<char> = org.chars().collect();
                            chars.remove(at);
                            *org = chars.into_iter().collect();
                            *cursor = (*cursor).min(at);
                            self.bootstrap_edited();
                        }
                    }
                    KeyCode::Char(c) if typing => self.bootstrap_insert(&c.to_string()),
                    _ => return false,
                }
            }
            _ if self.focus == 2 => match key.code {
                // The mask reveals no position, so editing is append-only.
                KeyCode::Backspace => {
                    self.bootstrap.key.pop();
                    self.bootstrap_edited();
                }
                KeyCode::Delete => self.clear_bootstrap(),
                _ if clear_line => self.clear_bootstrap(),
                KeyCode::Char(c) if typing => self.bootstrap_insert(&c.to_string()),
                KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End => {}
                _ => return false,
            },
            _ => return false,
        }
        true
    }
    /// Sanitized F7 report: no credential, key ID or response body.
    pub fn bootstrap_report(&self, offset: i64) -> String {
        let b = &self.bootstrap;
        let mut lines = vec![
            format!(
                "Organization ID: {}",
                if b.organization.is_empty() {
                    "Not entered"
                } else {
                    &b.organization
                }
            ),
            format!("Pangolin server: {}", self.target),
            format!("API tunnel: {}", self.tunnel.current.state.label()),
            format!(
                "Temporary key: {}",
                if b.key.is_empty() {
                    "Not entered"
                } else {
                    "Entered; held in memory only, never displayed or saved"
                }
            ),
            format!("Status: {}", self.bootstrap_status()),
        ];
        if let Some(check) = self.bootstrap_check().filter(|_| b.running == 0) {
            let o = &check.outcome;
            let pass = |ok: bool| if ok { "PASS" } else { "Not verified" };
            lines.push(String::new());
            lines.push(format!(
                "{} (all pages): {}{}",
                Probe::Keys.request(),
                pass(o.listed),
                if o.listed {
                    format!(
                        " - authentication, Root guard and listApiKeys exercised; {} organization keys",
                        o.keys
                    )
                } else {
                    String::new()
                }
            ));
            lines.push(format!(
                "{} (all pages): {}{}",
                Probe::Actions.request(),
                pass(o.read),
                if o.read { " - listApiKeyActions exercised" } else { "" }
            ));
            if o.read {
                for action in MANAGEMENT {
                    lines.push(format!(
                        "  {action}: {}",
                        if o.assigned.contains(&action) {
                            "Assigned"
                        } else {
                            "Missing"
                        }
                    ));
                }
                let missing = o.missing();
                if !missing.is_empty() {
                    lines.push(format!("Missing management actions: {}", missing.join(", ")));
                }
            }
            lines.push(format!(
                "{}: {}",
                Probe::Organization.request(),
                match o.organization {
                    Some(true) => "PASS - organization confirmed by getOrg",
                    Some(false) =>
                        "Organization not verified. Grant getOrg to the bootstrap key and verify Step 6 again.",
                    None => "Not checked - organization existence not confirmed",
                }
            ));
            if o.listed {
                lines.push(if o.existing.is_empty() {
                    "Existing organization key named homestack: none".to_owned()
                } else {
                    format!(
                        "Existing organization key named homestack: found ({}). Step 7 must handle it without creating a duplicate.",
                        o.existing.len()
                    )
                });
            }
            lines.push(
                "Write operations were not executed. createApiKey and setApiKeyActions are confirmed only as assigned metadata; Step 7 exercises them after separate approval."
                    .to_owned(),
            );
            if let Some(problem) = o.problem {
                lines.push(String::new());
                lines.push(format!("Diagnosis: {}", problem.detail()));
                if !problem.action().is_empty() {
                    lines.push(format!("Next: {}", problem.action()));
                }
            }
        }
        if let Some(h) = &b.history {
            let date = Report {
                target: h.target.clone(),
                at: h.at.clone(),
                passed: true,
                detail: String::new(),
                duration_ms: 0,
            }
            .local_date(offset);
            lines.push(String::new());
            lines.push(format!(
                "Last successful verification: {date} for organization {} (historical; never authorizes requests).",
                h.organization
            ));
        }
        lines.push(String::new());
        lines.push("Requests: GET only, Bearer header through the owned loopback forward, no proxies or redirects, 10 seconds each and 60 seconds overall. Responses are bounded and never displayed or saved.".to_owned());
        lines.join("\n")
    }
}
