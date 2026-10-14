//! Running a plan.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::InstallError;
use crate::plan::{Action, Plan};
use crate::root::Root;
use crate::settings::Settings;
use crate::shell::{join, quote};
use crate::snippets;
use crate::state::HS_USER;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Applied {
    /// Files on the desktop that now hold a token's secret.
    pub token_files: Vec<PathBuf>,
    pub steps_done: usize,
}

/// Write the secret of a token to `file` (mode 0600), never over a file.
fn write_secret(file: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = file.parent()
        && !dir.exists()
    {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let mut handle = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(file)?;
    handle.write_all(line.as_bytes())?;
    handle.flush()
}

fn create_token(root: &mut dyn Root, token: &str, comment: &str, file: &Path) -> Result<(), InstallError> {
    let command = format!(
        "pveum user token add {HS_USER} {} --privsep 1 --comment {} --output-format json",
        quote(token),
        quote(comment)
    );
    let output = root.run(&command)?;
    // The secret has been shown on the access: clear it whatever follows.
    root.forget_screen();
    if output.code != 0 {
        return Err(InstallError::failed(format!(
            "failed: create token {HS_USER}!{token}: {}",
            output.detail()
        )));
    }
    let value = serde_json::from_str::<Value>(output.stdout.trim())
        .ok()
        .and_then(|answer| answer.get("value").and_then(Value::as_str).map(str::to_owned))
        .filter(|value| !value.is_empty());
    let Some(value) = value else {
        return Err(InstallError::failed(format!(
            "pveum returned no secret for {HS_USER}!{token}; the token is left as it is (delete it on the node and run this again)"
        )));
    };
    if let Err(error) = write_secret(file, &format!("{HS_USER}!{token}={value}\n")) {
        // The secret cannot be read again: a token nobody has the secret of
        // is of no use, so remove it and let the run be repeated.
        let removed = root.run(&format!("pveum user token delete {HS_USER} {}", quote(token)));
        let fate = if removed.is_ok_and(|output| output.code == 0) {
            "the token was deleted again, so this can be run again"
        } else {
            "the token could not be deleted again: delete it on the node before running this again"
        };
        return Err(InstallError::failed(format!(
            "cannot write {}: {error}; {fate}",
            file.display()
        )));
    }
    Ok(())
}

/// Run every step of the plan; the first that fails stops the run, and a
/// second run finishes what is left. Token secrets go to the desktop.
pub fn apply(
    root: &mut dyn Root,
    plan: &Plan,
    _settings: &Settings,
    progress: &mut dyn FnMut(&str),
) -> Result<Applied, InstallError> {
    let local = plan
        .topology
        .local_node()
        .map(|node| node.name.clone())
        .ok_or_else(|| InstallError::failed("the plan does not know which node the access is on"))?;
    let mut done = Applied::default();
    for step in &plan.steps {
        progress(&format!("- {}", step.description));
        let failed = |detail: String| {
            InstallError::failed(format!(
                "failed: {}: {detail}; run this again to finish",
                step.description
            ))
        };
        match &step.action {
            Action::Pveum(args) => {
                let output = root.run(&format!("pveum {}", join(args)))?;
                if output.code != 0 {
                    return Err(failed(output.detail()));
                }
            }
            Action::Pvesh(args) => {
                let output = root.run(&format!("pvesh {}", join(args)))?;
                if output.code != 0 {
                    return Err(failed(output.detail()));
                }
            }
            Action::CreateToken {
                token,
                comment,
                file,
                replaced,
            } => {
                if let Some(old) = replaced {
                    std::fs::rename(file, old).map_err(|error| {
                        failed(format!("cannot keep {} as {}: {error}", file.display(), old.display()))
                    })?;
                }
                if let Err(error) = create_token(root, token, comment, file) {
                    // Put the old file back where it was.
                    if let Some(old) = replaced
                        && !file.exists()
                    {
                        let _ = std::fs::rename(old, file);
                    }
                    return Err(error);
                }
                done.token_files.push(file.clone());
            }
            Action::WriteSnippet { node, name, path } => {
                let target = plan
                    .topology
                    .nodes
                    .iter()
                    .find(|candidate| candidate.name == *node)
                    .ok_or_else(|| failed(format!("no node {node}")))?;
                snippets::write(root, &local, target, name, path).map_err(|error| failed(error.to_string()))?;
            }
            Action::RemoveSnippet { node, path, .. } => {
                let target = plan
                    .topology
                    .nodes
                    .iter()
                    .find(|candidate| candidate.name == *node)
                    .ok_or_else(|| failed(format!("no node {node}")))?;
                snippets::remove(root, &local, target, path).map_err(|error| failed(error.to_string()))?;
            }
        }
        done.steps_done += 1;
    }
    progress("Done.");
    Ok(done)
}
