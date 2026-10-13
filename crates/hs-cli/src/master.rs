//! The workspace SSH master (Python `WorkspaceSSH.__enter__`, I2): one
//! owned, non-expiring master connection; child connections cannot
//! authenticate on their own. With `fido_devices` set, the master runs in a
//! sandbox that shows only the preferred kind of security key.
//!
//! The commands that use it (`setup`, `repo`) arrive in phase 14; this
//! module carries the opening, its checks and its messages.

use std::path::Path;

use hs_config::client::WorkspaceSsh;

use crate::system::System;
use crate::touch::{self, Player, Sounds};
use crate::{AppError, expand_home, fido};

/// One master to `alias`.
pub struct Master {
    /// What ssh connects to (`user@address`).
    pub alias: String,
    /// What the user calls the host, for messages.
    pub name: String,
    /// The host's address, for messages only.
    pub address: Option<String>,
    /// ssh options (`-o`, `-i`, `-F`).
    pub options: Vec<String>,
    pub fido_devices: Vec<String>,
    pub sounds: Sounds,
    pub player: Player,
    /// The kind of key the sandbox shows, once chosen.
    pub fido_kind: Option<String>,
    directory: Option<tempfile::TempDir>,
    control: String,
    opened: bool,
}

impl Master {
    /// A silent master to `target` with no options.
    #[must_use]
    pub fn new(target: &str) -> Self {
        Self {
            alias: target.to_owned(),
            name: target.to_owned(),
            address: None,
            options: Vec::new(),
            fido_devices: Vec::new(),
            sounds: Sounds::disabled(),
            player: touch::default_player(),
            fido_kind: None,
            directory: None,
            control: String::new(),
            opened: false,
        }
    }

    /// The master to `user@ip` with the settings of `[workspace_ssh]` and
    /// `[sounds]` (Python `WorkspaceSSH.configured`).
    #[must_use]
    pub fn configured(
        home: &Path,
        ssh: &WorkspaceSsh,
        sounds: &Sounds,
        user: &str,
        ip: &str,
        name: Option<&str>,
    ) -> Self {
        let mut options = vec![
            "-o".to_owned(),
            format!("IdentitiesOnly={}", if ssh.identities_only { "yes" } else { "no" }),
            "-o".to_owned(),
            format!("LogLevel={}", ssh.log_level),
        ];
        for identity in &ssh.identity_files {
            options.push("-i".to_owned());
            options.push(expand_home(identity, home).display().to_string());
        }
        let mut master = Self::new(&format!("{user}@{ip}"));
        master.name = name.filter(|name| !name.is_empty()).unwrap_or(ip).to_owned();
        master.address = Some(ip.to_owned());
        master.options = options;
        master.fido_devices.clone_from(&ssh.fido_devices);
        master.sounds = sounds.clone();
        master
    }

    /// The options of a child connection: only through the master.
    #[must_use]
    pub fn child_options(&self) -> Vec<String> {
        let mut options = self.options.clone();
        options.extend(
            [
                "-S",
                &self.control,
                "-o",
                "ControlMaster=no",
                "-o",
                "BatchMode=yes",
                "-o",
                "ProxyCommand=false",
            ]
            .map(str::to_owned),
        );
        for auth in ["Pubkey", "Password", "KbdInteractive", "GSSAPI", "Hostbased"] {
            options.push("-o".to_owned());
            options.push(format!("{auth}Authentication=no"));
        }
        options
    }

    /// `~/.ssh/config`, or `/dev/null` without one: inside the sandbox
    /// root-owned files look like they belong to "nobody", which ssh
    /// rejects for the system-wide config.
    #[must_use]
    pub fn user_ssh_config(home: &Path) -> String {
        let path = home.join(".ssh").join("config");
        if path.is_file() {
            path.display().to_string()
        } else {
            "/dev/null".to_owned()
        }
    }

    /// Choose the key kind and check the host can use it; the sandbox
    /// prefix, empty without `fido_devices`.
    fn sandbox_prefix(&mut self, system: &dyn System) -> Result<Vec<String>, AppError> {
        if self.fido_devices.is_empty() {
            return Ok(Vec::new());
        }
        let (entry, devices) = fido::select(&self.fido_devices, &fido::list_devices(system)?)?;
        self.fido_kind = Some(entry.clone());
        // A key the host does not know would make the device ask for every
        // handle ssh tries.
        let mut listing = vec!["ssh".to_owned()];
        listing.extend(self.options.iter().cloned());
        listing.extend(["-G".to_owned(), self.alias.clone()]);
        let stdout = system.run(&listing).map(|output| output.stdout).unwrap_or_default();
        let identity_files: Vec<String> = stdout
            .lines()
            .filter(|line| line.to_lowercase().starts_with("identityfile "))
            .filter_map(|line| {
                line.split_once(char::is_whitespace)
                    .map(|(_, path)| path.trim().to_owned())
            })
            .collect();
        if let Some(problem) = fido::missing_key_message(&entry, &identity_files, &self.name, self.address.as_deref()) {
            return Err(AppError::new(problem));
        }
        fido::sandbox_prefix(system, &devices)
    }

    /// Open the master. Nothing is left behind on failure.
    pub fn open(&mut self, system: &dyn System, home: &Path) -> Result<(), AppError> {
        if system.which("ssh").is_none() {
            return Err(AppError::new("Required local command 'ssh' was not found"));
        }
        let directory = tempfile::Builder::new()
            .prefix("hs-ssh-")
            .tempdir()
            .map_err(|error| AppError::new(format!("Could not create the SSH control directory: {error}")))?;
        self.control = directory.path().join("master").display().to_string();
        self.directory = Some(directory);
        let result = self.start(system, home);
        if result.is_err() {
            self.close(system);
        }
        result
    }

    fn start(&mut self, system: &dyn System, home: &Path) -> Result<(), AppError> {
        let prefix = self.sandbox_prefix(system)?;
        let config_args: Vec<String> = if prefix.is_empty() || self.options.iter().any(|o| o == "-F") {
            Vec::new()
        } else {
            vec!["-F".to_owned(), Self::user_ssh_config(home)]
        };
        // An ssh-agent signs outside the sandbox and would ask every attached
        // key, so ssh signs by itself unless an agent was chosen explicitly.
        let agent_args: Vec<String> = if !prefix.is_empty()
            && !self
                .options
                .iter()
                .any(|option| option.to_lowercase().starts_with("identityagent"))
        {
            vec!["-o".to_owned(), "IdentityAgent=none".to_owned()]
        } else {
            Vec::new()
        };
        let master_options = match &self.fido_kind {
            Some(kind) => fido::filter_identity_options(kind, &self.options),
            None => self.options.clone(),
        };
        let mut argv = prefix;
        argv.push("ssh".to_owned());
        argv.extend(config_args);
        argv.extend(master_options);
        argv.extend(agent_args);
        argv.extend(
            [
                "-M",
                "-S",
                &self.control,
                "-o",
                "ControlPersist=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "PasswordAuthentication=no",
                "-o",
                "KbdInteractiveAuthentication=no",
                "-fN",
                &self.alias,
            ]
            .map(str::to_owned),
        );
        let started = system.monotonic();
        let output = touch::touch_wait(&self.name, &self.sounds, self.player.clone(), |watch| {
            let output = system.run_master(&argv).map_err(|error| error.to_string())?;
            if output.code != 0 {
                watch.finish(false);
            }
            Ok::<_, String>(output)
        })
        .map_err(|error| AppError::new(format!("{}: {error}", touch::message(touch::FailureKind::Failed))))?;
        self.opened = output.code == 0;
        if !self.opened {
            return Err(AppError::new(Self::failure(
                &output.stderr,
                system.monotonic() - started,
            )));
        }
        self.require_master(system)
    }

    /// A short message for a failed master; raw ssh output is not shown.
    fn failure(stderr: &str, elapsed: f64) -> String {
        let kind = touch::classify_ssh_failure(255, elapsed, stderr);
        let mut message = touch::message(kind).to_owned();
        if kind == touch::FailureKind::Failed
            && let Some(last) = stderr.lines().map(str::trim).rfind(|line| !line.is_empty())
        {
            message.push(' ');
            message.extend(last.chars().take(200));
        }
        message
    }

    /// The master is up, or the command fails without authenticating again.
    pub fn require_master(&self, system: &dyn System) -> Result<(), AppError> {
        let mut check = vec!["ssh".to_owned()];
        check.extend(self.child_options());
        check.extend(["-O".to_owned(), "check".to_owned(), self.alias.clone()]);
        if !self.opened || system.run(&check).map_or(true, |output| output.code != 0) {
            return Err(AppError::new(
                "Shared workspace SSH master is unavailable; no reauthentication attempted",
            ));
        }
        Ok(())
    }

    /// Stop the master and remove its control directory.
    pub fn close(&mut self, system: &dyn System) {
        if self.opened || (!self.control.is_empty() && Path::new(&self.control).exists()) {
            let mut exit = vec!["ssh".to_owned()];
            exit.extend(self.child_options());
            exit.extend(["-O".to_owned(), "exit".to_owned(), self.alias.clone()]);
            let _ = system.run(&exit);
        }
        self.forget();
    }

    /// Drop the control directory without asking ssh to stop anything.
    pub fn forget(&mut self) {
        self.opened = false;
        self.directory = None;
    }
}
