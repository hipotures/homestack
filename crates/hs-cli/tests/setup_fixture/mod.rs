//! What the setup tests share: the settings and target of Python's
//! `support.test_config()`, the guest program run on this desktop, and
//! workspaces that record what the setup does.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use hs_cli::AppError;
use hs_cli::setup::definitions::{Entry, SetupConfig, defaults};
use hs_cli::setup::guest::script;
use hs_cli::setup::workspace::Workspace;
use hs_cli::setup::{RepoSettings, Settings, Target};
use hs_cli::system::Output;
use serde_json::{Value, json};

/// Python's `test_config()` for the setup, with `home` as the desktop.
pub fn settings(home: &Path) -> Settings {
    Settings {
        home: home.to_path_buf(),
        user: "user".to_owned(),
        uid: 1000,
        gid: 1000,
        setup: SetupConfig::default(),
        repo: RepoSettings::default(),
        config_path: home.join("client.toml"),
        state_home: home.join("state"),
    }
}

/// Python's `TARGET`.
pub fn target() -> Target {
    Target {
        vmid: 200,
        name: "workspace".to_owned(),
        node: "example-node-1".to_owned(),
        status: "running".to_owned(),
        alien: false,
        ip: "192.0.2.200".to_owned(),
        user: "user".to_owned(),
        home: "/home/user".to_owned(),
        local: false,
    }
}

/// A built-in item.
pub fn entry(id: &str) -> Entry {
    defaults().into_iter().find(|e| e.id == id).unwrap()
}

/// `python3 -` with `input`; its stdout.
fn python(input: &str, path: Option<&str>) -> String {
    let mut command = Command::new("python3");
    command
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let mut child = command.spawn().expect("python3 runs the guest program");
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Run the guest program here on `payload` (Python `setup_guest.run`
/// through `main`): its answer, `ok` false with `error` on a refusal.
pub fn run_guest(payload: &Value) -> Value {
    serde_json::from_str(&python(&script(payload), None)).unwrap()
}

/// [`run_guest`] with `path` as `PATH` (fake `hostname`, `findmnt`).
pub fn run_guest_with_path(payload: &Value, path: &str) -> Value {
    serde_json::from_str(&python(&script(payload), Some(path))).unwrap()
}

/// The payload of a guest script (its last line, `main("…")`).
pub fn payload_of(script: &str) -> Value {
    let line = script.trim_end().lines().last().unwrap();
    let literal = line.strip_prefix("main(").unwrap().strip_suffix(')').unwrap();
    let text: String = serde_json::from_str(literal).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// What a test workspace answers to a command that is not a guest call.
pub type Answer = Box<dyn FnMut(&str, bool) -> Result<Output, AppError>>;
/// What a test workspace answers to a guest call: `None` runs the real
/// program in the workspace's home.
pub type GuestAnswer = Box<dyn FnMut(&str, &Value) -> Option<Result<Value, AppError>>>;

/// A workspace whose home is a local directory: guest calls run the real
/// program there (identity is accepted), other commands are answered.
pub struct TestWorkspace {
    pub home: PathBuf,
    pub commands: Vec<(String, bool)>,
    pub guests: Vec<Value>,
    pub transfers: Vec<(String, String)>,
    pub answer: Answer,
    pub guest: GuestAnswer,
}

impl TestWorkspace {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            commands: Vec::new(),
            guests: Vec::new(),
            transfers: Vec::new(),
            answer: Box::new(|_, _| Ok(Output::default())),
            guest: Box::new(|_, _| None),
        }
    }

    /// Every guest call answered by `answer` (the real program is not run).
    pub fn scripted(home: &Path, mut answer: impl FnMut(&str, &Value) -> Result<Value, AppError> + 'static) -> Self {
        let mut workspace = Self::new(home);
        workspace.guest = Box::new(move |operation, payload| Some(answer(operation, payload)));
        workspace
    }

    pub fn operations(&self) -> Vec<String> {
        self.guests
            .iter()
            .map(|payload| payload["operation"].as_str().unwrap_or_default().to_owned())
            .collect()
    }
}

impl Workspace for TestWorkspace {
    fn run(&mut self, command: &str, input: Option<&str>, interactive: bool) -> Result<Output, AppError> {
        if let Some(input) = input
            && command.contains("python3 -")
        {
            let mut payload = payload_of(input);
            self.guests.push(payload.clone());
            let operation = payload["operation"].as_str().unwrap_or_default().to_owned();
            let answer = match (self.guest)(&operation, &payload) {
                Some(answer) => answer,
                None if operation == "identity" => Ok(json!({"ok": true})),
                None => {
                    payload["home"] = json!(self.home.display().to_string());
                    Ok(run_guest(&payload))
                }
            };
            let stdout = match answer {
                Ok(value) => value.to_string(),
                Err(error) => json!({"ok": false, "error": error.0}).to_string(),
            };
            return Ok(Output::stdout(&stdout));
        }
        self.commands.push((command.to_owned(), interactive));
        (self.answer)(command, interactive)
    }

    fn transfer(&mut self, source: &str, destination: &str) -> Result<(), AppError> {
        self.transfers.push((source.to_owned(), destination.to_owned()));
        Ok(())
    }
}
