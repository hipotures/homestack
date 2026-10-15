//! Root on a Proxmox node over SSH, for `homestack install` and `uninstall`
//! only. One master connection is opened, so a hardware key is touched once
//! (with the sound cues of phase 13) and every command of the run goes
//! through it; the user's own `~/.ssh/config` applies.

use std::path::Path;
use std::sync::Arc;

use hs_install::{InstallError, Output, Root};

use crate::AppError;
use crate::master::Master;
use crate::system::System;
use crate::touch::Sounds;

pub struct SshRoot {
    system: Arc<dyn System>,
    master: Master,
    target: String,
}

/// `USER@HOST` as the user wrote it; a bare `HOST` is `root@HOST`.
pub fn target(text: &str) -> Result<String, AppError> {
    let text = text.trim();
    let (user, host) = text.split_once('@').unwrap_or(("root", text));
    let plain = |word: &str| {
        !word.is_empty()
            && !word.starts_with('-')
            && word.chars().all(|c| c.is_ascii_alphanumeric() || "._-:[]".contains(c))
    };
    if !plain(user) || !plain(host) {
        return Err(AppError::new(format!(
            "Invalid SSH target {text:?}: write it as USER@HOST (usually root@pve1)"
        )));
    }
    Ok(format!("{user}@{host}"))
}

impl SshRoot {
    /// Open the master to `text`; with `identity` ssh offers only that key.
    pub fn open(
        system: Arc<dyn System>,
        home: &Path,
        text: &str,
        identity: Option<&str>,
        sounds: &Sounds,
    ) -> Result<Self, AppError> {
        Self::open_with(system, home, text, identity, sounds, Vec::new(), "accept-new")
    }

    fn open_with(
        system: Arc<dyn System>,
        home: &Path,
        text: &str,
        identity: Option<&str>,
        sounds: &Sounds,
        prefix: Vec<String>,
        host_keys: &str,
    ) -> Result<Self, AppError> {
        let target = target(text)?;
        let mut master = Master::new(&target);
        master.prefix = prefix;
        master.host_keys = host_keys.to_owned();
        master.name = target
            .rsplit_once('@')
            .map_or(target.clone(), |(_, host)| host.to_owned());
        master.sounds = sounds.clone();
        if let Some(identity) = identity {
            master.options.extend([
                "-i".to_owned(),
                crate::expand_home(identity, home).display().to_string(),
                "-o".to_owned(),
                "IdentitiesOnly=yes".to_owned(),
            ]);
        }
        master.open(system.as_ref(), home)?;
        Ok(Self { system, master, target })
    }

    /// [`SshRoot::open`] for the TUI: ssh runs behind `prefix` (the askpass
    /// bridge, `askpass::Bridge::command_prefix`), so its questions, a new
    /// host key among them, are asked in the TUI.
    pub fn open_asking(
        system: Arc<dyn System>,
        home: &Path,
        text: &str,
        identity: Option<&str>,
        sounds: &Sounds,
        prefix: Vec<String>,
    ) -> Result<Self, AppError> {
        Self::open_with(system, home, text, identity, sounds, prefix, "ask")
    }
}

impl Root for SshRoot {
    fn run(&mut self, command: &str) -> Result<Output, InstallError> {
        let mut argv = vec!["ssh".to_owned()];
        argv.extend(self.master.child_options());
        argv.extend([self.target.clone(), command.to_owned()]);
        let output = self
            .system
            .run(&argv)
            .map_err(|error| InstallError::failed(format!("Could not run ssh: {error}")))?;
        // 255 is ssh's own failure; a remote command that exits 255 is not
        // told apart, and no command of the installer does.
        if output.code == 255 {
            let detail = output
                .stderr
                .lines()
                .map(str::trim)
                .rfind(|line| !line.is_empty())
                .unwrap_or("no reason given");
            return Err(InstallError::failed(format!("ssh to {} failed: {detail}", self.target)));
        }
        Ok(Output {
            code: output.code,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn describe(&self) -> String {
        format!("ssh {}", self.target)
    }
}

impl Drop for SshRoot {
    fn drop(&mut self) {
        self.master.close(self.system.as_ref());
    }
}
