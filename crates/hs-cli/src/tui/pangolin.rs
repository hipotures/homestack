//! Pangolin setup: SSH verification, manual prerequisites and read-only discovery.
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use crate::system::{Output, System};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub const STAGES: [&str; 8] = [
    "Pangolin server",
    "Pangolin prerequisites",
    "Pangolin discovery",
    "Integration API",
    "API tunnel",
    "Bootstrap API key",
    "HomeStack API key",
    "Integration verification",
];
pub const DESCRIPTIONS: [&str; 8] = [
    "Select and test an existing OpenSSH destination.",
    "Confirm installation and preparation of a temporary Root bootstrap key.",
    "Discover runtime, configuration and ports through read-only SSH inspection.",
    "Inspect private Integration API health; approve minimal backed-up configuration changes.",
    "Manage an owned SSH tunnel and verify private API health without credentials.",
    "Future: accept a masked temporary key and verify its permissions.",
    "Future: provision and securely save a least-privilege organization key.",
    "Future: verify integration permissions and read-only operations.",
];
/// Contextual help for locked stages describes future work, not a working operation.
pub const HELP: [&str; 8] = [
    "Select an OpenSSH alias, hostname, IP address or user@host. OpenSSH reads your existing configuration to resolve the user, port, identities and proxy settings; HomeStack does not assume root.\n\nF6 tests the current draft asynchronously: ssh -G resolves configuration, then an authenticated SSH session executes only true. Existing askpass prompts support host confirmation, passphrases, PINs and FIDO key touch. Host verification remains enabled; password and keyboard-interactive fallback remain disabled.\n\nThe overall deadline is 45 seconds. Esc cancels; changing the target invalidates verification. Only a successful unchanged test saves its target automatically, with mode 0600. Failure or cancellation does not overwrite the saved target.\n\nHomeStack stores no private SSH keys or authentication secrets. The last successful test and its date are retained when reopening. Editing the target requires a new test. This phase does not inspect or configure Pangolin.",
    "Prepare Pangolin Server on the SSH server verified in Step 1. Confirm both instruction groups only after completing them. This is a manual declaration, not technical verification. No installation, API operation or remote change occurs here.\n\nIn the Pangolin administration interface, open API Keys and create homestack-bootstrap with Root authorization and these actions:\n  createApiKey\n  listApiKeys\n  listApiKeyActions\n  setApiKeyActions\n\nRoot scope is required for Integration API key management; selecting four actions alone is insufficient. These actions create the restricted key, detect conflicts, inspect actions and assign permissions. The temporary administrative key is separate from the permanent, restricted HomeStack key.\n\nKeep its token securely until masked entry in Step 6. Do not enter it here. Revoke it only after the permanent key has been provisioned and verified successfully.\n\nStep 3 will discover the installation method, version, services, configuration paths and ports read-only. It does not assume Docker Compose or fixed paths. The Integration API may remain disabled until Step 4. Checking this box does not verify installation or API availability. Confirmation is saved for the selected server; changing the target requires reconfirmation.",
    "Discover the existing installation over authenticated SSH, read-only, without modifying it. Docker Compose metadata identifies a project and its configuration files; plain Docker has no verified Compose project. Native systemd executables may be identified, but unsupported layouts remain incomplete.\n\nHomeStack reads selected runtime metadata, discovered configuration paths, literal Integration API settings and listening sockets. Mounts describe host-to-container paths; published ports describe host bind addresses. Only allowlisted inventory is returned. No environments, secrets or full configuration files are displayed or saved. Remote Python 3 is required; nothing is installed.\n\nF6 discovers or retries, Esc cancels, F7 opens the scrollable report. Permission failures require access through your existing account; HomeStack never uses sudo. Multiple installations are ambiguous and never selected automatically. Supply an account with read access or inspect unsupported layouts separately.\n\nDisabled Integration API is allowed. Missing or ambiguous settings remain unknown, never guessed. API changes belong to Step 4 with approval and backup. Steps 6-8 remain unimplemented.\n\nSaved results are historical observations, not continuous monitoring. Changing the SSH destination invalidates discovery and requires a new inspection.",
    "Inspect the existing Integration API over SSH without credentials. F6 revalidates Step 3 metadata and checks the configured API port, runtime publication and unauthenticated GET /v1/ health. A successful loopback-only health response completes this step without any write or restart.\n\nWhen changes are required, F7 shows exact values, files, service, impact and protected remote backup path. F5 opens confirmation defaulting to Cancel. Only explicit Apply authorizes writes. The target, file hashes and effective Compose configuration are checked again before backup and activation.\n\nAutomatic editing supports literal YAML in a single-file Docker Compose project with a verified directory bind mount and ownership allowing atomic replacement. Existing remote PyYAML and Docker Compose v2 are required for editing; no packages or privilege escalation are attempted. Complex YAML, interpolation, multiple files and ambiguous mounts require independent configuration.\n\nOnly Pangolin is recreated, without dependencies, builds or image pulls. Sites, Resources and sessions may briefly be interrupted. Backups remain private on the VPS, never downloaded. Failures retain backups and distinguish no changes from uncertain or unhealthy activation. No automatic rollback or mutation retry occurs.\n\nEsc cancels inspection. Interrupting Apply can leave changes in place: inspect the service and backup before retrying. Saved results are historical. Changing the target or rerunning discovery invalidates verification. No API key or tunnel is used; Step 5 manages a separate private tunnel.",
    "Connect through authenticated OpenSSH to the private endpoint verified in Step 4. No API key, password fallback, remote command or server change is used. Existing SSH configuration and hardware-key askpass prompts are retained; host identity verification stays enabled.\n\nF6 connects or verifies the existing tunnel. F5 disconnects only the owned connection. Tab moves between navigation and actions; Enter activates them. F7 shows a sanitized report. Esc cancels connection or verification; Esc leaving the wizard closes its owned tunnel. Closing help or navigating between stages does not close an active connection.\n\nHomeStack creates a private session-only control socket with configured forwards cleared, then adds only its own IPv4 loopback forward through that socket. A free unprivileged local port is allocated automatically, with up to three collision retries. No shared SSH master is changed.\n\nConnected requires an HTTP 200 Pangolin Healthy JSON response through the forward and a live owned SSH child. Requests use no proxies, redirects or credentials. A listener alone is insufficient. Startup is bounded to 45 seconds and each health request to 2 seconds. Process termination is monitored; remote API health is refreshed with F6, not continuously monitored.\n\nThe connection survives navigation to future stages. It closes on disconnect, cancellation, wizard exit, application shutdown or invalidated server/API metadata. Only historical successful connection metadata is persisted; restarting HomeStack always starts disconnected, without authentication prompts. Local ports and process IDs are never saved as reusable connections.\n\nWhen remote API access fails, return to Step 4. Steps 6-8 remain unimplemented.",
    "Locked - not implemented.\n\nA future step will accept a temporary bootstrap API credential in a masked field and verify its Root scope and required API-key permissions. The secret must not enter logs, command arguments or repository files.\n\nNo API credential field is enabled now. Step 1 only stores the non-secret SSH destination.",
    "Locked - not implemented.\n\nA future step will create an organization-scoped HomeStack API key with only the permissions required by the integration, verify its assigned permissions and save it securely.\n\nSelective reconfiguration must reuse unrelated existing credentials and avoid duplicate keys or resources. No keys or Pangolin objects are created by the current wizard.",
    "Locked - not implemented.\n\nA future step will independently verify the integration, assigned permissions and safe read-only API operations. It must distinguish observed results from assumptions and user confirmations.\n\nA successful SSH test marks only Step 1 complete (1/8). It does not establish that Pangolin, its API, tunnels or resource routing work.",
];
pub const INSTALLATION: &str = "Pangolin Server must be installed and running on the selected server.\n\nPangolin must already be installed on the SSH server selected in Step 1. No particular installation method is required: Docker Compose, standalone services and other deployment methods may exist. Step 3 will discover the actual installation. The Integration API may still be disabled; Step 4 handles it. Phase 2 makes no remote configuration changes.";
pub const BOOTSTRAP: &str = "Create the temporary homestack-bootstrap Root API key.\n\nIn the Pangolin administration interface, open API Keys and create:\nName: homestack-bootstrap\nAuthorization: Root\nRequired actions:\n  createApiKey\n  listApiKeys\n  listApiKeyActions\n  setApiKeyActions\n\nIntegration API key management requires Root authorization; four actions without Root scope are insufficient. This temporary administrative key is separate from the restricted HomeStack integration key. Keep the token securely until Step 6; do not enter it here. Revoke it after successful provisioning and verification of the permanent key.";
static NEXT_TEST: AtomicU64 = AtomicU64::new(1);
pub(super) fn next_test() -> u64 {
    NEXT_TEST.fetch_add(1, Ordering::Relaxed)
}

/// Strict destination syntax, never an option, URI, command or credential.
pub fn validate_destination(target: &str) -> Result<(), String> {
    let invalid = || "Enter an SSH alias, hostname, IP address or user@host (no options or URI).".to_owned();
    if target.is_empty() || target.len() > 320 || !target.is_ascii() {
        return Err(invalid());
    }
    let (user, host) = match target.split_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, target),
    };
    if let Some(user) = user
        && (user.is_empty()
            || user.starts_with('-')
            || !user.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)))
    {
        return Err(invalid());
    }
    if host.is_empty() || host.starts_with('-') {
        return Err(invalid());
    }
    let ip = host.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(host);
    if ip.parse::<std::net::IpAddr>().is_ok() {
        return Ok(());
    }
    if host.len() > 253 || !host.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)) {
        return Err(invalid());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub target: String,
    pub at: String,
    pub passed: bool,
    pub detail: String,
    pub duration_ms: u64,
}

impl Report {
    pub fn local_date(&self, offset: i64) -> String {
        let Ok(at) = time::OffsetDateTime::parse(&self.at, &time::format_description::well_known::Rfc3339) else {
            return "Unknown date".to_owned();
        };
        let offset = i32::try_from(offset)
            .ok()
            .and_then(|seconds| time::UtcOffset::from_whole_seconds(seconds).ok())
            .unwrap_or(time::UtcOffset::UTC);
        let at = at.to_offset(offset);
        format!(
            "{:02}.{:02}.{:04} {:02}:{:02}:{:02}",
            at.day(),
            u8::from(at.month()),
            at.year(),
            at.hour(),
            at.minute(),
            at.second()
        )
    }

    pub fn duration(&self) -> String {
        if self.duration_ms < 1000 {
            return format!("{} ms", self.duration_ms);
        }
        let tenths = self.duration_ms.saturating_add(50) / 100;
        format!("{}.{:01} s", tenths / 10, tenths % 10)
    }
}

#[derive(Clone, PartialEq)]
pub struct Wizard {
    pub tunnel: super::pangolin_tunnel::View,
    pub api: Option<super::pangolin_api::Api>,
    pub api_running: bool,
    pub api_applying: bool,
    pub api_confirm: bool,
    pub api_confirm_yes: bool,
    pub discovery: Option<Discovery>,
    pub discovering: bool,
    pub target: String,
    pub focus: usize,
    pub cursor: usize,
    pub expanded: bool,
    pub selected_step: usize,
    pub instructions: [bool; 2],
    pub prerequisites_scroll: usize,
    prerequisites_target: Option<String>,
    pub help_open: bool,
    pub help_scroll: usize,
    pub testing: bool,
    pub test_id: u64,
    pub message: String,
    pub report: Option<Report>,
    pub report_open: bool,
    pub report_scroll: usize,
    pub verified: Option<String>,
    path: PathBuf,
    saved: String,
    revision: u64,
    tested_revision: u64,
    confirm_discard: bool,
}

impl std::fmt::Debug for Wizard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PangolinWizard")
            .field("testing", &self.testing)
            .finish_non_exhaustive()
    }
}

impl Wizard {
    pub fn open(config: &Path) -> Result<Self, String> {
        let path = config.join("integrations/pangolin.toml");
        let settings = read(&path)?;
        let target = settings
            .get("server_target")
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if !target.is_empty() {
            validate_destination(&target)?;
        }
        let report = settings.get("last_test").and_then(|value| {
            let at = value.get("at")?.as_str()?;
            time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339).ok()?;
            let tested = value.get("target")?.as_str()?;
            validate_destination(tested).ok()?;
            Some(Report {
                target: tested.to_owned(),
                at: at.to_owned(),
                passed: value.get("passed")?.as_bool()?,
                duration_ms: u64::try_from(value.get("duration_ms")?.as_integer()?).ok()?,
                detail: "PASS · SSH authenticated, remote command successful (true).".to_owned(),
            })
        });
        let verified = report
            .as_ref()
            .filter(|r| r.passed && r.target == target)
            .map(|r| r.target.clone());
        let prerequisites_target = settings
            .get("prerequisites")
            .filter(|value| value.get("confirmed").and_then(toml::Value::as_bool) == Some(true))
            .and_then(|value| value.get("target").and_then(toml::Value::as_str))
            .filter(|confirmed| verified.as_deref() == Some(*confirmed))
            .map(str::to_owned);
        let history = settings
            .get("tunnel_history")
            .and_then(|v| v.clone().try_into::<super::pangolin_tunnel::History>().ok())
            .filter(|h| {
                h.target == target
                    && validate_destination(&h.target).is_ok()
                    && h.remote
                        .strip_prefix("127.0.0.1:")
                        .and_then(|p| p.parse::<u16>().ok())
                        .is_some_and(|p| p > 0)
                    && time::OffsetDateTime::parse(&h.at, &time::format_description::well_known::Rfc3339).is_ok()
            });
        Ok(Self {
            tunnel: super::pangolin_tunnel::View {
                history,
                ..Default::default()
            },
            api: settings
                .get("integration_api")
                .and_then(super::pangolin_api::Api::restore),
            api_running: false,
            api_applying: false,
            api_confirm: false,
            api_confirm_yes: false,
            discovery: settings.get("discovery").and_then(Discovery::restore),
            discovering: false,
            cursor: target.chars().count(),
            saved: target.clone(),
            target,
            path,
            focus: 0,
            expanded: false,
            selected_step: 0,
            instructions: [false; 2],
            prerequisites_scroll: 0,
            prerequisites_target,
            help_open: false,
            help_scroll: 0,
            testing: false,
            test_id: 0,
            message: String::new(),
            report,
            report_open: false,
            report_scroll: 0,
            verified,
            revision: 0,
            tested_revision: 0,
            confirm_discard: false,
        })
    }
    pub(super) fn save_tunnel_history(&self, history: &super::pangolin_tunnel::History) -> Result<(), String> {
        let mut table = read(&self.path)?;
        if table.get("server_target").and_then(toml::Value::as_str) != Some(self.target.as_str()) {
            return Err("Server changed; tunnel history was not saved.".into());
        }
        table.insert(
            "tunnel_history".into(),
            toml::Value::try_from(history).map_err(|_| "Cannot serialize tunnel history.")?,
        );
        persist(&self.path, &table)
    }
    pub(super) fn persist_api(&self, report: &super::pangolin_api::Api) -> Result<(), String> {
        let mut table = read(&self.path)?;
        if table.get("discovery").and_then(Discovery::restore) != self.discovery {
            return Err("Discovery changed on disk; inspect again before saving API verification.".into());
        }
        if let Some(discovery) = &report.refreshed {
            table.insert("discovery".into(), discovery.store());
        }
        table.insert("integration_api".into(), report.store()?);
        persist(&self.path, &table)
    }
    pub fn discovery_available(&self) -> bool {
        self.prerequisites_confirmed()
    }
    pub fn discovery_status(&self) -> &str {
        if self.discovering {
            return "Discovering";
        }
        match &self.discovery {
            Some(r) if !self.discovery_available() || r.target != self.target => "Outdated",
            Some(r) => &r.outcome,
            None => "Not discovered",
        }
    }
    pub fn begin_discovery(&mut self) -> Option<u64> {
        if self.tunnel.busy()
            || self.testing
            || self.selected_step != 2
            || !self.discovery_available()
            || self.help_open
            || self.report_open
        {
            return None;
        }
        self.invalidate_api();
        self.test_id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        self.tested_revision = self.revision;
        self.testing = true;
        self.discovering = true;
        self.message.clear();
        Some(self.test_id)
    }
    pub fn complete_discovery(&mut self, id: u64, result: Discovery) {
        if !self.discovering
            || self.test_id != id
            || result.target != self.target
            || self.revision != self.tested_revision
            || !self.discovery_available()
        {
            return;
        }
        self.testing = false;
        self.discovering = false;
        let saved = (|| {
            let mut table = read(&self.path)?;
            if table.get("server_target").and_then(toml::Value::as_str) != Some(self.target.as_str())
                || table
                    .get("prerequisites")
                    .and_then(|p| p.get("confirmed"))
                    .and_then(toml::Value::as_bool)
                    != Some(true)
                || table
                    .get("prerequisites")
                    .and_then(|p| p.get("target"))
                    .and_then(toml::Value::as_str)
                    != Some(self.target.as_str())
            {
                return Err("Server prerequisites changed; confirm them again.".to_owned());
            }
            table.insert("discovery".to_owned(), result.store());
            persist(&self.path, &table)
        })();
        self.discovery = Some(result);
        if let Err(error) = saved {
            if let Some(result) = self.discovery.as_mut() {
                result.outcome = "Failed".to_owned();
            }
            self.message = error;
        }
    }
    pub fn worker_failed(&mut self, id: u64, message: &str) {
        if self.api_running {
            let mut report = self.api.clone().unwrap_or_else(|| self.api_failure("ssh"));
            report.status = "Failed".into();
            report.code = "ssh".into();
            report.plan = None;
            self.complete_api(id, report);
        } else if self.discovering {
            self.complete_discovery(id, Discovery::failure(&self.target, "Failed", message));
        } else {
            self.testing = false;
        }
        self.message = message.to_owned();
    }
    pub fn has_changes(&self) -> bool {
        self.target != self.saved
    }
    pub fn status(&self) -> &'static str {
        if self.testing && !self.discovering && !self.api_running {
            "Testing"
        } else if self.verified.as_ref() == Some(&self.target) {
            "Verified"
        } else if let Some(report) = &self.report {
            if report.target != self.target || self.revision != self.tested_revision {
                "Verification outdated"
            } else if !report.passed {
                "Failed"
            } else {
                "Verification outdated"
            }
        } else if self.target.is_empty() {
            "Not configured"
        } else if self.has_changes() {
            "Verification outdated"
        } else {
            "Saved, not verified"
        }
    }
    fn changed(&mut self) {
        self.invalidate_api();
        self.api_running = false;
        self.revision += 1;
        if let Some(discovery) = self.discovery.as_mut() {
            discovery.outcome = "Outdated".to_owned();
        }
        self.discovering = false;
        self.verified = None;
        self.prerequisites_target = None;
        self.testing = false;
        self.test_id = 0;
        self.confirm_discard = false;
        self.message.clear();
    }
    pub fn paste(&mut self, text: &str) {
        if self.focus != 1 || self.selected_step != 0 || self.report_open || self.help_open {
            return;
        }
        let clean: String = text.chars().filter(|c| !c.is_control()).take(320).collect();
        let byte = self
            .target
            .char_indices()
            .nth(self.cursor)
            .map_or(self.target.len(), |(at, _)| at);
        self.target.insert_str(byte, &clean);
        self.cursor += clean.chars().count();
        self.changed();
    }
    pub fn prerequisites_available(&self) -> bool {
        self.status() == "Verified" && self.saved == self.target
    }
    pub fn prerequisites_confirmed(&self) -> bool {
        self.prerequisites_available() && self.prerequisites_target.as_ref() == Some(&self.target)
    }
    pub fn completed(&self) -> usize {
        usize::from(self.status() == "Verified")
            + usize::from(self.prerequisites_confirmed())
            + usize::from(self.discovery_status() == "Discovered")
            + usize::from(self.api_status() == "Verified")
            + usize::from(
                self.tunnel.current.state == super::pangolin_tunnel::State::Connected && self.tunnel_available(),
            )
    }
    pub fn toggle_prerequisites(&mut self) {
        if !self.prerequisites_available() {
            return;
        }
        let confirmed = !self.prerequisites_confirmed();
        self.invalidate_api();
        let result = (|| {
            let mut table = read(&self.path)?;
            // Never record confirmation against a replaced or unverified file.
            if table.get("server_target").and_then(toml::Value::as_str) != Some(self.target.as_str())
                || table
                    .get("last_test")
                    .and_then(|v| v.get("target"))
                    .and_then(toml::Value::as_str)
                    != Some(self.target.as_str())
                || table
                    .get("last_test")
                    .and_then(|v| v.get("passed"))
                    .and_then(toml::Value::as_bool)
                    != Some(true)
            {
                return Err("Server settings changed; retest Step 1 before confirming.".to_owned());
            }
            let mut prerequisites = toml::Table::new();
            prerequisites.insert("target".to_owned(), toml::Value::String(self.target.clone()));
            prerequisites.insert("confirmed".to_owned(), toml::Value::Boolean(confirmed));
            table.insert("prerequisites".to_owned(), toml::Value::Table(prerequisites));
            if !confirmed && let Some(discovery) = table.get_mut("discovery").and_then(toml::Value::as_table_mut) {
                discovery.insert("outcome".to_owned(), toml::Value::String("Outdated".to_owned()));
            }
            persist(&self.path, &table)
        })();
        match result {
            Ok(()) => {
                self.prerequisites_target = confirmed.then(|| self.target.clone());
                if !confirmed && self.discovering {
                    self.discovering = false;
                    self.testing = false;
                    self.test_id = 0;
                }
                if !confirmed && let Some(result) = self.discovery.as_mut() {
                    result.outcome = "Outdated".to_owned();
                }
                self.message.clear();
            }
            Err(error) => self.message = format!("{error} Space retries."),
        }
    }
    /// Mouse interaction uses the same controls and dependency checks as keys.
    pub fn prerequisite_control(&mut self, control: usize) {
        if self.selected_step != 1 || !self.prerequisites_available() || self.help_open || self.report_open {
            return;
        }
        self.focus = control.clamp(1, 3);
        if self.focus == 3 {
            self.toggle_prerequisites();
        } else {
            self.instructions[self.focus - 1] = !self.instructions[self.focus - 1];
            self.prerequisites_scroll = 0;
        }
    }
    /// Every stage can be selected for explanation; only Steps 1-5 have actions.
    pub fn select_step(&mut self, step: usize) {
        if self.api_confirm || self.api_running {
            return;
        }
        self.selected_step = step.min(STAGES.len() - 1);
        self.focus = 0;
        self.help_scroll = 0;
        self.message.clear();
    }

    /// Field and action focus is independent of step navigation.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        if self.api_confirm {
            match key.code {
                KeyCode::Esc => self.api_confirm = false,
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                    self.api_confirm_yes = !self.api_confirm_yes
                }
                KeyCode::Enter if !self.api_confirm_yes => self.api_confirm = false,
                _ => {}
            }
            return false;
        }
        if self.help_open {
            match key.code {
                KeyCode::Esc | KeyCode::F(4) | KeyCode::Enter => self.help_open = false,
                KeyCode::Down => self.help_scroll = self.help_scroll.saturating_add(1),
                KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(8),
                KeyCode::Up => self.help_scroll = self.help_scroll.saturating_sub(1),
                KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(8),
                KeyCode::Home => self.help_scroll = 0,
                _ => {}
            }
            return false;
        }
        if self.report_open {
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::F(7) => self.report_open = false,
                KeyCode::Down => self.report_scroll = self.report_scroll.saturating_add(1),
                KeyCode::PageDown => self.report_scroll = self.report_scroll.saturating_add(8),
                KeyCode::Up => self.report_scroll = self.report_scroll.saturating_sub(1),
                KeyCode::PageUp => self.report_scroll = self.report_scroll.saturating_sub(8),
                _ => {}
            }
            return false;
        }
        if self.selected_step == 4 && self.tunnel_available() {
            match key.code {
                KeyCode::Tab | KeyCode::BackTab => {
                    let mut controls = vec![0, 1];
                    if self.tunnel.current.state == super::pangolin_tunnel::State::Connected {
                        controls.push(2);
                    }
                    let at = controls.iter().position(|v| *v == self.focus).unwrap_or(0);
                    self.focus = controls[if key.code == KeyCode::BackTab {
                        (at + controls.len() - 1) % controls.len()
                    } else {
                        (at + 1) % controls.len()
                    }];
                    return false;
                }
                KeyCode::Enter | KeyCode::Right if self.focus == 0 => {
                    self.focus = 1;
                    return false;
                }
                _ => {}
            }
        }
        if self.selected_step == 3 && self.api_available() {
            match key.code {
                KeyCode::Tab | KeyCode::BackTab => {
                    let mut controls = vec![0, 1];
                    if self.api.is_some() {
                        controls.push(2);
                    }
                    if self.can_apply_api() {
                        controls.push(3);
                    }
                    let at = controls.iter().position(|f| *f == self.focus).unwrap_or(0);
                    self.focus = controls[if key.code == KeyCode::BackTab {
                        (at + controls.len() - 1) % controls.len()
                    } else {
                        (at + 1) % controls.len()
                    }];
                    return false;
                }
                KeyCode::Enter | KeyCode::Right if self.focus == 0 => {
                    self.focus = 1;
                    return false;
                }
                KeyCode::Enter if self.focus == 2 => {
                    self.report_open = self.api.is_some();
                    self.report_scroll = 0;
                    return false;
                }
                KeyCode::F(5) | KeyCode::Enter if key.code == KeyCode::F(5) || self.focus == 3 => {
                    if self.can_apply_api() {
                        self.api_confirm = true;
                        self.api_confirm_yes = false;
                    }
                    return false;
                }
                KeyCode::Esc if self.api_running => {
                    self.testing = false;
                    self.api_running = false;
                    self.test_id = 0;
                    let mut r = self.api.clone().unwrap_or_else(|| self.api_failure("cancelled"));
                    r.code = if self.api_applying { "uncertain" } else { "cancelled" }.into();
                    r.status = "Cancelled".into();
                    if self.api_applying
                        && let Some(plan) = &r.plan
                    {
                        r.backup = plan.backup.clone();
                    }
                    r.plan = None;
                    if self.api_applying {
                        r.changed = "Remote outcome uncertain; inspect before retrying".into();
                    }
                    self.api = Some(r);
                    self.api_applying = false;
                    if let Some(r) = &self.api {
                        let _ = self.persist_api(r);
                    }
                    return false;
                }
                _ => {}
            }
        }
        if self.selected_step == 2 && self.discovery_available() {
            match key.code {
                KeyCode::Tab | KeyCode::BackTab => {
                    let count = if self.discovery.is_some() { 3 } else { 2 };
                    self.focus = if key.code == KeyCode::BackTab {
                        (self.focus + count - 1) % count
                    } else {
                        (self.focus + 1) % count
                    };
                    return false;
                }
                KeyCode::Enter | KeyCode::Right if self.focus == 0 => {
                    self.focus = 1;
                    return false;
                }
                KeyCode::Enter if self.focus == 2 => {
                    self.report_open = self.discovery.is_some();
                    self.report_scroll = 0;
                    return false;
                }
                _ => {}
            }
        }
        if self.selected_step == 1 && self.prerequisites_available() {
            match key.code {
                KeyCode::Tab => {
                    self.focus = (self.focus + 1) % 4;
                    return false;
                }
                KeyCode::BackTab => {
                    self.focus = (self.focus + 3) % 4;
                    return false;
                }
                KeyCode::Enter | KeyCode::Right if self.focus == 0 => {
                    self.focus = 1;
                    return false;
                }
                KeyCode::Enter if self.focus == 1 || self.focus == 2 => {
                    self.prerequisite_control(self.focus);
                    return false;
                }
                KeyCode::Right | KeyCode::Left if self.focus == 1 || self.focus == 2 => {
                    self.instructions[self.focus - 1] = key.code == KeyCode::Right;
                    self.prerequisites_scroll = 0;
                    return false;
                }
                KeyCode::Char(' ') if self.focus == 3 => {
                    self.toggle_prerequisites();
                    return false;
                }
                KeyCode::Enter if self.focus == 3 => {
                    if !self.prerequisites_confirmed() {
                        self.toggle_prerequisites();
                    }
                    if self.prerequisites_confirmed() {
                        // Select discovery without starting any remote operation.
                        self.select_step(2);
                    }
                    return false;
                }
                KeyCode::PageDown => {
                    self.prerequisites_scroll = self.prerequisites_scroll.saturating_add(5);
                    return false;
                }
                KeyCode::PageUp => {
                    self.prerequisites_scroll = self.prerequisites_scroll.saturating_sub(5);
                    return false;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc if self.testing => {
                if self.discovering {
                    let result = Discovery::failure(&self.target, "Cancelled", "Discovery cancelled.");
                    self.complete_discovery(self.test_id, result);
                } else {
                    self.verified = None;
                    self.message = "SSH test cancelled.".to_owned();
                }
                self.testing = false;
                self.test_id = 0;
            }
            KeyCode::Esc if !self.has_changes() || self.confirm_discard => return true,
            KeyCode::Esc => {
                self.confirm_discard = true;
                self.message = "Unsaved changes. Esc again to discard; F6 to test and save.".to_owned();
            }
            KeyCode::F(4) => {
                self.help_open = true;
                self.help_scroll = 0;
            }
            KeyCode::F(7) => {
                self.report_open = if self.selected_step == 4 {
                    true
                } else if self.selected_step == 3 {
                    self.api.is_some()
                } else if self.selected_step == 2 {
                    self.discovery.is_some()
                } else {
                    self.selected_step == 0 && self.report.is_some()
                };
                self.report_scroll = 0;
            }
            KeyCode::Down if self.focus == 0 => self.select_step((self.selected_step + 1) % STAGES.len()),
            KeyCode::Up if self.focus == 0 => self.select_step((self.selected_step + STAGES.len() - 1) % STAGES.len()),
            KeyCode::Tab | KeyCode::Down if self.selected_step == 0 => self.focus = (self.focus + 1) % 3,
            KeyCode::BackTab | KeyCode::Up if self.selected_step == 0 => self.focus = (self.focus + 2) % 3,
            KeyCode::Enter | KeyCode::Right if self.focus == 0 && self.selected_step == 0 => self.focus = 1,
            KeyCode::Enter if self.selected_step > 0 => {
                self.help_open = true;
                self.help_scroll = 0;
            }
            KeyCode::Char(' ') if self.focus == 0 => self.expanded = !self.expanded,
            KeyCode::Home if self.selected_step == 0 && self.focus == 1 => self.cursor = 0,
            KeyCode::End if self.selected_step == 0 && self.focus == 1 => self.cursor = self.target.chars().count(),
            KeyCode::Left if self.selected_step == 0 && self.focus == 1 => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right if self.selected_step == 0 && self.focus == 1 => {
                self.cursor = (self.cursor + 1).min(self.target.chars().count())
            }
            KeyCode::Char('u')
                if self.selected_step == 0 && self.focus == 1 && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.target.clear();
                self.cursor = 0;
                self.changed();
            }
            KeyCode::Backspace | KeyCode::Delete if self.selected_step == 0 && self.focus == 1 => {
                let mut chars: Vec<char> = self.target.chars().collect();
                let at = if key.code == KeyCode::Backspace {
                    self.cursor.checked_sub(1)
                } else {
                    Some(self.cursor)
                };
                if let Some(at) = at.filter(|at| *at < chars.len()) {
                    chars.remove(at);
                    self.cursor = self.cursor.min(at);
                    self.target = chars.into_iter().collect();
                    self.changed();
                }
            }
            KeyCode::Char(c)
                if self.focus == 1 && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.paste(&c.to_string())
            }
            _ => {}
        }
        false
    }
    fn save(&mut self, report: &Report) -> Result<(), String> {
        validate_destination(&self.target)?;
        if !report.passed || report.target != self.target {
            return Err("Only a successful test can save its exact target".to_owned());
        }
        // Preserve unrelated future settings without resetting credentials.
        let mut table = read(&self.path)?;
        table.insert("server_target".to_owned(), toml::Value::String(self.target.clone()));
        let mut last_test = toml::Table::new();
        last_test.insert("target".to_owned(), toml::Value::String(report.target.clone()));
        last_test.insert("at".to_owned(), toml::Value::String(report.at.clone()));
        last_test.insert("passed".to_owned(), toml::Value::Boolean(true));
        last_test.insert(
            "duration_ms".to_owned(),
            toml::Value::Integer(i64::try_from(report.duration_ms).unwrap_or(i64::MAX)),
        );
        // Only fixed metadata is persisted; no raw SSH output or prompt text.
        table.insert("last_test".to_owned(), toml::Value::Table(last_test));
        if self.prerequisites_target.as_ref() != Some(&self.target) {
            table.remove("integration_api");
            table.remove("discovery");
            table.remove("prerequisites");
        }
        persist(&self.path, &table)?;
        self.saved = self.target.clone();
        self.confirm_discard = false;
        Ok(())
    }
    pub fn begin_test(&mut self) -> Option<u64> {
        if self.tunnel.busy() || self.testing || self.selected_step != 0 || self.help_open || self.report_open {
            return None;
        }
        if let Err(error) = validate_destination(&self.target) {
            self.message = error;
            return None;
        }
        self.test_id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        self.tested_revision = self.revision;
        self.testing = true;
        self.verified = None;
        self.message = "Testing SSH…".to_owned();
        Some(self.test_id)
    }
    pub fn complete(&mut self, id: u64, mut report: Report) {
        if !self.testing || self.test_id != id || self.target != report.target || self.revision != self.tested_revision
        {
            return;
        }
        self.testing = false;
        if report.at.is_empty() {
            report.at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default();
        }
        if report.passed {
            self.verified = Some(report.target.clone());
            self.message = match self.save(&report) {
                Ok(()) => String::new(),
                Err(error) => format!("SSH verified, but {error}. Run F6 again to test and retry saving."),
            };
        } else {
            self.message = report.detail.clone();
        }
        self.report = Some(report);
    }
}
fn persist(path: &Path, table: &toml::Table) -> Result<(), String> {
    let save = || -> Result<(), Box<dyn std::error::Error>> {
        let parent = path.parent().ok_or("Invalid integration path")?;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(toml::to_string_pretty(table)?.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    };
    save().map_err(|_| "Cannot save Pangolin settings.".to_owned())
}
fn read(path: &Path) -> Result<toml::Table, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|_| "Invalid Pangolin settings".to_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(_) => Err("Cannot read Pangolin settings".to_owned()),
    }
}

/// The existing askpass bridge supplies `prefix`; no separate authentication
/// implementation, identities, usernames or SSH configuration files are used.
pub fn test_ssh(
    system: &dyn System,
    target: &str,
    prefix: &[String],
    stop: &AtomicBool,
    sounds: &crate::touch::Sounds,
    player: crate::touch::Player,
) -> Result<String, String> {
    ssh_command(system, target, prefix, stop, sounds, player, "true", Duration::from_secs(45)).0
        .map(|_| "PASS · Effective OpenSSH configuration resolved\nPASS · SSH authenticated, remote command successful (true)\nNo Pangolin inspection or server configuration performed.".to_owned())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn ssh_command(
    system: &dyn System,
    target: &str,
    prefix: &[String],
    stop: &AtomicBool,
    sounds: &crate::touch::Sounds,
    player: crate::touch::Player,
    remote: &str,
    limit: Duration,
) -> (Result<String, String>, Option<String>) {
    let mut account = None;
    let result = (|| {
        validate_destination(target)?;
        let started = std::time::Instant::now();
        let run = |argv: &[String]| -> Result<Output, String> {
            if stop.load(Ordering::SeqCst) {
                return Err("SSH test cancelled.".to_owned());
            }
            let remaining = limit.checked_sub(started.elapsed()).ok_or("SSH operation timed out.")?;
            system.run_until(argv, stop, remaining).map_err(|e| {
                match e.kind() {
                    std::io::ErrorKind::TimedOut => "SSH operation timed out.",
                    std::io::ErrorKind::Interrupted => "SSH test cancelled.",
                    _ => "Cannot execute OpenSSH; check that ssh is installed.",
                }
                .to_owned()
            })
        };
        // Explicitly retain host verification, disable side effects from forwards
        // and LocalCommand, and require a foreground command rather than a shell,
        // configured RemoteCommand, multiplexed session or background connection.
        let options = [
            "-T",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "ConnectionAttempts=1",
            "-o",
            "BatchMode=no",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "PermitLocalCommand=no",
            "-o",
            "RemoteCommand=none",
            "-o",
            "SessionType=default",
            "-o",
            "ForkAfterAuthentication=no",
            "-o",
            "PasswordAuthentication=no",
            "-o",
            "KbdInteractiveAuthentication=no",
            "-o",
            "LogLevel=ERROR",
        ];
        let mut config = prefix.to_vec();
        config.push("ssh".to_owned());
        config.extend(options.map(str::to_owned));
        config.extend(["-G".to_owned(), "--".to_owned(), target.to_owned()]);
        let resolved = run(&config)?;
        if resolved.code != 0
            || !["hostname ", "user ", "port "]
                .iter()
                .all(|key| resolved.stdout.lines().any(|line| line.starts_with(key)))
        {
            return Err("Cannot resolve effective OpenSSH configuration.".to_owned());
        }
        account = resolved
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("user "))
            .filter(|user| {
                !user.is_empty()
                    && user.len() <= 128
                    && user.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            })
            .map(str::to_owned);
        let host_keys = resolved
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("stricthostkeychecking "));
        if !matches!(host_keys, Some("ask" | "true" | "yes" | "accept-new")) {
            return Err(
                "SSH configuration must enable host key verification; insecure host checking is refused.".to_owned(),
            );
        }
        let mut command = prefix.to_vec();
        command.push("ssh".to_owned());
        command.extend(options.map(str::to_owned));
        command.extend(["--".to_owned(), target.to_owned(), remote.to_owned()]);
        crate::touch::touch_wait(target, sounds, player, |_| {
            let output = run(&command)?;
            if stop.load(Ordering::SeqCst) {
                return Err("SSH test cancelled.".to_owned());
            }
            if output.code != 0 {
                // Never display raw SSH output, config dumps, paths, prompts or secrets.
                let error = output.stderr.to_ascii_lowercase();
                return Err(
                    if error.contains("permission denied") || error.contains("authentication failed") {
                        "SSH authentication failed; check the configured identity and access."
                    } else if error.contains("host key verification failed")
                        || error.contains("host identification has changed")
                    {
                        "SSH host key verification failed; inspect known_hosts before retrying."
                    } else if error.contains("timed out") {
                        "SSH connection timed out."
                    } else if error.contains("resolve hostname")
                        || error.contains("connection refused")
                        || error.contains("no route to host")
                        || error.contains("network is unreachable")
                    {
                        "SSH host is unreachable; check the destination, port and network."
                    } else {
                        "SSH remote command failed; check SSH configuration and server access."
                    }
                    .to_owned(),
                );
            }
            Ok(output.stdout)
        })
    })();
    (result, account)
}

/// Cancels the worker when the wizard/TUI closes, edits or starts another test.
pub struct Pending {
    pub id: u64,
    pub cancel: Arc<AtomicBool>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// Only these non-secret inventory fields may cross the SSH/report boundary.
const FACTS: [&str; 21] = [
    "ssh_user",
    "working_directory",
    "installation",
    "identity",
    "state",
    "image",
    "version_evidence",
    "compose_project",
    "compose",
    "config",
    "config_status",
    "mounts",
    "mappings",
    "api_enabled",
    "api_port",
    "api_publication",
    "api_listening",
    "listening_ports",
    "internal_port",
    "external_port",
    "service_file",
];
const OUTCOMES: [&str; 10] = [
    "Discovered",
    "Partially discovered",
    "Not found",
    "Permission denied",
    "Unsupported installation",
    "Failed",
    "Cancelled",
    "Outdated",
    "Not discovered",
    "Discovering",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Discovery {
    pub target: String,
    pub at: String,
    pub outcome: String,
    pub facts: std::collections::BTreeMap<String, String>,
    pub diagnostic: String,
}
impl Discovery {
    pub fn failure(target: &str, outcome: &str, detail: &str) -> Self {
        Self {
            target: target.to_owned(),
            at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            outcome: outcome.to_owned(),
            facts: Default::default(),
            diagnostic: detail.to_owned(),
        }
    }
    pub fn parse(target: &str, text: &str) -> Result<Self, String> {
        let invalid = || "Discovery returned malformed or unsafe inventory; raw output omitted.".to_owned();
        if text.len() > 32768 {
            return Err(invalid());
        }
        let value: serde_json::Value = serde_json::from_str(text).map_err(|_| invalid())?;
        let outcome = value
            .get("outcome")
            .and_then(|v| v.as_str())
            .filter(|s| OUTCOMES.contains(s))
            .ok_or_else(invalid)?;
        let mut report = Self::failure(target, outcome, "Inventory contains unresolved fields. See report.");
        let facts = value.get("facts").and_then(|v| v.as_object()).ok_or_else(invalid)?;
        for key in FACTS {
            if let Some(value) = facts.get(key) {
                let text = value
                    .as_str()
                    .filter(|s| s.len() <= 512 && !s.chars().any(char::is_control))
                    .ok_or_else(invalid)?;
                report.facts.insert(key.to_owned(), text.to_owned());
            }
        }
        // Map bounded issue codes to fixed diagnostics; never display remote free text.
        let issues = value.get("issues").and_then(|v| v.as_array());
        let messages: Vec<&str> = issues.into_iter().flatten().take(16).filter_map(|v| match v.as_str()? {
            "unsupported" => Some("A Pangolin-named service uses an unrecognized executable; installation is unsupported."),
            "ambiguous" => Some("Multiple Pangolin installations detected. Automatic selection is unsafe."),
            "docker_unavailable" => Some("Docker runtime inspection failed; check daemon availability and account access."),
            "docker_permission" => Some("SSH connected, but Docker metadata is not accessible to this account."),
            "compose_unknown" => Some("Docker Compose project found, but its configuration file location is unknown."),
            "config_unknown" => Some("Pangolin configuration path could not be uniquely determined."),
            "config_unreadable" => Some("Pangolin configuration is unreadable or uses unsupported YAML."),
            "sockets_unreadable" => Some("Container listening sockets are not readable by this account."),
            "stopped" => Some("Pangolin is installed but its service is stopped."),
            "api_unknown" => Some("Integration API settings are unknown; defaults are not inferred."),
            "native_incomplete" => Some("Native/systemd service identified; configuration and version remain unresolved."),
            "docker_listing_invalid" => Some("Docker returned unexpected runtime metadata."),
            "not_found" => Some("Pangolin installation not found in accessible supported runtimes. Custom layouts are not searched."),
            _ => None,
        }).collect();
        report.diagnostic = if messages.is_empty() {
            "Read-only inventory; unknown fields remain unresolved. No remote changes made.".to_owned()
        } else {
            messages.join("\n")
        };
        if outcome == "Discovered"
            && !(matches!(report.fact("installation"), "Docker" | "Docker Compose")
                && report.fact("state") == "Running"
                && report.fact("identity") != "Unknown"
                && report.fact("config").starts_with('/')
                && report.fact("config_status") == "Literal settings parsed"
                && matches!(report.fact("api_enabled"), "true" | "false")
                && (report.fact("api_enabled") == "false"
                    || (report.fact("api_port").parse::<u16>().is_ok_and(|p| p > 0)
                        && matches!(report.fact("api_listening"), "Yes" | "No"))))
        {
            report.outcome = "Partially discovered".to_owned();
            report.diagnostic = "Mandatory discovery evidence is missing; unresolved fields remain unknown.".to_owned();
        }
        Ok(report)
    }
    pub fn fact(&self, key: &str) -> &str {
        self.facts.get(key).map_or("Unknown", String::as_str)
    }
    pub fn known(&self, key: &str) -> Option<&str> {
        self.facts
            .get(key)
            .map(String::as_str)
            .filter(|s| !s.is_empty() && !matches!(*s, "Unknown" | "Unavailable"))
    }
    pub fn has_inventory(&self) -> bool {
        self.known("installation").is_some() || self.known("identity").is_some()
    }
    /// A single category, explanation and next action; technical detail stays in F7.
    pub fn problem(&self) -> Option<(&'static str, &'static str, &'static str)> {
        Some(match self.outcome.as_str() {
            "Discovered" => return None,
            "Permission denied" => (
                "Permission denied",
                "Cannot access Docker metadata.",
                "Grant appropriate Docker access or choose another authorized SSH account.",
            ),
            "Not found" => (
                "Not found",
                "Pangolin was not found in accessible runtimes.",
                "Check the selected server and confirm Pangolin is installed.",
            ),
            "Unsupported installation" if self.diagnostic.contains("Multiple") => (
                "Ambiguous installation",
                "Multiple Pangolin installations were found.",
                "Inspect F7 Details to identify the intended installation.",
            ),
            "Unsupported installation" => (
                "Unsupported installation",
                "The installation layout could not be identified safely.",
                "Inspect F7 Details and ask the server administrator to identify the runtime.",
            ),
            "Partially discovered" if self.fact("state") == "Stopped" => (
                "Stopped service",
                "Pangolin is installed but its service is stopped.",
                "Ask the server administrator to start Pangolin, then retry.",
            ),
            "Partially discovered" => (
                "Partially discovered",
                "Some installation details could not be read or identified.",
                "Inspect F7 Details for missing metadata and required read access.",
            ),
            "Cancelled" => ("Cancelled", "Discovery was cancelled.", "Press F6 to retry when ready."),
            "Outdated" => (
                "Outdated",
                "The saved discovery no longer applies to this server configuration.",
                "Complete the server prerequisites and run discovery again.",
            ),
            _ if self.diagnostic.contains("authentication failed") => (
                "SSH authentication failed",
                "SSH authentication did not succeed.",
                "Check the configured SSH identity or choose another authorized account.",
            ),
            _ if self.diagnostic.contains("host key") => (
                "SSH host key failure",
                "SSH host key verification did not succeed.",
                "Inspect the host key and known_hosts before retrying.",
            ),
            _ if self.diagnostic.contains("unreachable") => (
                "SSH unreachable",
                "The configured SSH destination is unreachable.",
                "Check the SSH destination, port and network, then retry.",
            ),
            _ if self.diagnostic.contains("timed out") => (
                "Discovery timed out",
                "Discovery exceeded its connection or execution deadline.",
                "Check server reachability and retry; inspect F7 Details if it persists.",
            ),
            _ => (
                "Discovery failed",
                "The read-only inspection could not complete.",
                "Inspect F7 Details, resolve the reported issue and retry.",
            ),
        })
    }
    pub fn summary(&self) -> String {
        let account = self
            .known("ssh_user")
            .or_else(|| self.target.split_once('@').map(|(user, _)| user));
        let server = self
            .target
            .split_once('@')
            .map_or(self.target.as_str(), |(_, host)| host);
        let mut lines = vec![match account {
            Some(account) => format!("SSH account: {account} · Server: {server}"),
            None => format!("Server: {server} (SSH account not resolved)"),
        }];
        if let Some((_, explanation, action)) = self.problem() {
            lines.push(explanation.to_owned());
            lines.push(action.to_owned());
        } else if self.has_inventory() {
            for (key, label) in [
                ("installation", "Installation"),
                ("state", "Runtime"),
                ("config", "Config"),
                ("api_enabled", "API enabled"),
                ("api_port", "API port"),
            ] {
                if let Some(value) = self.known(key) {
                    lines.push(format!("{label}: {value}"));
                }
            }
            if self.known("image").is_some() {
                lines.push("Software version: unverified (image in F7 Details)".to_owned());
            }
            lines.push("Historical observation; date and inventory: F7 Details.".to_owned());
        }
        lines.join("\n")
    }
    fn store(&self) -> toml::Value {
        let mut table = toml::Table::new();
        for (key, value) in [("target", &self.target), ("at", &self.at), ("outcome", &self.outcome)] {
            table.insert(key.to_owned(), toml::Value::String(value.clone()));
        }
        let facts = self
            .facts
            .iter()
            .map(|(key, value)| (key.clone(), toml::Value::String(value.clone())))
            .collect();
        table.insert("facts".to_owned(), toml::Value::Table(facts));
        if self.diagnostic.lines().all(|line| SAFE_DIAGNOSTICS.contains(&line)) {
            table.insert("diagnostic".to_owned(), toml::Value::String(self.diagnostic.clone()));
        }
        toml::Value::Table(table)
    }
    fn restore(value: &toml::Value) -> Option<Self> {
        let target = value.get("target")?.as_str()?;
        validate_destination(target).ok()?;
        let at = value.get("at")?.as_str()?;
        time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339).ok()?;
        let facts: serde_json::Map<String, serde_json::Value> = value
            .get("facts")?
            .as_table()?
            .iter()
            .filter_map(|(key, value)| {
                value
                    .as_str()
                    .map(|s| (key.clone(), serde_json::Value::String(s.to_owned())))
            })
            .collect();
        let input = serde_json::json!({"outcome":value.get("outcome")?.as_str()?, "facts":facts});
        let mut result = Self::parse(target, &input.to_string()).ok()?;
        result.at = at.to_owned();
        if let Some(detail) = value.get("diagnostic").and_then(toml::Value::as_str)
            && detail.len() <= 4096
            && detail.lines().all(|line| SAFE_DIAGNOSTICS.contains(&line))
        {
            result.diagnostic = detail.to_owned();
        }
        Some(result)
    }
    pub fn text(&self, offset: i64) -> String {
        let date = Report {
            target: String::new(),
            at: self.at.clone(),
            passed: false,
            detail: String::new(),
            duration_ms: 0,
        }
        .local_date(offset);
        let mut lines = vec![
            format!("Server: {}", self.target),
            format!("Last discovery: {date} (historical observation)"),
            format!("Outcome: {}", self.outcome),
        ];
        for detail in self.diagnostic.lines().filter(|line| !line.is_empty()) {
            if !lines.iter().any(|line| line == detail) {
                lines.push(detail.to_owned());
            }
        }
        if let Some(account) = self.known("ssh_user") {
            lines.insert(1, format!("SSH account: {account}"));
        }
        if self.has_inventory() {
            for key in FACTS.into_iter().filter(|key| *key != "ssh_user") {
                if let Some(value) = self.known(key) {
                    lines.push(format!("{}: {value}", key.replace('_', " ")));
                }
            }
        }
        let access = if self.fact("api_enabled") == "false" {
            "Integration API is disabled; enabling it belongs to Step 4."
        } else if self.fact("api_enabled") != "true" {
            "Integration API configuration is unknown; inspect permissions and layout."
        } else if self.fact("api_listening") == "No" {
            "Configured API is not listening; Step 4 must investigate before bootstrap."
        } else if self.fact("api_publication") == "Wider interface" {
            "API is published beyond loopback; review exposure before future bootstrap."
        } else if self.fact("api_publication") == "Not published" {
            "API is not published to the host; future bootstrap transport needs configuration."
        } else {
            "Transport and API reachability are not tested in this phase."
        };
        if self.known("api_enabled").is_some() {
            lines.push(format!("Future bootstrap access: {access}"));
        }
        lines.join("\n\n")
    }
}

pub fn discover(
    system: &dyn System,
    target: &str,
    prefix: &[String],
    stop: &AtomicBool,
    sounds: &crate::touch::Sounds,
    player: crate::touch::Player,
) -> Discovery {
    // Single-quote the fixed program for the remote SSH shell. No target/path
    // or other remotely supplied value is ever interpolated into shell source.
    let source = include_str!("pangolin_discover.py");
    let command = format!("python3 -B -c '{}'", source.replace('\'', "'\\''"));
    let (result, account) = ssh_command(
        system,
        target,
        prefix,
        stop,
        sounds,
        player,
        &command,
        Duration::from_secs(45),
    );
    let result = result.and_then(|text| Discovery::parse(target, &text));
    let mut report = match result {
        Ok(report) if !stop.load(Ordering::SeqCst) => report,
        Ok(_) => Discovery::failure(target, "Cancelled", "Discovery cancelled."),
        Err(error) => {
            let outcome = if stop.load(Ordering::SeqCst) || error.contains("cancelled") {
                "Cancelled"
            } else {
                "Failed"
            };
            let detail = if error.contains("timed out") {
                "Discovery timed out."
            } else if error.contains("remote command failed") {
                "Read-only probe failed. Remote Python 3 is required; no packages were installed."
            } else {
                &error
            };
            Discovery::failure(target, outcome, detail)
        }
    };
    if let Some(account) = account {
        report.facts.insert("ssh_user".to_owned(), account);
    }
    report
}

const SAFE_DIAGNOSTICS: &[&str] = &[
    "Docker runtime inspection failed; check daemon availability and account access.",
    "Previous SSH operation is stopping; retry shortly.",
    "Cannot start the existing SSH askpass bridge.",
    "Docker Compose project found, but its configuration file location is unknown.",
    "A Pangolin-named service uses an unrecognized executable; installation is unsupported.",
    "Multiple Pangolin installations detected. Automatic selection is unsafe.",
    "SSH connected, but Docker metadata is not accessible to this account.",
    "Pangolin configuration path could not be uniquely determined.",
    "Pangolin configuration is unreadable or uses unsupported YAML.",
    "Container listening sockets are not readable by this account.",
    "Pangolin is installed but its service is stopped.",
    "Integration API settings are unknown; defaults are not inferred.",
    "Native/systemd service identified; configuration and version remain unresolved.",
    "Docker returned unexpected runtime metadata.",
    "Pangolin installation not found in accessible supported runtimes. Custom layouts are not searched.",
    "Read-only inventory; unknown fields remain unresolved. No remote changes made.",
    "Mandatory discovery evidence is missing; unresolved fields remain unknown.",
    "Discovery cancelled.",
    "Discovery timed out.",
    "Read-only probe failed. Remote Python 3 is required; no packages were installed.",
    "SSH authentication failed; check the configured identity and access.",
    "SSH host key verification failed; inspect known_hosts before retrying.",
    "SSH host is unreachable; check the destination, port and network.",
    "SSH connection timed out.",
    "Cannot resolve effective OpenSSH configuration.",
    "Cannot execute OpenSSH; check that ssh is installed.",
    "Discovery returned malformed or unsafe inventory; raw output omitted.",
];
