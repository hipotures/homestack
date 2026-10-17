//! The ssh button (phase 13b, step 4): the TUI gives the terminal to `ssh`
//! and takes it back when ssh ends.

use super::state::App;

#[must_use]
pub fn is_gold(app: &App, vmid: u32) -> bool {
    app.status
        .as_ref()
        .and_then(|status| status.gold.as_ref())
        .is_some_and(|gold| gold.vmid == vmid)
}

#[must_use]
pub fn available(app: &App, vmid: u32) -> bool {
    app.selected_entry().is_some_and(|entry| {
        entry.vmid == vmid
            && (entry.status == "running"
                || (entry.status == "stopped" && is_gold(app, vmid) && app.lab_gold_vmid == Some(vmid)))
    })
}

/// Use the VM name as its SSH alias, preserving the desktop's Host/Match rules.
pub fn prepare(app: &mut App, vmid: u32) -> Option<String> {
    if app.selected != Some(vmid) {
        return None;
    }
    let gold = is_gold(app, vmid);
    if gold && app.lab_gold_vmid == Some(vmid) {
        return Some("root@gold-e2e".to_owned());
    }
    let name = app
        .selected_entry()
        .and_then(|entry| entry.name.as_deref())
        .filter(|name| crate::app::valid_name(name));
    if let Some(name) = name {
        return Some(if gold { format!("root@{name}") } else { name.to_owned() });
    }
    let time = app.clock.clone();
    app.log(
        &time,
        &format!("ssh {vmid}: the VM has no valid name to use as an SSH alias"),
    );
    None
}

/// Workspace access uses keys; an unavailable authenticator must not prompt for a password.
#[must_use]
pub fn command(target: &str) -> std::process::Command {
    let mut command = std::process::Command::new("ssh");
    command.args(["-o", "PreferredAuthentications=publickey", target]);
    command
}

/// The line written above ssh's own output, on the normal screen.
#[must_use]
pub fn banner(target: &str, name: &str) -> String {
    format!("homestack: ssh {target} ({name}); log out to go back to the TUI")
}

/// The log line of an ended ssh: `None` is ssh that did not start.
#[must_use]
pub fn outcome(target: &str, code: Option<i32>) -> String {
    match code {
        Some(0) => format!("ssh {target}: ended"),
        Some(255) => format!("ssh {target}: could not connect (255)"),
        Some(code) => format!("ssh {target}: ended with {code}"),
        None => format!("ssh {target}: could not run ssh"),
    }
}
