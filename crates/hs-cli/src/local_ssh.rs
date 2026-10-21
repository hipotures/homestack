//! The desktop's SSH files of a workspace (Python `workspace_ssh.py`, I1):
//! `~/.ssh/config.d/homestack/vm<VMID>-<NAME>.conf` and `known_hosts`.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use hs_config::client::WorkspaceSsh;

use crate::{AppError, fido};

/// `~/.ssh/config.d/homestack`.
#[must_use]
pub fn config_directory(home: &Path) -> PathBuf {
    home.join(".ssh").join("config.d").join("homestack")
}

fn ssh_path(path: &str) -> String {
    if path.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '\\' | '#')) {
        format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        path.to_owned()
    }
}

/// The lines of a workspace's SSH config.
///
/// With keys of one kind (or unnamed ones) every `IdentityFile` is listed.
/// With keys of several security-key kinds ssh would try the handles of a
/// kind that is not connected, and the connected device would ask once per
/// handle, so the files are chosen by which kind is connected, in the order
/// of `fido_devices`.
#[must_use]
pub fn local_ssh_lines(home: &Path, ssh: &WorkspaceSsh, vmid: u32, name: &str, ip: &str) -> Vec<String> {
    // OpenSSH expands ~ using passwd, independently of the child's HOME.
    let files: Vec<String> = ssh
        .identity_files
        .iter()
        .map(|file| crate::expand_home(file, home).display().to_string())
        .collect();
    let mut kinds: Vec<&str> = Vec::new();
    for kind in files.iter().filter_map(|path| fido::kind_of(path)) {
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    let mut lines = vec![
        format!("Host {name} vm{vmid}"),
        format!("    HostName {ip}"),
        format!("    User {}", ssh.user),
    ];
    let mut tail = vec![format!(
        "    IdentitiesOnly {}",
        if ssh.identities_only { "yes" } else { "no" }
    )];
    // BatchMode makes ssh announce a security-key touch through SSH_ASKPASS
    // even on a terminal, so an askpass helper can play a sound.
    if !kinds.is_empty() {
        tail.push("    BatchMode yes".to_owned());
    }
    tail.extend([
        format!("    LogLevel {}", ssh.log_level),
        "    StrictHostKeyChecking accept-new".to_owned(),
        format!(
            "    UserKnownHostsFile {}",
            ssh_path(&known_hosts_file(home, vmid).display().to_string())
        ),
    ]);
    if kinds.len() < 2 {
        lines.extend(files.iter().map(|path| format!("    IdentityFile {}", ssh_path(path))));
        lines.extend(tail);
        return lines;
    }
    let mut order: Vec<&str> = ssh
        .fido_devices
        .iter()
        .map(String::as_str)
        .filter(|kind| kinds.contains(kind))
        .collect();
    for (kind, _, _) in fido::NAMES {
        if kinds.contains(&kind) && !order.contains(&kind) {
            order.push(kind);
        }
    }
    lines.extend(
        files
            .iter()
            .filter(|path| fido::kind_of(path).is_none())
            .map(|path| format!("    IdentityFile {}", ssh_path(path))),
    );
    lines.extend(tail);
    lines.push(String::new());
    lines.push(format!(
        "# Keys offered depend on the connected security key; preference: {}",
        order.join(", ")
    ));
    let mut earlier: Vec<&str> = Vec::new();
    for kind in &order {
        let tested: &[&str] = if earlier.is_empty() {
            std::slice::from_ref(kind)
        } else {
            &earlier
        };
        let vendors = tested
            .iter()
            .flat_map(|entry| {
                fido::NAMES
                    .iter()
                    .filter(move |(name, _, _)| name == entry)
                    .flat_map(|(_, ids, _)| ids.iter())
            })
            .map(|vendor| format!("vendor=0x{vendor}"))
            .collect::<Vec<_>>()
            .join("|");
        let test = format!("fido2-token -L 2>/dev/null | grep -qE '{vendors}'");
        let negate = if earlier.is_empty() { "" } else { "! " };
        lines.push(format!("Match originalhost {name},vm{vmid} exec \"{negate}{test}\""));
        lines.extend(
            files
                .iter()
                .filter(|path| fido::kind_of(path) == Some(kind))
                .map(|path| format!("    IdentityFile {}", ssh_path(path))),
        );
        earlier.push(kind);
    }
    lines
}

/// Write the workspace's SSH config atomically (mode 0600) and remove the
/// other files of the same VMID.
pub fn write_local_ssh_config(
    home: &Path,
    ssh: &WorkspaceSsh,
    vmid: u32,
    name: &str,
    ip: &str,
) -> Result<PathBuf, AppError> {
    write_local_ssh_config_with(home, ssh, vmid, name, ip, |from, to| std::fs::rename(from, to))
}

/// [`write_local_ssh_config`] with the final rename given (tests make it
/// fail).
pub fn write_local_ssh_config_with(
    home: &Path,
    ssh: &WorkspaceSsh,
    vmid: u32,
    name: &str,
    ip: &str,
    rename: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<PathBuf, AppError> {
    let directory = config_directory(home);
    let target = directory.join(format!("vm{vmid}-{name}.conf"));
    let content = local_ssh_lines(home, ssh, vmid, name, ip).join("\n") + "\n";
    let failed = |error: std::io::Error| {
        AppError::new(format!(
            "Could not write local SSH config {}: {error}",
            target.display()
        ))
    };
    std::fs::create_dir_all(&directory).map_err(failed)?;
    // ssh makes the `known_hosts` file the host config names, not its
    // directory.
    let known = known_hosts_directory(home);
    if !known.is_dir() {
        std::fs::create_dir_all(&known).map_err(failed)?;
        std::fs::set_permissions(&known, std::fs::Permissions::from_mode(0o700)).map_err(failed)?;
    }
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".vm{vmid}-{name}.conf."))
        .tempfile_in(&directory)
        .map_err(failed)?;
    temporary
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(failed)?;
    temporary.write_all(content.as_bytes()).map_err(failed)?;
    temporary.as_file().sync_all().map_err(failed)?;
    // The temporary file is removed when `temporary` is dropped unless the
    // rename moved it.
    rename(temporary.path(), &target).map_err(failed)?;
    let _ = temporary.into_temp_path().keep();
    let stale = matching(&directory, vmid).map_err(failed)?;
    for path in stale.iter().filter(|path| **path != target) {
        std::fs::remove_file(path).map_err(failed)?;
    }
    Ok(target)
}

/// Whether the desktop has an SSH config file of `vmid`.
#[must_use]
pub fn has_config(home: &Path, vmid: u32) -> bool {
    matching(&config_directory(home), vmid).is_ok_and(|found| !found.is_empty())
}

/// `vm<VMID>-*.conf` in `directory`, sorted.
fn matching(directory: &Path, vmid: u32) -> std::io::Result<Vec<PathBuf>> {
    let prefix = format!("vm{vmid}-");
    let mut found = Vec::new();
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let path = entry?.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with(&prefix) && name.ends_with(".conf") {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

/// Remove the SSH config files of `vmid` and nothing else.
pub fn remove_local_ssh_config(home: &Path, vmid: u32) -> Result<Vec<PathBuf>, AppError> {
    if vmid < 1 {
        return Err(AppError::new(format!(
            "Invalid VMID for local SSH config cleanup: {vmid}"
        )));
    }
    let directory = config_directory(home);
    let found = matching(&directory, vmid).map_err(|error| {
        AppError::new(format!(
            "Could not remove local SSH config {}: {error}",
            directory.display()
        ))
    })?;
    let mut removed = Vec::new();
    for path in found {
        std::fs::remove_file(&path)
            .map_err(|error| AppError::new(format!("Could not remove local SSH config {}: {error}", path.display())))?;
        removed.push(path);
    }
    Ok(removed)
}

/// `~/.ssh/config.d/homestack/known_hosts`: one file per workspace, named
/// in its host config (owner's decision of 2026-10-13), so the user's own
/// `~/.ssh/known_hosts` is never read or changed.
#[must_use]
pub fn known_hosts_directory(home: &Path) -> PathBuf {
    config_directory(home).join("known_hosts")
}

/// The `known_hosts` file of workspace `vmid`.
#[must_use]
pub fn known_hosts_file(home: &Path, vmid: u32) -> PathBuf {
    known_hosts_directory(home).join(format!("vm{vmid}"))
}

/// Remove the workspace's own `known_hosts` file, so that the next
/// connection learns the keys the machine has now. A file that is not there
/// (the workspace was never connected to) is nothing to remove.
pub fn forget_known_hosts(home: &Path, vmid: u32) -> Result<Vec<PathBuf>, AppError> {
    let file = known_hosts_file(home, vmid);
    match std::fs::remove_file(&file) {
        Ok(()) => Ok(vec![file]),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(Vec::new())
        }
        Err(error) => Err(AppError::new(format!(
            "Could not remove local SSH known_hosts file {}: {error}",
            file.display()
        ))),
    }
}

/// Likely OpenSSH FIDO identity stubs in `~/.ssh` (`id_*_sk*`, not
/// `.pub`), without reading them (Python `discover_hardware_identities`).
#[must_use]
pub fn discover_hardware_identities(home: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(home.join(".ssh")) else {
        return Vec::new();
    };
    let mut found: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.strip_prefix("id_").and_then(|rest| rest.find("_sk")).is_some() && !name.ends_with(".pub"))
        .map(|name| format!("~/.ssh/{name}"))
        .collect();
    found.sort();
    found
}
