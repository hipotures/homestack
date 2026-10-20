//! Pangolin setup: SSH verification and manual, non-secret prerequisites.
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
    "Future: discover the actual Pangolin installation through read-only SSH inspection.",
    "Future: inspect API configuration; approve and back up any changes.",
    "Future: tunnel to the private Integration API and verify reachability.",
    "Future: accept a masked temporary key and verify its permissions.",
    "Future: provision and securely save a least-privilege organization key.",
    "Future: verify integration permissions and read-only operations.",
];
/// Contextual help for locked stages describes future work, not a working operation.
pub const HELP: [&str; 8] = [
    "Select an OpenSSH alias, hostname, IP address or user@host. OpenSSH reads your existing configuration to resolve the user, port, identities and proxy settings; HomeStack does not assume root.\n\nF6 tests the current draft asynchronously: ssh -G resolves configuration, then an authenticated SSH session executes only true. Existing askpass prompts support host confirmation, passphrases, PINs and FIDO key touch. Host verification remains enabled; password and keyboard-interactive fallback remain disabled.\n\nThe overall deadline is 45 seconds. Esc cancels; changing the target invalidates verification. Only a successful unchanged test saves its target automatically, with mode 0600. Failure or cancellation does not overwrite the saved target.\n\nHomeStack stores no private SSH keys or authentication secrets. The last successful test and its date are retained when reopening. Editing the target requires a new test. This phase does not inspect or configure Pangolin.",
    "Prepare Pangolin Server on the SSH server verified in Step 1. Confirm both instruction groups only after completing them. This is a manual declaration, not technical verification. No installation, API operation or remote change occurs here.\n\nIn the Pangolin administration interface, open API Keys and create homestack-bootstrap with Root authorization and these actions:\n  createApiKey\n  listApiKeys\n  listApiKeyActions\n  setApiKeyActions\n\nRoot scope is required for Integration API key management; selecting four actions alone is insufficient. These actions create the restricted key, detect conflicts, inspect actions and assign permissions. The temporary administrative key is separate from the permanent, restricted HomeStack key.\n\nKeep its token securely until masked entry in Step 6. Do not enter it here. Revoke it only after the permanent key has been provisioned and verified successfully.\n\nStep 3 will discover the installation method, version, services, configuration paths and ports read-only. It does not assume Docker Compose or fixed paths. The Integration API may remain disabled until Step 4. Checking this box does not verify installation or API availability. Confirmation is saved for the selected server; changing the target requires reconfirmation.",
    "Locked - not implemented.\n\nPangolin discovery will inspect the selected server over authenticated SSH, read-only. It will identify whether Pangolin is installed and running, its version, actual installation method, container and service identities, Compose or service configuration paths, application configuration files, active listening ports and mappings, and the Integration API port and enabled/disabled state.\n\nNo Docker Compose layout, container name or installation path is assumed. Ambiguous or unsupported layouts will be reported and never modified automatically. Later configuration changes belong to Step 4 and require explicit approval and backup.\n\nNo discovery or remote connection is performed by this stage yet.",
    "Locked - not implemented.\n\nA future step will inspect the installed Pangolin version and actual Integration API configuration. If enable_integration_api or a loopback port mapping needs to change, HomeStack will show the exact plan, require approval and back up configuration first.\n\nThe Integration API must not be exposed publicly. This screen currently performs no server inspection, configuration change or service restart.",
    "Locked - not implemented.\n\nA future step will establish an SSH tunnel to the private Integration API endpoint and verify reachability. The tunnel will be owned and managed separately from basic SSH verification.\n\nNo tunnel is opened, no API port is guessed and no API connection is attempted in this phase.",
    "Locked - not implemented.\n\nA future step will accept a temporary bootstrap API credential in a masked field and verify its Root scope and required API-key permissions. The secret must not enter logs, command arguments or repository files.\n\nNo API credential field is enabled now. Step 1 only stores the non-secret SSH destination.",
    "Locked - not implemented.\n\nA future step will create an organization-scoped HomeStack API key with only the permissions required by the integration, verify its assigned permissions and save it securely.\n\nSelective reconfiguration must reuse unrelated existing credentials and avoid duplicate keys or resources. No keys or Pangolin objects are created by the current wizard.",
    "Locked - not implemented.\n\nA future step will independently verify the integration, assigned permissions and safe read-only API operations. It must distinguish observed results from assumptions and user confirmations.\n\nA successful SSH test marks only Step 1 complete (1/8). It does not establish that Pangolin, its API, tunnels or resource routing work.",
];
pub const INSTALLATION: &str = "Pangolin Server must be installed and running on the selected server.\n\nPangolin must already be installed on the SSH server selected in Step 1. No particular installation method is required: Docker Compose, standalone services and other deployment methods may exist. Step 3 will discover the actual installation. The Integration API may still be disabled; Step 4 handles it. Phase 2 makes no remote configuration changes.";
pub const BOOTSTRAP: &str = "Create the temporary homestack-bootstrap Root API key.\n\nIn the Pangolin administration interface, open API Keys and create:\nName: homestack-bootstrap\nAuthorization: Root\nRequired actions:\n  createApiKey\n  listApiKeys\n  listApiKeyActions\n  setApiKeyActions\n\nIntegration API key management requires Root authorization; four actions without Root scope are insufficient. This temporary administrative key is separate from the restricted HomeStack integration key. Keep the token securely until Step 6; do not enter it here. Revoke it after successful provisioning and verification of the permanent key.";
static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

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
        Ok(Self {
            cursor: target.chars().count(),
            saved: target.clone(),
            target,
            path,
            focus: 0,
            expanded: true,
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
    pub fn has_changes(&self) -> bool {
        self.target != self.saved
    }
    pub fn status(&self) -> &'static str {
        if self.testing {
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
        self.revision += 1;
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
        usize::from(self.status() == "Verified") + usize::from(self.prerequisites_confirmed())
    }
    pub fn toggle_prerequisites(&mut self) {
        if !self.prerequisites_available() {
            return;
        }
        let confirmed = !self.prerequisites_confirmed();
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
            persist(&self.path, &table)
        })();
        match result {
            Ok(()) => {
                self.prerequisites_target = confirmed.then(|| self.target.clone());
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
    /// Every stage can be selected for explanation; only Steps 1 and 2 have actions.
    pub fn select_step(&mut self, step: usize) {
        self.selected_step = step.min(STAGES.len() - 1);
        self.focus = 0;
        self.help_scroll = 0;
        self.message.clear();
    }

    /// Field and action focus is independent of step navigation.
    pub fn key(&mut self, key: KeyEvent) -> bool {
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
                KeyCode::Down | KeyCode::PageDown => self.report_scroll += 1,
                KeyCode::Up | KeyCode::PageUp => self.report_scroll = self.report_scroll.saturating_sub(1),
                _ => {}
            }
            return false;
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
                KeyCode::Char(' ') | KeyCode::Enter if self.focus == 3 => {
                    self.toggle_prerequisites();
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
                self.testing = false;
                self.test_id = 0;
                self.verified = None;
                self.message = "SSH test cancelled.".to_owned();
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
                self.report_open = self.report.is_some();
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
            table.remove("prerequisites");
        }
        persist(&self.path, &table)?;
        self.saved = self.target.clone();
        self.confirm_discard = false;
        Ok(())
    }
    pub fn begin_test(&mut self) -> Option<u64> {
        if self.testing || self.selected_step != 0 || self.help_open || self.report_open {
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
    validate_destination(target)?;
    let started = std::time::Instant::now();
    let limit = Duration::from_secs(45);
    let run = |argv: &[String]| -> Result<Output, String> {
        if stop.load(Ordering::SeqCst) {
            return Err("SSH test cancelled.".to_owned());
        }
        let remaining = limit
            .checked_sub(started.elapsed())
            .ok_or("SSH test timed out (45s).")?;
        system.run_until(argv, stop, remaining).map_err(|e| {
            match e.kind() {
                std::io::ErrorKind::TimedOut => "SSH test timed out (45s).",
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
    command.extend(["--".to_owned(), target.to_owned(), "true".to_owned()]);
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
        Ok("PASS · Effective OpenSSH configuration resolved\nPASS · SSH authenticated, remote command successful (true)\nNo Pangolin inspection or server configuration performed.".to_owned())
    })
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
