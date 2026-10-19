//! Desktop DNS credentials, edited locally and saved atomically with mode 0600.
use std::io::Write;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub const PROVIDERS: [(&str, &str); 3] = [
    ("OVH", "ovh-dns"),
    ("Spaceship", "spaceship-dns"),
    ("Local DNS (TSIG)", "dns-tsig.key"),
];
const OVH: &[&str] = &["OVH_AK", "OVH_AS", "OVH_CK", "OVH_END_POINT"];
const SPACESHIP: &[&str] = &["SPACESHIP_API_KEY", "SPACESHIP_API_SECRET"];
const TSIG: &[&str] = &["Key name", "Algorithm", "Secret"];
const TEST_ZONE: &str = "Test zone";
const TEST_SERVER: &str = "DNS server (IP)";
static NEXT_TEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Persisted metadata contains no credentials or raw provider responses.
#[derive(Debug, Clone, PartialEq)]
pub struct TestReport {
    pub provider: usize,
    pub at: String,
    pub zone: String,
    pub target: String,
    pub passed: bool,
    pub duration_ms: u64,
    pub detail: String,
}

impl TestReport {
    pub fn completed(editor: &Editor, result: Result<String, String>, duration_ms: u64) -> Self {
        Self {
            provider: editor.provider,
            at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            zone: editor.zone().to_owned(),
            target: match editor.provider {
                0 => editor.values[3].clone(),
                1 => "https://spaceship.dev/api/v1".to_owned(),
                _ => editor.values[4].clone(),
            },
            passed: result.is_ok(),
            duration_ms,
            detail: editor.safe_error(&result.unwrap_or_else(|error| error)),
        }
    }

    fn from_table(provider: usize, value: &toml::Value) -> Option<Self> {
        Some(Self {
            provider,
            at: value.get("at")?.as_str()?.to_owned(),
            zone: value.get("zone")?.as_str()?.to_owned(),
            target: value.get("target")?.as_str()?.to_owned(),
            passed: value.get("passed")?.as_bool()?,
            duration_ms: u64::try_from(value.get("duration_ms")?.as_integer()?).ok()?,
            detail: value.get("detail")?.as_str()?.to_owned(),
        })
    }

    pub fn save(&self, preferences: &mut super::theme::Preferences) -> Result<(), String> {
        let mut table = preferences.table.clone();
        let tests = table
            .entry("integration_tests")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or("Invalid integration test settings")?;
        let provider = tests
            .entry(PROVIDERS[self.provider].1)
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or("Invalid provider test settings")?;
        let mut report = toml::Table::new();
        for (key, value) in [
            ("at", &self.at),
            ("zone", &self.zone),
            ("target", &self.target),
            ("detail", &self.detail),
        ] {
            report.insert(key.to_owned(), toml::Value::String(value.clone()));
        }
        report.insert("passed".to_owned(), toml::Value::Boolean(self.passed));
        report.insert(
            "duration_ms".to_owned(),
            toml::Value::Integer(i64::try_from(self.duration_ms).unwrap_or(i64::MAX)),
        );
        provider.insert("last_test".to_owned(), toml::Value::Table(report));
        preferences
            .write(&table)
            .map_err(|_| "Cannot save the connection test report".to_owned())?;
        preferences.table = table;
        Ok(())
    }
}

#[derive(Clone, PartialEq)]
pub struct Editor {
    pub provider: usize,
    pub path: PathBuf,
    pub values: Vec<String>,
    pub focus: usize,
    pub cursor: usize,
    pub visible: bool,
    pub message: String,
    pub test_id: u64,
    pub testing: bool,
    pub report: Option<TestReport>,
    pub report_open: bool,
    pub report_scroll: usize,
    original: String,
    dirty: bool,
    confirm_discard: bool,
}

// Debug output must never contain credentials.
impl std::fmt::Debug for Editor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Editor")
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

impl Editor {
    fn credential_labels(&self) -> &'static [&'static str] {
        match self.provider {
            0 => OVH,
            1 => SPACESHIP,
            _ => TSIG,
        }
    }

    pub fn labels(&self) -> Vec<&'static str> {
        let mut labels = self.credential_labels().to_vec();
        labels.push(TEST_ZONE);
        if self.provider == 2 {
            labels.push(TEST_SERVER);
        }
        labels
    }

    pub fn is_secret(&self, at: usize) -> bool {
        match self.provider {
            0 => at < 3,
            1 => at < 2,
            _ => at == 2,
        }
    }

    pub fn open(dir: &Path, provider: usize) -> Result<Self, String> {
        let path = dir.join(PROVIDERS[provider].1);
        let original = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(_) => return Err("Cannot read credentials file".to_owned()),
        };
        let mut editor = Self {
            provider,
            path,
            values: Vec::new(),
            focus: 0,
            cursor: 0,
            visible: false,
            message: String::new(),
            original,
            test_id: 0,
            testing: false,
            report: None,
            report_open: false,
            report_scroll: 0,
            dirty: false,
            confirm_discard: false,
        };
        editor.values = if provider == 2 {
            if editor.original.trim().is_empty() {
                vec![String::new(), "hmac-sha256".to_owned(), String::new()]
            } else {
                parse_tsig(&editor.original)?
            }
        } else {
            editor
                .credential_labels()
                .iter()
                .map(|key| {
                    editor
                        .original
                        .lines()
                        .filter_map(|line| line.trim().split_once('='))
                        .find(|(name, _)| name.trim() == *key)
                        .map(|(_, value)| value.trim().trim_matches('"').to_owned())
                        .unwrap_or_else(|| {
                            if *key == "OVH_END_POINT" {
                                "ovh-eu".to_owned()
                            } else {
                                String::new()
                            }
                        })
                })
                .collect()
        };
        let preferences =
            super::theme::Preferences::load(&dir.join("tui.toml")).map_err(|_| "Cannot read test settings")?;
        let target = preferences
            .table
            .get("integration_tests")
            .and_then(|value| value.get(PROVIDERS[provider].1));
        editor.report = target
            .and_then(|value| value.get("last_test"))
            .and_then(|value| TestReport::from_table(provider, value));
        editor.values.push(
            target
                .and_then(|value| value.get("zone"))
                .and_then(toml::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        );
        if provider == 2 {
            editor.values.push(
                target
                    .and_then(|value| value.get("server"))
                    .and_then(toml::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            );
        }
        editor.cursor = editor.values[0].chars().count();
        Ok(editor)
    }

    pub fn select(&mut self, field: usize) {
        self.focus = field.min(self.values.len());
        self.cursor = self.values.get(self.focus).map_or(0, |value| value.chars().count());
    }

    pub fn paste(&mut self, text: &str) {
        if self.on_test_button() || self.report_open {
            return;
        }
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        let byte = self.values[self.focus]
            .char_indices()
            .nth(self.cursor)
            .map_or(self.values[self.focus].len(), |(at, _)| at);
        self.values[self.focus].insert_str(byte, &clean);
        self.cursor += clean.chars().count();
        self.changed();
    }

    fn changed(&mut self) {
        self.dirty = true;
        self.testing = false;
        self.test_id = 0;
        self.confirm_discard = false;
        self.message.clear();
    }

    pub fn on_test_button(&self) -> bool {
        self.focus == self.values.len()
    }

    /// Return true to leave. F4 toggles disclosure; F5 saves without exposing values.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        if self.on_test_button()
            && !matches!(
                key.code,
                KeyCode::Esc
                    | KeyCode::F(4)
                    | KeyCode::F(5)
                    | KeyCode::Tab
                    | KeyCode::Down
                    | KeyCode::Enter
                    | KeyCode::BackTab
                    | KeyCode::Up
            )
        {
            return false;
        }
        match key.code {
            KeyCode::Esc if !self.dirty || self.confirm_discard => return true,
            KeyCode::Esc => {
                self.visible = false;
                self.confirm_discard = true;
                self.message = "Unsaved changes. Esc again to discard; F5 to save.".to_owned();
            }
            KeyCode::F(4) => self.visible = !self.visible,
            KeyCode::F(5) => {
                self.visible = false;
                self.message = match self.save() {
                    Ok(()) => "Saved".to_owned(),
                    Err(error) => error,
                };
            }
            KeyCode::Tab | KeyCode::Down | KeyCode::Enter => self.select((self.focus + 1) % (self.values.len() + 1)),
            KeyCode::BackTab | KeyCode::Up => self.select((self.focus + self.values.len()) % (self.values.len() + 1)),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.values[self.focus].chars().count(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.values[self.focus].chars().count()),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.values[self.focus].clear();
                self.cursor = 0;
                self.changed();
            }
            KeyCode::Backspace | KeyCode::Delete => {
                let mut chars: Vec<char> = self.values[self.focus].chars().collect();
                let at = if key.code == KeyCode::Backspace {
                    self.cursor.checked_sub(1)
                } else {
                    Some(self.cursor)
                };
                if let Some(at) = at.filter(|at| *at < chars.len()) {
                    chars.remove(at);
                    self.cursor = self.cursor.min(at);
                    self.values[self.focus] = chars.into_iter().collect();
                    self.changed();
                }
            }
            KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                self.paste(&c.to_string())
            }
            _ => {}
        }
        false
    }

    pub fn validate(&self) -> Result<(), String> {
        for (label, value) in self.credential_labels().iter().zip(&self.values) {
            if value.is_empty() {
                return Err(format!("{label} is required"));
            }
            if value.chars().any(|c| c.is_control() || matches!(c, '"' | '\\')) {
                return Err(format!("{label} contains unsupported characters"));
            }
        }
        if self.provider == 0 {
            // The parser's unknown-endpoint error includes the input: never display it.
            hs_deploy::ovh::OvhCredentials::parse(&self.serialize())
                .map_err(|_| "Check the OVH endpoint (ovh-eu, ovh-ca, ovh-us or HTTPS URL)".to_owned())?;
        }
        if self.provider == 2 {
            use base64::Engine;
            if !self.values[1].bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
                return Err("Invalid TSIG algorithm".to_owned());
            }
            if base64::engine::general_purpose::STANDARD
                .decode(&self.values[2])
                .is_err()
            {
                return Err("TSIG secret must be base64".to_owned());
            }
        }
        Ok(())
    }

    fn serialize(&self) -> String {
        if self.provider == 2 {
            return format!(
                "key \"{}\" {{\n    algorithm {};\n    secret \"{}\";\n}};\n",
                self.values[0], self.values[1], self.values[2]
            );
        }
        // Preserve comments and unrelated entries; replace every occurrence of edited keys.
        let mut seen = vec![false; self.credential_labels().len()];
        let mut lines = Vec::new();
        for line in self.original.lines() {
            if let Some(at) = line
                .trim()
                .split_once('=')
                .and_then(|(name, _)| self.credential_labels().iter().position(|key| *key == name.trim()))
            {
                if !seen[at] {
                    lines.push(format!("{}=\"{}\"", self.credential_labels()[at], self.values[at]));
                }
                seen[at] = true;
            } else {
                lines.push(line.to_owned());
            }
        }
        for (at, present) in seen.iter().enumerate() {
            if !present {
                lines.push(format!("{}=\"{}\"", self.credential_labels()[at], self.values[at]));
            }
        }
        format!("{}\n", lines.join("\n"))
    }

    pub fn safe_error(&self, error: &str) -> String {
        let mut text = error.to_owned();
        let mut secrets: Vec<&str> = self
            .values
            .iter()
            .enumerate()
            .filter(|(at, value)| self.is_secret(*at) && !value.is_empty())
            .map(|(_, value)| value.as_str())
            .collect();
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        for secret in secrets {
            text = text.replace(secret, "[redacted]");
        }
        text.chars()
            .filter(|c| !c.is_control() || *c == '\n')
            .take(2000)
            .collect()
    }

    pub fn zone(&self) -> &str {
        &self.values[self.credential_labels().len()]
    }

    pub fn begin_test(&mut self) -> Option<u64> {
        if self.testing {
            return None;
        }
        self.visible = false;
        if let Err(error) = self.validate_test() {
            self.message = error;
            return None;
        }
        self.test_id = NEXT_TEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.testing = true;
        self.message = "Testing authentication and zone access…".to_owned();
        Some(self.test_id)
    }

    fn validate_test(&self) -> Result<(), String> {
        self.validate()?;
        let zone = self.zone().trim_end_matches('.');
        if zone.is_empty()
            || zone.len() > 253
            || !zone.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            })
        {
            return Err("Enter a DNS zone to test".to_owned());
        }
        if self.provider == 2 && self.values[4].parse::<std::net::IpAddr>().is_err() {
            return Err("Enter the authoritative DNS server IP address".to_owned());
        }
        Ok(())
    }

    pub async fn test_connection(&self) -> Result<String, String> {
        self.validate_test()?;
        let check = async {
            match self.provider {
                0 => {
                    let credentials = hs_deploy::ovh::OvhCredentials::parse(&self.serialize())
                        .map_err(|_| "Invalid OVH credentials")?;
                    let client = hs_deploy::ovh::OvhClient::connect(credentials)
                        .await
                        .map_err(|error| self.safe_error(&error))?;
                    client
                        .check_zone(self.zone().trim_end_matches('.'))
                        .await
                        .map_err(|error| self.safe_error(&error))?;
                }
                1 => {
                    let credentials = hs_deploy::spaceship::SpaceshipCredentials::parse(&self.serialize())
                        .map_err(|_| "Invalid Spaceship credentials")?;
                    let client = hs_deploy::spaceship::SpaceshipClient::new(credentials)
                        .map_err(|_| "Cannot create Spaceship client")?;
                    client
                        .check_zone(self.zone().trim_end_matches('.'))
                        .await
                        .map_err(|error| self.safe_error(&error))?;
                }
                _ => {
                    let editor = self.clone();
                    tokio::task::spawn_blocking(move || editor.test_tsig())
                        .await
                        .map_err(|_| "DNS test interrupted")??;
                }
            }
            Ok(match self.provider {
                0 => format!("PASS · API connection and clock synchronization\nPASS · Signed GET /domain/zone/{}\nPASS · Credentials accepted and zone read access verified", self.zone()),
                1 => format!("PASS · HTTPS API connection\nPASS · GET /dns/records/{}?take=1&skip=0\nPASS · Credentials accepted and zone read access verified", self.zone()),
                _ => "PASS · DNS server connection\nPASS · TSIG response signature\nPASS · Authoritative SOA answer for the requested zone".to_owned(),
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(15), check)
            .await
            .map_err(|_| "Connection test timed out".to_owned())?
    }

    fn test_tsig(&self) -> Result<(), String> {
        let mut key = tempfile::NamedTempFile::new().map_err(|_| "Cannot prepare TSIG test")?;
        key.write_all(self.serialize().as_bytes())
            .map_err(|_| "Cannot prepare TSIG test")?;
        let output = std::process::Command::new("dig")
            .arg("-r")
            .arg("-k")
            .arg(key.path())
            .arg(format!("@{}", self.values[4]))
            .arg(self.zone())
            .args([
                "SOA",
                "+norecurse",
                "+tcp",
                "+time=5",
                "+tries=1",
                "+noall",
                "+comments",
                "+answer",
                "+additional",
            ])
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    "Install dig to test TSIG connections"
                } else {
                    "Cannot run DNS test"
                }
            })?;
        verify_tsig_response(
            output.status.success(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    pub fn save(&mut self) -> Result<(), String> {
        self.validate()?;
        let parent = self.path.parent().ok_or("Invalid credentials path")?;
        let text = self.serialize();
        let mut preferences =
            super::theme::Preferences::load(&parent.join("tui.toml")).map_err(|_| "Cannot read test settings")?;
        let mut tests = preferences
            .table
            .get("integration_tests")
            .and_then(toml::Value::as_table)
            .cloned()
            .unwrap_or_default();
        let mut target = tests
            .get(PROVIDERS[self.provider].1)
            .and_then(toml::Value::as_table)
            .cloned()
            .unwrap_or_default();
        target.insert("zone".to_owned(), toml::Value::String(self.zone().to_owned()));
        if self.provider == 2 {
            target.insert("server".to_owned(), toml::Value::String(self.values[4].clone()));
        }
        tests.insert(PROVIDERS[self.provider].1.to_owned(), toml::Value::Table(target));
        preferences
            .table
            .insert("integration_tests".to_owned(), toml::Value::Table(tests));
        preferences
            .write(&preferences.table)
            .map_err(|_| "Cannot save test settings")?;
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(parent)?;
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            // NamedTempFile creates private files (0600), including when replacing a public file.
            file.write_all(text.as_bytes())?;
            file.as_file().sync_all()?;
            file.persist(&self.path).map_err(|error| error.error)?;
            Ok(())
        };
        write().map_err(|_| "Cannot save credentials file".to_owned())?;
        self.original = text;
        self.dirty = false;
        self.confirm_discard = false;
        Ok(())
    }
}

/// Accept one BIND key block. Refuse unfamiliar syntax instead of destroying it.
fn parse_tsig(text: &str) -> Result<Vec<String>, String> {
    let error = || "Unsupported TSIG file: expected one BIND key block".to_owned();
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '#' => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '"' => {
                let mut token = String::new();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') | None => return Err(error()),
                        Some(c) => token.push(c),
                    }
                }
                tokens.push(token);
            }
            '{' | '}' | ';' => tokens.push(c.to_string()),
            _ => {
                let mut token = c.to_string();
                while let Some(c) = chars.peek().copied() {
                    if c.is_whitespace() || matches!(c, '{' | '}' | ';') {
                        break;
                    }
                    token.push(c);
                    chars.next();
                }
                tokens.push(token);
            }
        }
    }
    if tokens.len() != 11
        || tokens[0] != "key"
        || tokens[2] != "{"
        || tokens[3] != "algorithm"
        || tokens[5] != ";"
        || tokens[6] != "secret"
        || tokens[8] != ";"
        || tokens[9] != "}"
        || tokens[10] != ";"
    {
        return Err(error());
    }
    Ok(vec![tokens[1].clone(), tokens[4].clone(), tokens[7].clone()])
}

/// Require a verified signature and an authoritative SOA answer, not just a reachable DNS server.
pub fn verify_tsig_response(success: bool, stdout: &str, stderr: &str) -> Result<(), String> {
    for (code, reason) in [
        ("BADKEY", "the server does not recognize the TSIG key name or algorithm"),
        ("BADSIG", "the TSIG secret does not match the server"),
        ("BADTIME", "the client and DNS server clocks differ"),
    ] {
        if stdout.contains(code) || stderr.contains(code) {
            return Err(format!("TSIG {code}: {reason}"));
        }
    }
    if stdout.contains("status: REFUSED") {
        return Err("DNS REFUSED: check zone access and server permissions".to_owned());
    }
    if !success
        || !stderr.trim().is_empty()
        || stdout.to_ascii_lowercase().contains("couldn't verify")
        || stdout.contains("BADKEY")
        || stdout.contains("BADSIG")
        || stdout.contains("BADTIME")
        || !stdout.contains("status: NOERROR")
    {
        return Err("DNS test failed: check server, zone, TSIG key and permissions".to_owned());
    }
    let authoritative = stdout
        .lines()
        .filter_map(|line| line.split_once("flags:"))
        .any(|(_, flags)| {
            flags
                .split(';')
                .next()
                .is_some_and(|flags| flags.split_whitespace().any(|flag| flag == "aa"))
        });
    let soa = stdout
        .lines()
        .filter(|line| !line.starts_with(';'))
        .any(|line| line.split_whitespace().any(|word| word == "SOA"));
    if !stdout.contains("TSIG PSEUDOSECTION") || !authoritative || !soa {
        return Err("DNS server did not return a signed authoritative SOA answer".to_owned());
    }
    Ok(())
}
