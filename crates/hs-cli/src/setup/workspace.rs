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
