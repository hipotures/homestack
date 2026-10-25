//! The connection a setup works through (Python `WorkspaceSSH.run`,
//! `transfer`): one master SSH connection, every command and copy a child
//! of it that cannot authenticate on its own.

use std::path::Path;
use std::sync::Arc;

use hs_install::shell;
use serde_json::Value;

use crate::AppError;
use crate::master::Master;
use crate::system::{Output, System};

/// What the setup runs on: the workspace, or (`setup --local`) the desktop.
pub trait Workspace {
    /// Run `command` (one shell line) with `input` on its stdin, or with the
    /// terminal when `interactive`. ssh's own failure (255) is an error.
    fn run(&mut self, command: &str, input: Option<&str>, interactive: bool) -> Result<Output, AppError>;
    /// Copy `source` (a desktop path; a trailing slash copies a
    /// directory's contents) to `destination` on the workspace.
    fn transfer(&mut self, source: &str, destination: &str) -> Result<(), AppError>;
    /// The desktop itself (`setup --local`).
    fn local(&self) -> bool {
        false
    }
    /// A guest operation run on the desktop: the program's output.
    fn local_guest(&mut self, _payload: &Value) -> Result<String, AppError> {
        Err(AppError::new("Local Setup is not available on this connection"))
    }
    /// A shell command run on the desktop (`setup --local`).
    fn local_command(&mut self, _command: &str) -> Result<Output, AppError> {
        Err(AppError::new("Local Setup is not available on this connection"))
    }
}

/// The workspace over SSH.
pub struct SshWorkspace {
    system: Arc<dyn System>,
    master: Master,
}

impl SshWorkspace {
    /// Open `master` (the touch of a security key happens here).
    pub fn open(system: Arc<dyn System>, home: &Path, mut master: Master) -> Result<Self, AppError> {
        master.open(system.as_ref(), home)?;
        Ok(Self { system, master })
    }

    fn ssh(&self, mode: &str, command: &str) -> Vec<String> {
        let mut argv = vec!["ssh".to_owned()];
        argv.extend(self.master.child_options());
        argv.extend([mode.to_owned(), self.master.alias.clone(), command.to_owned()]);
        argv
    }
}

impl Workspace for SshWorkspace {
    fn run(&mut self, command: &str, input: Option<&str>, interactive: bool) -> Result<Output, AppError> {
        if interactive && input.is_some() {
            return Err(AppError::new(
                "Interactive workspace commands cannot receive scripted input",
            ));
        }
        self.master.require_master(self.system.as_ref())?;
        let transport = |_| AppError::new("Workspace SSH transport failed; no reauthentication attempted");
        let output = if interactive {
            let code = self
                .system
                .run_interactive(&self.ssh("-tt", command))
                .map_err(transport)?;
            Output {
                code,
                ..Output::default()
            }
        } else {
            let argv = self.ssh("-T", command);
            match input {
                Some(input) => self.system.run_input(&argv, input),
                None => self.system.run(&argv),
            }
            .map_err(transport)?
        };
        if output.code == 255 {
            return Err(AppError::new(
                "Workspace SSH transport failed; no reauthentication attempted",
            ));
        }
        Ok(output)
    }

    fn transfer(&mut self, source: &str, destination: &str) -> Result<(), AppError> {
        self.master.require_master(self.system.as_ref())?;
        let mut ssh = vec!["ssh".to_owned()];
        ssh.extend(self.master.child_options());
        let argv: Vec<String> = [
            "rsync",
            "-a",
            "--no-owner",
            "--no-group",
            "--protect-args",
            "--no-links",
            "--chmod=Du=rwx,Dgo=,Fu=rwX,Fgo=",
            "-e",
            &shell::join(&ssh),
            "--",
            source,
            &format!("{}:{destination}", self.master.alias),
        ]
        .map(str::to_owned)
        .to_vec();
        let code = self.system.run(&argv).map_or(-1, |output| output.code);
        if code != 0 {
            return Err(AppError::new(format!(
                "File transfer failed (exit {code}); output withheld"
            )));
        }
        Ok(())
    }
}

impl Drop for SshWorkspace {
    fn drop(&mut self) {
        self.master.close(self.system.as_ref());
    }
}

/// The desktop's own account and home (`setup --local`, Python
/// `LocalSetup`): commands run here, the guest program with the desktop's
/// `python3`, no SSH.
pub struct LocalWorkspace {
    pub home: std::path::PathBuf,
    pub user: String,
}

/// The name and home of the account `uid` (`getent passwd`).
#[must_use]
pub fn account(uid: u32) -> Option<(String, String)> {
    let output = std::process::Command::new("getent")
        .args(["passwd", &uid.to_string()])
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&output.stdout).lines().next()?.to_owned();
    let fields: Vec<&str> = line.split(':').collect();
    (fields.len() >= 6).then(|| (fields[0].to_owned(), fields[5].to_owned()))
}

impl LocalWorkspace {
    fn identity(&mut self) -> Result<String, AppError> {
        use std::os::unix::fs::MetadataExt;
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let refused = || AppError::new("Local Setup requires the current non-root user's owned home directory");
        let meta = std::fs::symlink_metadata(&self.home).map_err(|_| refused())?;
        let home_dir = account(uid).map(|(_, dir)| dir).unwrap_or_default();
        if uid == 0 || !meta.is_dir() || meta.uid() != uid || meta.gid() != gid || Path::new(&home_dir) != self.home {
            return Err(refused());
        }
        let registry = self.local_guest(&serde_json::json!({
            "home": self.home.display().to_string(), "operation": "state-read",
        }))?;
        let answer: Value = serde_json::from_str(&registry).unwrap_or(Value::Null);
        if answer["ok"] != Value::Bool(true) {
            return Ok(registry);
        }
        Ok(r#"{"ok": true}"#.to_owned())
    }
}

impl Workspace for LocalWorkspace {
    fn run(&mut self, _command: &str, _input: Option<&str>, _interactive: bool) -> Result<Output, AppError> {
        Err(AppError::new("Local Setup currently supports only the Backup category"))
    }

    fn transfer(&mut self, _source: &str, _destination: &str) -> Result<(), AppError> {
        Err(AppError::new("Local Setup currently supports only the Backup category"))
    }

    fn local(&self) -> bool {
        true
    }

    fn local_guest(&mut self, payload: &Value) -> Result<String, AppError> {
        let mut payload = payload.clone();
        payload["home"] = Value::String(self.home.display().to_string());
        if payload["operation"] == "identity" {
            return self.identity();
        }
        let mut child = std::process::Command::new("python3")
            .arg("-")
            .current_dir(&self.home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|_| AppError::new("Missing local executable python3; install system prerequisites separately"))?;
        let script = super::guest::script(&payload);
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(script.as_bytes());
        }
        let output = child
            .wait_with_output()
            .map_err(|_| AppError::new("Local transport or file operation failed; output withheld"))?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn local_command(&mut self, command: &str) -> Result<Output, AppError> {
        // The system runtime, not whatever environment runs HomeStack.
        let mut process = std::process::Command::new("/bin/sh");
        process
            .args(["-c", &format!("umask 077\n{command}")])
            .current_dir(&self.home)
            .env_clear()
            .env("HOME", &self.home)
            .env("USER", &self.user)
            .env("LOGNAME", &self.user)
            .env("LANG", "C.UTF-8")
            .env("PATH", "/usr/local/bin:/usr/bin:/bin");
        for key in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
            if let Some(value) = std::env::var_os(key) {
                process.env(key, value);
            }
        }
        let output = process
            .output()
            .map_err(|_| AppError::new("Local transport or file operation failed; output withheld"))?;
        Ok(Output {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
