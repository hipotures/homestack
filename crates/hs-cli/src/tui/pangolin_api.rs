//! Private Integration API inspection and explicitly approved configuration.
use super::pangolin::{Discovery, Wizard};
use crate::system::System;
use serde::{Deserialize, Serialize};
use std::sync::atomic::AtomicBool;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Api {
    pub target: String,
    pub installation: String,
    pub at: String,
    pub status: String,
    pub code: String,
    pub changed: String,
    pub backup: String,
    pub endpoint: String,
    pub access: String,
    pub port: u16,
    pub enabled: bool,
    pub running: bool,
    #[serde(skip)]
    pub plan: Option<Plan>,
    #[serde(skip)]
    pub refreshed: Option<Discovery>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub identity: String,
    pub image_id: String,
    pub config: String,
    pub compose: String,
    pub service: String,
    pub project: String,
    pub workdir: String,
    pub files: Vec<File>,
    pub edits: Vec<String>,
    pub backup: String,
    pub effective: String,
    pub host_port: u16,
    pub port: u16,
    pub external: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub path: String,
    pub before: String,
    pub after: String,
    pub device: u64,
    pub inode: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}
impl Api {
    pub fn failure(target: &str, installation: &str, code: &str) -> Self {
        Self {
            target: target.to_owned(),
            installation: installation.to_owned(),
            at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            status: "Failed".into(),
            code: code.into(),
            changed: "No changes applied".into(),
            backup: String::new(),
            endpoint: String::new(),
            access: "Not inspected".into(),
            port: 0,
            enabled: false,
            running: false,
            plan: None,
            refreshed: None,
        }
    }
    pub fn explanation(&self) -> &'static str {
        match self.code.as_str() {
            "healthy" => "Private Integration API health: PASS. No API credentials used.",
            "plan" => "Review the complete plan (F7). F5 opens confirmation; no changes made yet.",
            "permission" | "write_permission" => {
                "Configuration access denied. Use an authorized SSH account or adjust its permissions; HomeStack never elevates privileges."
            }
            "stale" => "Installation or configuration changed. Inspect again before applying a new plan.",
            "compose" | "compose_required" => {
                "Automatic editing requires an unambiguous single-file Docker Compose project. Configure a private API endpoint independently, then inspect again."
            }
            "yaml" => {
                "Unsupported YAML. Use literal flags.enable_integration_api and server.integration_port settings, then inspect again."
            }
            "yaml_parser" => {
                "Remote PyYAML is unavailable. Automatic editing requires a safe YAML parser; no packages were installed. Configure the API independently or provide the parser, then retry."
            }
            "mount" => {
                "Configuration must use a verified directory bind mount. Single-file mounts and ambiguous volumes cannot be safely replaced automatically."
            }
            "path" => "Unsafe configuration path, symlink or shared inode. Automatic editing refused.",
            "discovery" => "Installation changed or is not running. Refresh Step 3 discovery before continuing.",
            "port_busy" => {
                "The proposed host port is already occupied. Resolve the conflict independently and inspect again."
            }
            "backup" => {
                "Backup creation failed. No configuration replacement was attempted. Check available space and directory permissions."
            }
            "validation" => {
                "Proposed Compose configuration failed validation. Live configuration was not replaced; protected backups were retained."
            }
            "restart" => {
                "Pangolin service activation failed. Configuration may have changed; inspect the backup reference and service before retrying. No rollback was attempted."
            }
            "health" => {
                "Private Pangolin health verification failed. Inspect service state and configured port; no automatic retry of mutations or rollback."
            }
            "timeout" => "Operation timed out. Inspect current service state before retrying.",
            "cancelled" => "Operation cancelled. Inspect again when ready.",
            "ssh_auth" => "SSH authentication failed. Check the selected identity and access.",
            "ssh" => {
                "SSH connection or remote execution failed. Check host verification, reachability and remote Python 3 availability."
            }
            "uncertain" => {
                "SSH operation interrupted. Remote changes may have occurred. Inspect the service and protected backup reference before any retry; no rollback was attempted."
            }
            _ => {
                "Inspection failed safely; raw remote output was omitted. Check runtime access and supported configuration layout."
            }
        }
    }
    pub fn text(&self, offset: i64) -> String {
        let date = super::pangolin::Report {
            target: self.target.clone(),
            at: self.at.clone(),
            passed: false,
            detail: String::new(),
            duration_ms: 0,
        }
        .local_date(offset);
        let mut text = format!(
            "Server: {}\nStatus: {}\nLast inspection: {} (historical)\n{}\n{}",
            self.target,
            self.status,
            date,
            self.explanation(),
            self.changed
        );
        if self.port > 0 {
            text.push_str(&format!(
                "\nConfigured API port: {}\nEndpoint to verify: {}\nAccess: {}\nAPI enabled: {}\nRuntime running: {}",
                self.port, self.endpoint, self.access, self.enabled, self.running
            ));
        }
        if !self.backup.is_empty() {
            text.push_str(&format!("\nProtected remote backup: {}\nRecovery: original files and recovery.json remain on the VPS. Restore only after inspecting current hashes and explicitly approving service interruption; no automatic rollback.", self.backup));
        }
        if let Some(p) = &self.plan {
            text.push_str(&format!("\n\nPROPOSED PLAN (not executed)\nService: {}\nProject: {}\nProject directory: {}\nConfiguration: {}\nCompose: {}", p.service, p.project, p.workdir, p.config, p.compose));
            for edit in &p.edits {
                text.push_str(&format!("\n  {edit}"));
            }
            for f in p.files.iter().filter(|f| f.before != f.after) {
                text.push_str(&format!("\nFile to modify: {}", f.path));
            }
            text.push_str(&format!("\nBackup directory (created only after approval): {}\nRestart/recreate only Pangolin service: {}\nImpact: temporary Pangolin interruption; Sites, Resources and active sessions may be interrupted.\nVerification: revalidate files/runtime; validate full effective Compose model; recreate only this service without dependencies, builds or pulls; verify loopback publication and GET /v1/ Healthy.\nExisting external API connectivity will be removed: {}\nNo keys, tunnels, proxies, DNS or firewall changes.", p.backup, p.service, p.external));
        }
        text
    }
    pub fn store(&self) -> Result<toml::Value, String> {
        toml::Value::try_from(self).map_err(|_| "Cannot serialize non-secret API verification metadata.".into())
    }
    pub fn restore(value: &toml::Value) -> Option<Self> {
        let report: Self = value.clone().try_into().ok()?;
        report.valid().then_some(report)
    }
    fn valid(&self) -> bool {
        let safe = |s: &str| s.len() <= 512 && !s.chars().any(char::is_control);
        matches!(
            self.status.as_str(),
            "Verified" | "Changes required" | "Failed" | "Permission denied" | "Outdated" | "Cancelled"
        ) && matches!(
            self.access.as_str(),
            "Not inspected" | "Externally published" | "Loopback only" | "Not published on IPv4 loopback"
        ) && [
            self.target.as_str(),
            &self.installation,
            &self.at,
            &self.backup,
            &self.endpoint,
        ]
        .into_iter()
        .all(safe)
            && matches!(
                self.code.as_str(),
                "healthy"
                    | "plan"
                    | "permission"
                    | "write_permission"
                    | "stale"
                    | "compose"
                    | "compose_required"
                    | "yaml"
                    | "yaml_parser"
                    | "mount"
                    | "path"
                    | "discovery"
                    | "port_busy"
                    | "backup"
                    | "validation"
                    | "restart"
                    | "health"
                    | "timeout"
                    | "cancelled"
                    | "ssh_auth"
                    | "ssh"
                    | "uncertain"
                    | "unsupported"
                    | "runtime"
                    | "unexpected"
            )
            && matches!(
                self.changed.as_str(),
                "No changes applied"
                    | "Configuration changed; service activation not verified"
                    | "Changes applied; service health not verified"
                    | "Changes applied and private health verified"
                    | "Remote outcome uncertain; inspect before retrying"
            )
            && (self.status != "Verified"
                || (self.port > 0
                    && self.enabled
                    && self.running
                    && self.code == "healthy"
                    && self.access == "Loopback only"
                    && self.endpoint.starts_with("http://127.0.0.1:")
                    && self.endpoint.ends_with("/v1/")))
    }
}

impl Wizard {
    pub(super) fn api_failure(&self, code: &str) -> Api {
        Api::failure(
            &self.target,
            &self.discovery.as_ref().map(installation).unwrap_or_default(),
            code,
        )
    }
    pub fn api_available(&self) -> bool {
        self.discovery_status() == "Discovered"
    }
    pub fn api_status(&self) -> &str {
        if self.api_running {
            return if self.api_applying { "Applying" } else { "Inspecting" };
        }
        match &self.api {
            Some(r)
                if !self.api_available()
                    || r.target != self.target
                    || self
                        .discovery
                        .as_ref()
                        .is_none_or(|d| r.installation != installation(d)) =>
            {
                "Outdated"
            }
            Some(r) => &r.status,
            None => "Not verified",
        }
    }
    pub fn can_apply_api(&self) -> bool {
        !self.testing && self.api_status() == "Changes required" && self.api.as_ref().is_some_and(|a| a.plan.is_some())
    }
    pub fn begin_api(&mut self, apply: bool) -> Option<u64> {
        if self.testing
            || self.selected_step != 3
            || !self.api_available()
            || self.help_open
            || self.report_open
            || (apply && (!self.can_apply_api() || !self.api_confirm || !self.api_confirm_yes))
        {
            return None;
        }
        if apply {
            let mut pending = self.api.clone()?;
            pending.backup = pending.plan.as_ref()?.backup.clone();
            pending.status = "Failed".into();
            pending.code = "uncertain".into();
            pending.changed = "Remote outcome uncertain; inspect before retrying".into();
            if let Err(error) = self.persist_api(&pending) {
                self.message = error;
                self.api_confirm = false;
                return None;
            }
        }
        self.test_id = super::pangolin::next_test();
        self.testing = true;
        self.api_running = true;
        self.api_applying = apply;
        self.api_confirm = false;
        self.message.clear();
        Some(self.test_id)
    }
    pub fn complete_api(&mut self, id: u64, mut report: Api) {
        if !self.api_running || id != self.test_id || report.target != self.target || !self.api_available() {
            return;
        }
        self.testing = false;
        self.api_running = false;
        self.api_applying = false;
        if let Some(d) = &report.refreshed {
            report.installation = installation(d);
        }
        if let Err(error) = self.persist_api(&report) {
            report.status = "Failed".into();
            report.plan = None;
            self.message = error;
        }
        if report.status == "Verified"
            && let Some(d) = report.refreshed.take()
        {
            self.discovery = Some(d);
        }
        self.api = Some(report);
    }
    pub fn invalidate_api(&mut self) {
        if let Some(api) = self.api.as_mut() {
            api.status = "Outdated".into();
            api.plan = None;
        }
        self.api_confirm = false;
    }
}
fn installation(d: &Discovery) -> String {
    format!("{}|{}|{}", d.fact("identity"), d.fact("config"), d.at)
}

#[allow(clippy::too_many_arguments)]
pub fn execute(
    system: &dyn System,
    target: &str,
    discovery: &Discovery,
    approved: Option<&Plan>,
    prefix: &[String],
    stop: &AtomicBool,
    sounds: &crate::touch::Sounds,
    player: crate::touch::Player,
) -> Api {
    let identity = installation(discovery);
    let expected = serde_json::to_string(&discovery.facts).unwrap_or_default();
    let approved_json = serde_json::to_string(&approved).unwrap_or_default();
    let source = format!(
        "PROBE = {}\nEXPECTED = __import__('json').loads({})\nAPPROVED = __import__('json').loads({})\n{}",
        serde_json::to_string(include_str!("pangolin_discover.py")).unwrap_or_default(),
        serde_json::to_string(&expected).unwrap_or_default(),
        serde_json::to_string(&approved_json).unwrap_or_default(),
        include_str!("pangolin_api.py")
    );
    let command = format!("python3 -B -c '{}'", source.replace('\'', "'\\''"));
    let (response, _) = super::pangolin::ssh_command(
        system,
        target,
        prefix,
        stop,
        sounds,
        player,
        &command,
        std::time::Duration::from_secs(180),
    );
    let parsed = response.as_ref().ok().and_then(|text| {
        if text.len() > 32768 {
            return None;
        }
        let mut value: serde_json::Value = serde_json::from_str(text).ok()?;
        let inventory = value.as_object_mut()?.remove("inventory").filter(|v| !v.is_null());
        let refreshed = inventory
            .map(|v| Discovery::parse(target, &v.to_string()))
            .transpose()
            .ok()?;
        let plan = value
            .as_object_mut()?
            .remove("plan")
            .filter(|p| !p.is_null())
            .map(serde_json::from_value::<Plan>)
            .transpose()
            .ok()?;
        value["target"] = target.into();
        value["installation"] = identity.clone().into();
        value["at"] = Api::failure(target, &identity, "unexpected").at.into();
        let mut report: Api = serde_json::from_value(value).ok()?;
        report.plan = plan;
        report.refreshed = refreshed;
        if let Some(p) = &report.plan
            && (p.files.len() != 2
                || p.edits.len() > 3
                || p.edits.iter().any(|s| s.len() > 256 || s.chars().any(char::is_control))
                || [&p.config, &p.compose, &p.workdir, &p.backup, &p.service, &p.project]
                    .iter()
                    .any(|s| s.len() > 512 || s.chars().any(char::is_control)))
        {
            return None;
        }
        if let Some(d) = &report.refreshed
            && (d.outcome != "Discovered"
                || d.fact("config") != discovery.fact("config")
                || d.fact("compose_project") != discovery.fact("compose_project"))
        {
            return None;
        }
        if approved.is_some() && report.status == "Verified" && report.refreshed.is_none() {
            return None;
        }
        report.valid().then_some(report)
    });
    parsed.unwrap_or_else(|| {
        let error = response.err().unwrap_or_default();
        let code = if error.contains("authentication failed") {
            "ssh_auth"
        } else if approved.is_some() {
            "uncertain"
        } else if stop.load(std::sync::atomic::Ordering::SeqCst) {
            "cancelled"
        } else if error.contains("timed out") {
            "timeout"
        } else {
            "ssh"
        };
        let mut report = Api::failure(target, &identity, code);
        if code == "uncertain"
            && let Some(p) = approved
        {
            report.backup = p.backup.clone();
            report.changed = "Remote outcome uncertain; inspect before retrying".into();
        }
        if code == "cancelled" {
            report.status = "Cancelled".into();
        }
        report
    })
}
