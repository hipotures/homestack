//! The ssh button (phase 13b, step 4): the TUI gives the terminal to `ssh`
//! and takes it back when ssh ends.

use std::path::Path;

use super::state::{App, DetailsAction};
use crate::local_ssh;

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

/// Resolve an explicit SSH request, loading server details only without a local entry.
pub fn prepare(app: &mut App, home: &Path, vmid: u32) -> Option<String> {
    if app.selected != Some(vmid) {
        return None;
    }
    let gold = is_gold(app, vmid);
    if gold && app.lab_gold_vmid == Some(vmid) {
        return Some("root@gold-e2e".to_owned());
    }
    if let Some(local) = target(home, vmid, None) {
        return Some(if gold { format!("root@{local}") } else { local });
    }
    if app.wait_for_details(DetailsAction::Ssh) {
        return None;
    }
    let login = app.selected_details().and_then(|details| {
        if gold {
            details.ssh.root.as_deref()
        } else {
            details.ssh.user.as_deref()
        }
    });
    let target = target(home, vmid, login);
    if target.is_none() {
        let time = app.clock.clone();
        app.log(
            &time,
            &format!("ssh {vmid}: no SSH entry on this desktop and no address yet"),
        );
    }
    target
}

/// What `ssh` is given to log in to workspace `vmid`: its desktop entry
/// `vm<VMID>` when the desktop has one (its keys and its `known_hosts`),
/// else the user login the server reports (`ssh USER@IP`), else nothing.
#[must_use]
pub fn target(home: &Path, vmid: u32, login: Option<&str>) -> Option<String> {
    if local_ssh::has_config(home, vmid) {
        return Some(format!("vm{vmid}"));
    }
    login
        .and_then(|login| login.strip_prefix("ssh "))
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(str::to_owned)
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
