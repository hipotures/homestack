//! Phase 1 of Pangolin setup: non-secret target and read-only SSH verification.
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
    "SSH connection",
    "Integration API",
    "API tunnel",
    "Bootstrap API key",
    "HomeStack API key",
    "Integration verification",
];
pub const DESCRIPTIONS: [&str; 8] = [
    "Select and test an existing OpenSSH destination.",
    "Future: confirm installation and preparation of a Root bootstrap key.",
    "Future: verify administrative SSH access for server configuration.",
    "Future: inspect API configuration; approve and back up any changes.",
    "Future: tunnel to the private Integration API and verify reachability.",
    "Future: accept a masked temporary key and verify its permissions.",
    "Future: provision and securely save a least-privilege organization key.",
    "Future: verify integration permissions and read-only operations.",
];
/// Contextual help for locked stages describes future work, not a working operation.
pub const HELP: [&str; 8] = [
    "Select an OpenSSH alias, hostname, IP address or user@host. OpenSSH reads your existing configuration to resolve the user, port, identities and proxy settings; HomeStack does not assume root.\n\nF6 tests the current draft asynchronously: ssh -G resolves configuration, then an authenticated SSH session executes only true. Existing askpass prompts support host confirmation, passphrases, PINs and FIDO key touch. Host verification remains enabled; password and keyboard-interactive fallback remain disabled.\n\nThe overall deadline is 45 seconds. Esc cancels; changing the target invalidates verification. Only a successful unchanged test saves its target automatically, with mode 0600. Failure or cancellation does not overwrite the saved target.\n\nHomeStack stores no private SSH keys or authentication secrets. The last successful test and its date are retained when reopening. Editing the target requires a new test. This phase does not inspect or configure Pangolin.",
    "Locked - not implemented in Phase 1.\n\nThe future prerequisite step will explain how to install Pangolin and prepare a temporary Root API key named homestack-bootstrap with createApiKey, listApiKeys, listApiKeyActions and setApiKeyActions.\n\nA future confirmation checkbox will record that the user completed these preparations. User confirmation will not be presented as technical verification. No checkbox, key input or remote operation is enabled now.",
    "Locked - not implemented in Phase 1.\n\nA future step will establish the administrative SSH session required for server-side configuration. It will reuse existing OpenSSH configuration and HomeStack authentication prompts, including hardware keys.\n\nThe current Step 1 test verifies basic authentication and harmless command execution only; it does not verify administrator privileges or inspect the Pangolin installation.",
    "Locked - not implemented in Phase 1.\n\nA future step will inspect the installed Pangolin version and actual Integration API configuration. If enable_integration_api or a loopback port mapping needs to change, HomeStack will show the exact plan, require approval and back up configuration first.\n\nThe Integration API must not be exposed publicly. This screen currently performs no server inspection, configuration change or service restart.",
    "Locked - not implemented in Phase 1.\n\nA future step will establish an SSH tunnel to the private Integration API endpoint and verify reachability. The tunnel will be owned and managed separately from basic SSH verification.\n\nNo tunnel is opened, no API port is guessed and no API connection is attempted in this phase.",
    "Locked - not implemented in Phase 1.\n\nA future step will accept a temporary bootstrap API credential in a masked field and verify its Root scope and required API-key permissions. The secret must not enter logs, command arguments or repository files.\n\nNo API credential field is enabled now. Step 1 only stores the non-secret SSH destination.",
    "Locked - not implemented in Phase 1.\n\nA future step will create an organization-scoped HomeStack API key with only the permissions required by the integration, verify its assigned permissions and save it securely.\n\nSelective reconfiguration must reuse unrelated existing credentials and avoid duplicate keys or resources. No keys or Pangolin objects are created by the current wizard.",
    "Locked - not implemented in Phase 1.\n\nA future step will independently verify the integration, assigned permissions and safe read-only API operations. It must distinguish observed results from assumptions and user confirmations.\n\nA successful SSH test marks only Step 1 complete (1/8). It does not establish that Pangolin, its API, tunnels or resource routing work.",
];
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
        Ok(Self {
            cursor: target.chars().count(),
            saved: target.clone(),
            target,
            path,
            focus: 0,
            expanded: true,
            selected_step: 0,
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
    /// Every stage can be selected for explanation; only Step 1 has actions.
    pub fn select_step(&mut self, step: usize) {
        self.selected_step = step.min(STAGES.len() - 1);
        self.focus = 0;
        self.help_scroll = 0;
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
            KeyCode::Home if self.focus == 1 => self.cursor = 0,
            KeyCode::End if self.focus == 1 => self.cursor = self.target.chars().count(),
            KeyCode::Left if self.focus == 1 => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right if self.focus == 1 => self.cursor = (self.cursor + 1).min(self.target.chars().count()),
            KeyCode::Char('u') if self.focus == 1 && key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.target.clear();
                self.cursor = 0;
                self.changed();
            }
            KeyCode::Backspace | KeyCode::Delete if self.focus == 1 => {
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
        let save = || -> Result<(), Box<dyn std::error::Error>> {
            let parent = self.path.parent().ok_or("Invalid integration path")?;
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            file.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(toml::to_string_pretty(&table)?.as_bytes())?;
            file.as_file().sync_all()?;
            file.persist(&self.path)?;
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        };
        save().map_err(|_| "Cannot save Pangolin server settings".to_owned())?;
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
