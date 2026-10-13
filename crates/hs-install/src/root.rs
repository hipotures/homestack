//! A root shell on a Proxmox node.

use std::process::Command;

use crate::InstallError;

/// A finished command on the node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// The last non-empty line of stderr, or of stdout, for a message.
    #[must_use]
    pub fn detail(&self) -> String {
        [&self.stderr, &self.stdout]
            .iter()
            .filter_map(|text| text.lines().map(str::trim).rfind(|line| !line.is_empty()))
            .next()
            .unwrap_or_default()
            .to_owned()
    }
}

/// Root on one node, reached once. A command that runs and fails is an
/// [`Output`] with a code; an access that does not work is an `Err`.
pub trait Root {
    /// Run `command` (one shell command line) as root on the node.
    fn run(&mut self, command: &str) -> Result<Output, InstallError>;

    /// A secret has just been shown on the access: remove its traces (the
    /// screen and scrollback of a pane). Nothing to do over SSH.
    fn forget_screen(&mut self) {}

    /// Where this access leads, for messages.
    fn describe(&self) -> String;
}

/// A shell on this machine, with an environment of its own: the fake
/// Proxmox tools of the tests. Not offered by the command line.
pub struct LocalShell {
    env: Vec<(String, String)>,
}

impl LocalShell {
    #[must_use]
    pub fn new(env: Vec<(String, String)>) -> Self {
        Self { env }
    }
}

impl Root for LocalShell {
    fn run(&mut self, command: &str) -> Result<Output, InstallError> {
        let output = Command::new("bash")
            .arg("-c")
            .arg(command)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|error| InstallError::failed(format!("cannot run bash: {error}")))?;
        Ok(Output {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn describe(&self) -> String {
        "this machine".to_owned()
    }
}
