//! Root on a Proxmox node through a Herdr pane, for `homestack install` and
//! `uninstall` only (the owner's decision of 2026-10-13: Herdr is one of the
//! two ways to reach root once; nothing else in the product uses it).
//!
//! The user opens a root shell on the node in a Herdr tab or pane and names
//! it. The name is searched among the tabs and panes of every workspace
//! (`WORKSPACE/NAME` narrows it); none or several is an error that says so.
//! Commands are typed into the pane wrapped so that their output and exit
//! code come back whole: a begin marker, the standard output and the
//! standard error in base64, and an end marker carrying the exit code, all
//! with a random token. The shell assembles the markers, so the echo of the
//! typed line never contains one.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hs_install::{InstallError, Output, Root};
use serde_json::Value;

use crate::AppError;
use crate::system::System;

/// How long a command may run before the client gives up on the pane.
const TIMEOUT_MS: u64 = 180_000;
/// A typed line longer than this is refused before it is sent.
const MAX_LINE: usize = 3800;
/// How much of the pane is read back after a command.
const READ_LINES: &str = "60000";

/// One tab or pane the name can mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub workspace: String,
    pub tab: String,
    pub pane: String,
    /// Panes of the tab the name matched; 1 when it matched a pane.
    pub panes_in_tab: usize,
    /// "tab" or "pane".
    pub kind: &'static str,
}

impl Candidate {
    fn shown(&self) -> String {
        format!("{} {}/{} (pane {})", self.kind, self.workspace, self.tab, self.pane)
    }
}

pub struct HerdrRoot {
    system: Arc<dyn System>,
    pane: String,
    label: String,
    token_counter: u64,
}

fn herdr(system: &dyn System, args: &[&str]) -> Result<crate::system::Output, AppError> {
    let mut argv = vec!["herdr".to_owned()];
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    system
        .run(&argv)
        .map_err(|error| AppError::new(format!("Could not run herdr: {error}")))
}

fn herdr_json(system: &dyn System, args: &[&str]) -> Result<Value, AppError> {
    let output = herdr(system, args)?;
    if output.code != 0 {
        let detail = [&output.stderr, &output.stdout]
            .iter()
            .find_map(|text| text.lines().map(str::trim).rfind(|line| !line.is_empty()))
            .unwrap_or_default();
        return Err(AppError::new(format!("herdr {} failed: {detail}", args.join(" "))));
    }
    serde_json::from_str(output.stdout.trim())
        .map_err(|error| AppError::new(format!("herdr {} did not answer JSON: {error}", args.join(" "))))
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// Every tab and pane named `name` (`WORKSPACE/NAME` narrows to a workspace).
pub fn candidates(system: &dyn System, name: &str) -> Result<Vec<Candidate>, AppError> {
    let (only_workspace, wanted) = match name.split_once('/') {
        Some((workspace, rest)) if !workspace.is_empty() && !rest.is_empty() => (Some(workspace), rest),
        _ => (None, name),
    };
    let workspaces = herdr_json(system, &["workspace", "list"])?;
    let tabs = herdr_json(system, &["tab", "list"])?;
    let panes = herdr_json(system, &["pane", "list"])?;
    let empty = Vec::new();
    let workspace_label = |id: &str| -> String {
        workspaces["result"]["workspaces"]
            .as_array()
            .unwrap_or(&empty)
            .iter()
            .find(|item| text(item, "workspace_id") == id)
            .map(|item| text(item, "label"))
            .unwrap_or_default()
    };
    let all_tabs = tabs["result"]["tabs"].as_array().unwrap_or(&empty);
    let all_panes = panes["result"]["panes"].as_array().unwrap_or(&empty);
    let mut found = Vec::new();
    for tab in all_tabs {
        let workspace = workspace_label(&text(tab, "workspace_id"));
        if only_workspace.is_some_and(|only| only != workspace) {
            continue;
        }
        let tab_id = text(tab, "tab_id");
        let in_tab: Vec<&Value> = all_panes.iter().filter(|pane| text(pane, "tab_id") == tab_id).collect();
        if text(tab, "label") == wanted {
            found.push(Candidate {
                workspace: workspace.clone(),
                tab: text(tab, "label"),
                pane: in_tab.first().map(|pane| text(pane, "pane_id")).unwrap_or_default(),
                panes_in_tab: in_tab.len(),
                kind: "tab",
            });
        }
        for pane in in_tab {
            if text(pane, "label") == wanted {
                found.push(Candidate {
                    workspace: workspace.clone(),
                    tab: text(tab, "label"),
                    pane: text(pane, "pane_id"),
                    panes_in_tab: 1,
                    kind: "pane",
                });
            }
        }
    }
    Ok(found)
}

/// The one pane `name` means, or an error that says why not.
pub fn find(system: &dyn System, name: &str) -> Result<Candidate, AppError> {
    let mut found = candidates(system, name)?;
    match found.len() {
        0 => Err(AppError::new(format!(
            "Herdr has no tab or pane named {name:?}. Open a root shell on the Proxmox node in a Herdr tab \
(or pane) with that name."
        ))),
        1 => {
            let one = found.remove(0);
            if one.panes_in_tab != 1 {
                return Err(AppError::new(format!(
                    "The Herdr {} has {} panes: put the root shell alone in a tab, or name the pane",
                    one.shown(),
                    one.panes_in_tab
                )));
            }
            Ok(one)
        }
        count => {
            let list: Vec<String> = found.iter().map(Candidate::shown).collect();
            Err(AppError::new(format!(
                "More than one Herdr tab or pane is named {name:?} ({count}): {}. \
Name it as WORKSPACE/NAME, or rename all but one.",
                list.join("; ")
            )))
        }
    }
}

/// `text` without terminal escape sequences and carriage returns.
#[must_use]
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
            }
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

impl HerdrRoot {
    /// Find the pane named `name` and check it is at a root shell prompt.
    pub fn open(system: Arc<dyn System>, name: &str) -> Result<Self, AppError> {
        if system.which("herdr").is_none() {
            return Err(AppError::new("Required local command 'herdr' was not found"));
        }
        let target = find(system.as_ref(), name)?;
        let label = format!("Herdr {}", target.shown());
        let root = Self {
            system,
            pane: target.pane,
            label,
            token_counter: 0,
        };
        root.require_root_prompt(name)?;
        Ok(root)
    }

    #[must_use]
    pub fn pane(&self) -> &str {
        &self.pane
    }

    /// The last line of the pane, which a root shell ends with `#`.
    fn require_root_prompt(&self, name: &str) -> Result<(), AppError> {
        let output = herdr(
            self.system.as_ref(),
            &["pane", "read", &self.pane, "--source", "detection"],
        )?;
        if output.code != 0 {
            return Err(AppError::new(format!("Could not read the Herdr pane {}", self.pane)));
        }
        let screen = strip_ansi(&output.stdout);
        let last = screen
            .lines()
            .map(str::trim_end)
            .rfind(|line| !line.trim().is_empty())
            .unwrap_or_default();
        if !last.ends_with('#') {
            return Err(AppError::new(format!(
                "The Herdr pane {name:?} is not at a root shell prompt (its last line is {last:?}); \
log in as root on the Proxmox node there, and finish whatever it is running"
            )));
        }
        Ok(())
    }

    fn wrapped(&mut self, command: &str) -> (String, String) {
        self.token_counter += 1;
        let token = format!(
            "{:x}{:x}{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos()),
            self.token_counter
        );
        let line = format!(
            "hs_t={token}; hs_e=$(mktemp); hs_o=$({{ /bin/sh -c {} 2>\"$hs_e\"; printf '\\nrc=%d' \"$?\"; }}); \
printf '\\n__HS_BEGIN__%s\\n' \"$hs_t\"; printf '%s' \"$hs_o\" | base64 -w0; \
printf '\\n__HS_ERR__%s\\n' \"$hs_t\"; base64 -w0 < \"$hs_e\"; \
printf '\\n__HS_END__%s:\\n' \"$hs_t\"; rm -f \"$hs_e\"; unset hs_t hs_e hs_o",
            hs_install::shell::quote(command)
        );
        (token, line)
    }
}

fn fail(message: impl Into<String>) -> InstallError {
    InstallError::failed(message)
}

/// The output of one wrapped command out of the pane's text.
fn parse_envelope(screen: &str, token: &str) -> Option<Output> {
    let screen = strip_ansi(screen);
    let begin = format!("__HS_BEGIN__{token}");
    let err = format!("__HS_ERR__{token}");
    let end = format!("__HS_END__{token}:");
    let after_begin = &screen[screen.rfind(&begin)? + begin.len()..];
    let (out_b64, after_out) = after_begin.split_once(&err)?;
    let (err_b64, _) = after_out.split_once(&end)?;
    let decode = |encoded: &str| {
        let squeezed: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
        STANDARD
            .decode(squeezed)
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    };
    // The shell appended "\nrc=N" to the standard output, so that trailing
    // newlines of the output survive the command substitution.
    let with_code = decode(out_b64)?;
    let (stdout, code) = with_code.rsplit_once("\nrc=")?;
    Some(Output {
        code: code.trim().parse().ok()?,
        stdout: stdout.to_owned(),
        stderr: decode(err_b64)?,
    })
}

impl Root for HerdrRoot {
    fn run(&mut self, command: &str) -> Result<Output, InstallError> {
        if command.contains('\n') {
            return Err(fail("a command for a pane must be one line"));
        }
        let (token, line) = self.wrapped(command);
        if line.len() > MAX_LINE {
            return Err(fail(format!(
                "a command of {} characters is too long to type into a pane",
                line.len()
            )));
        }
        let ran = herdr(self.system.as_ref(), &["pane", "run", &self.pane, &line]).map_err(|e| fail(e.0))?;
        if ran.code != 0 {
            return Err(fail(format!("herdr pane run failed: {}", ran.stderr.trim())));
        }
        let end = format!("__HS_END__{token}:");
        let timeout = TIMEOUT_MS.to_string();
        let waited = herdr(
            self.system.as_ref(),
            &[
                "pane",
                "wait-output",
                &self.pane,
                "--match",
                &end,
                "--source",
                "recent-unwrapped",
                "--timeout",
                &timeout,
            ],
        )
        .map_err(|e| fail(e.0))?;
        if waited.code != 0 {
            return Err(fail(format!(
                "the command did not finish in {} seconds in {}: {}",
                TIMEOUT_MS / 1000,
                self.label,
                command
            )));
        }
        let read = herdr(
            self.system.as_ref(),
            &[
                "pane",
                "read",
                &self.pane,
                "--source",
                "recent-unwrapped",
                "--lines",
                READ_LINES,
            ],
        )
        .map_err(|e| fail(e.0))?;
        parse_envelope(&read.stdout, &token).ok_or_else(|| {
            fail(format!(
                "could not read the answer of the command back from {}: {command}",
                self.label
            ))
        })
    }

    fn forget_screen(&mut self) {
        let _ = herdr(
            self.system.as_ref(),
            &["pane", "run", &self.pane, "printf '\\033[3J\\033[2J\\033[H'"],
        );
    }

    fn describe(&self) -> String {
        self.label.clone()
    }
}
