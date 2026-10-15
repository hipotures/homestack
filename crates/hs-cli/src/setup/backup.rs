//! The Backup group: BK on the workspace (Python `backup.py`). `bk.py` and
//! its user units are Python's, sent unchanged; the desktop keeps two
//! restricted retrieval key pairs per workspace (`archive`, `status`) whose
//! public halves the workspace's `authorized_keys` allows for BK only.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use base64::Engine;
use rustix::fs::{AtFlags, FlockOperation, Mode, OFlags};
use serde_json::{Map, Value, json};

use super::engine::truthy;
use super::guest::{command_environment, guest, require_tool};
use super::workspace::Workspace;
use super::{AUTHORIZED_KEYS_PATH, Settings, sha256_hex};
use crate::AppError;
use crate::guest_args;

/// A file BK owns in the home: where, what, its mode.
pub struct Asset {
    pub path: &'static str,
    pub content: &'static [u8],
    pub mode: u32,
}

/// BK and its user units, byte for byte Python's.
pub const ASSETS: [Asset; 3] = [
    Asset {
        path: ".local/bin/bk",
        content: include_bytes!("../../guest/backup/bk.py"),
        mode: 0o755,
    },
    Asset {
        path: ".config/systemd/user/backup.service",
        content: include_bytes!("../../guest/backup/backup.service"),
        mode: 0o644,
    },
    Asset {
        path: ".config/systemd/user/backup.timer",
        content: include_bytes!("../../guest/backup/backup.timer"),
        mode: 0o644,
    },
];
/// The directories the install makes.
pub const DIRECTORIES: [&str; 3] = [".local/bin", ".config/systemd/user", "backup"];
/// The retrieval keys of a workspace.
pub const ROLES: [&str; 2] = ["archive", "status"];

/// BK's paths in the home.
#[must_use]
pub fn managed_paths() -> Vec<String> {
    ASSETS.iter().map(|asset| asset.path.to_owned()).collect()
}

/// [`managed_paths`] and the guest's `authorized_keys`.
#[must_use]
pub fn managed_state_paths() -> Vec<String> {
    let mut paths = managed_paths();
    paths.push(AUTHORIZED_KEYS_PATH.to_owned());
    paths
}

fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn gid() -> u32 {
    rustix::process::getgid().as_raw()
}

fn local_error(label: &str, error: impl std::fmt::Display) -> AppError {
    AppError::new(format!("Cannot inspect {label}: {error}"))
}

/// `~/.ssh/homestack/backup` on the desktop.
#[must_use]
pub fn retrieval_directory(home: &Path) -> PathBuf {
    home.join(".ssh").join("homestack").join("backup")
}

/// The private and public file of each role for `vmid`.
#[must_use]
pub fn retrieval_key_paths(home: &Path, vmid: u32) -> Vec<(&'static str, PathBuf, PathBuf)> {
    let directory = retrieval_directory(home);
    ROLES
        .iter()
        .map(|role| {
            (
                *role,
                directory.join(format!("vm{vmid}-bk-{role}")),
                directory.join(format!("vm{vmid}-bk-{role}.pub")),
            )
        })
        .collect()
}

fn canonical_comment(role: &str, vmid: u32) -> String {
    format!("homestack-bk-{role}-vm{vmid}")
}

fn check_directory(meta: &std::fs::Metadata, path: &Path, exact_mode: Option<u32>) -> Result<(), AppError> {
    if !meta.is_dir() {
        return Err(AppError::new(format!(
            "Unsafe BK retrieval path: {} is not a directory",
            path.display()
        )));
    }
    if meta.uid() != uid() || meta.gid() != gid() {
        return Err(AppError::new(format!(
            "Unsafe BK retrieval path: {} has unexpected ownership",
            path.display()
        )));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(AppError::new(format!(
            "Unsafe BK retrieval path: {} is group/world-writable",
            path.display()
        )));
    }
    if let Some(mode) = exact_mode
        && meta.mode() & 0o7777 != mode
    {
        return Err(AppError::new(format!(
            "BK retrieval directory has mode {:04o}; expected {mode:04o}",
            meta.mode() & 0o7777
        )));
    }
    Ok(())
}

fn lstat_directory(path: &Path) -> Result<Option<std::fs::Metadata>, AppError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(local_error(&path.display().to_string(), error)),
    };
    if meta.file_type().is_symlink() {
        return Err(AppError::new(format!(
            "Unsafe BK retrieval path: {} is a symlink",
            path.display()
        )));
    }
    check_directory(&meta, path, None)?;
    Ok(Some(meta))
}

/// The key directory, inspected (or made) without following symlinks.
fn safe_key_directory(home: &Path, create: bool) -> Result<(PathBuf, Option<std::fs::Metadata>), AppError> {
    let meta = std::fs::symlink_metadata(home).map_err(|error| local_error("desktop home", error))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(AppError::new(
            "Unsafe BK retrieval path: desktop home is not a directory",
        ));
    }
    if meta.uid() != uid() || meta.gid() != gid() {
        return Err(AppError::new(
            "Unsafe BK retrieval path: desktop home has unexpected ownership",
        ));
    }
    let mut current = home.to_path_buf();
    let mut info = None;
    for relative in [".ssh", ".ssh/homestack", ".ssh/homestack/backup"] {
        current = home.join(relative);
        info = lstat_directory(&current)?;
        if info.is_none() {
            if !create {
                return Ok((current, None));
            }
            match std::fs::create_dir(&current) {
                Ok(()) => {
                    let _ = std::fs::set_permissions(&current, std::fs::Permissions::from_mode(0o700));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(local_error(&current.display().to_string(), error)),
            }
            info = lstat_directory(&current)?;
            if info.is_none() {
                return Err(AppError::new(format!(
                    "BK retrieval directory disappeared: {}",
                    current.display()
                )));
            }
        }
    }
    Ok((current, info))
}

fn directory_fd(path: &Path) -> Result<OwnedFd, AppError> {
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| local_error(&path.display().to_string(), error))?;
    let meta = File::from(
        fd.try_clone()
            .map_err(|e| local_error(&path.display().to_string(), e))?,
    )
    .metadata()
    .map_err(|error| local_error(&path.display().to_string(), error))?;
    check_directory(&meta, path, None)?;
    Ok(fd)
}

/// A regular, owned, single-link key file, or nothing.
fn owned_file_info(path: &Path, label: &str) -> Result<Option<std::fs::Metadata>, AppError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(local_error(label, error)),
    };
    let shown = path.display();
    if meta.file_type().is_symlink() {
        return Err(AppError::new(format!("Unsafe BK retrieval key: {shown} is a symlink")));
    }
    if !meta.is_file() {
        return Err(AppError::new(format!(
            "Unsafe BK retrieval key: {shown} is not a regular file"
        )));
    }
    if meta.uid() != uid() || meta.gid() != gid() {
        return Err(AppError::new(format!(
            "Unsafe BK retrieval key: {shown} has unexpected ownership"
        )));
    }
    if meta.nlink() != 1 {
        return Err(AppError::new(format!(
            "Unsafe BK retrieval key: {shown} has multiple hard links"
        )));
    }
    Ok(Some(meta))
}

/// Read a key through an `O_NOFOLLOW` descriptor.
fn read_owned_file(path: &Path, label: &str) -> Result<Vec<u8>, AppError> {
    let fd = rustix::fs::open(path, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())
        .map_err(|error| local_error(label, error))?;
    let mut file = File::from(fd);
    let meta = file.metadata().map_err(|error| local_error(label, error))?;
    let shown = path.display();
    if !meta.is_file() {
        return Err(AppError::new(format!(
            "Unsafe BK retrieval key: {shown} is not a regular file"
        )));
    }
    if meta.uid() != uid() || meta.gid() != gid() {
        return Err(AppError::new(format!(
            "Unsafe BK retrieval key: {shown} has unexpected ownership"
        )));
    }
    if meta.nlink() != 1 {
        return Err(AppError::new(format!(
            "Unsafe BK retrieval key: {shown} has multiple hard links"
        )));
    }
    let mut payload = Vec::new();
    file.read_to_end(&mut payload)
        .map_err(|error| local_error(label, error))?;
    Ok(payload)
}

/// `(type, blob, comment)` of a one-line ED25519 public key.
fn parse_public_identity(payload: &[u8], label: &str) -> Result<(String, String, String), AppError> {
    let text = std::str::from_utf8(payload).map_err(|_| AppError::new(format!("{label} is not valid UTF-8")))?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() != 1 {
        return Err(AppError::new(format!("{label} must contain exactly one public key")));
    }
    let parts: Vec<&str> = lines[0].split_whitespace().collect();
    if parts.len() < 2 || parts[0] != "ssh-ed25519" {
        return Err(AppError::new(format!("{label} is not an ED25519 public key")));
    }
    base64::engine::general_purpose::STANDARD
        .decode(parts[1])
        .map_err(|_| AppError::new(format!("{label} has an invalid public key blob")))?;
    Ok((parts[0].to_owned(), parts[1].to_owned(), parts[2..].join(" ")))
}

fn ssh_keygen_public(private: &str) -> Result<(String, String, String), AppError> {
    let output = Command::new("ssh-keygen")
        .args(["-y", "-P", "", "-f", private])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            AppError::new(format!(
                "Could not derive BK retrieval public key from {private}: {error}"
            ))
        })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        })
        .trim()
        .to_owned();
        let last = detail
            .lines()
            .last()
            .map(|line| format!(": {line}"))
            .unwrap_or_default();
        return Err(AppError::new(format!(
            "Could not derive BK retrieval public key from {private}{last}"
        )));
    }
    parse_public_identity(&output.stdout, &format!("Derived public key for {private}"))
}

/// The public identity of private key bytes, without reopening a path
/// that could be replaced: the bytes go to a memory file.
fn derive_identity(payload: &[u8]) -> Result<(String, String, String), AppError> {
    let failed = |error: std::io::Error| AppError::new(format!("Could not derive a BK retrieval public key: {error}"));
    let fd = rustix::fs::memfd_create("homestack-bk-key", rustix::fs::MemfdFlags::empty())
        .map_err(|error| failed(error.into()))?;
    rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o600)).map_err(|error| failed(error.into()))?;
    let mut file = File::from(fd);
    file.write_all(payload).map_err(failed)?;
    file.flush().map_err(failed)?;
    // The memory file has no close-on-exec flag: ssh-keygen inherits it.
    ssh_keygen_public(&format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn canonical_public(identity: &(String, String, String), role: &str, vmid: u32) -> Result<String, AppError> {
    if identity.0 != "ssh-ed25519" {
        return Err(AppError::new("BK retrieval key is not ED25519"));
    }
    Ok(format!(
        "{} {} {}",
        identity.0,
        identity.1,
        canonical_comment(role, vmid)
    ))
}

fn inspect_pair(
    role: &str,
    private: &Path,
    public: &Path,
    vmid: u32,
    directory_exists: bool,
) -> Result<Value, AppError> {
    let mut result = json!({
        "role": role, "private_path": private.display().to_string(), "public_path": public.display().to_string(),
        "private_exists": false, "public_exists": false, "private_mode": null, "public_mode": null,
        "state": "missing", "ready": false, "public_key": "",
    });

    if !directory_exists {
        return Ok(result);
    }
    let private_info = owned_file_info(private, &format!("private BK {role} key"))?;
    let public_info = owned_file_info(public, &format!("public BK {role} key"))?;
    result["private_exists"] = json!(private_info.is_some());
    result["public_exists"] = json!(public_info.is_some());
    result["private_mode"] = private_info.as_ref().map_or(Value::Null, |m| json!(m.mode() & 0o7777));
    result["public_mode"] = public_info.as_ref().map_or(Value::Null, |m| json!(m.mode() & 0o7777));
    let private_payload = if private_info.is_some() {
        read_owned_file(private, &format!("private BK {role} key"))?
    } else {
        Vec::new()
    };
    let public_payload = if public_info.is_some() {
        read_owned_file(public, &format!("public BK {role} key"))?
    } else {
        Vec::new()
    };
    let mut private_identity = None;
    if private_info.is_some() {
        let identity = derive_identity(&private_payload)?;
        result["public_key"] = json!(canonical_public(&identity, role, vmid)?);
        private_identity = Some(identity);
    }
    if public_info.is_some() {
        let public_identity = match parse_public_identity(&public_payload, &format!("Public BK {role} key")) {
            Ok(identity) => identity,
            Err(error) => {
                if private_identity.is_some() {
                    return Err(error);
                }
                result["state"] = json!("orphan-public");
                return Ok(result);
            }
        };
        let Some(private_identity) = &private_identity else {
            result["state"] = json!("orphan-public");
            return Ok(result);
        };
        if (&private_identity.0, &private_identity.1) != (&public_identity.0, &public_identity.1) {
            return Err(AppError::new(format!(
                "BK retrieval {role} private/public key mismatch; refusing to rotate credentials"
            )));
        }
    }
    if private_info.is_some() && public_info.is_some() {
        result["state"] = json!("valid");
        result["ready"] = json!(result["private_mode"] == json!(0o600) && result["public_mode"] == json!(0o644));
    } else if private_info.is_some() {
        result["state"] = json!("missing-public");
    }
    Ok(result)
}

/// The desktop's BK key pairs of `vmid`, read without creating or changing
/// anything.
pub fn inspect_retrieval_keys(home: &Path, vmid: u32) -> Result<Value, AppError> {
    let (_, info) = safe_key_directory(home, false)?;
    let mut pairs = Map::new();
    let mut key_paths = Map::new();
    let mut public_keys = Map::new();
    let mut identities: Vec<(String, String)> = Vec::new();
    for (role, private, public) in retrieval_key_paths(home, vmid) {
        let pair = inspect_pair(role, &private, &public, vmid, info.is_some())?;
        let key = pair["public_key"].as_str().unwrap_or_default().to_owned();
        if !key.is_empty() {
            let identity = key.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
            if let Some((_, previous)) = identities.iter().find(|(known, _)| *known == identity) {
                return Err(AppError::new(format!(
                    "BK retrieval keys for {previous} and {role} use the same public identity; refusing to continue"
                )));
            }
            identities.push((identity, role.to_owned()));
        }
        key_paths.insert(
            role.to_owned(),
            json!({"private": pair["private_path"], "public": pair["public_path"]}),
        );
        public_keys.insert(role.to_owned(), json!(key));
        pairs.insert(role.to_owned(), pair);
    }
    let ready = info.as_ref().is_some_and(|meta| meta.mode() & 0o7777 == 0o700)
        && pairs.values().all(|pair| pair["ready"] == json!(true));
    let exists = pairs
        .values()
        .any(|pair| pair["private_exists"] == json!(true) || pair["public_exists"] == json!(true));
    Ok(json!({
        "directory": retrieval_directory(home).display().to_string(),
        "key_paths": key_paths, "pairs": pairs, "public_keys": public_keys,
        "ready": ready, "exists": exists,
    }))
}

/// Who and what a path is, to notice a change between check and use.
/// Device, inode, mtime, ctime, size, mode, owner, group and links.
type FileIdentity = (u64, u64, i64, i64, i64, u32, u32, u32, u64);

fn identity_of(meta: Option<&rustix::fs::Stat>) -> Option<FileIdentity> {
    meta.map(|s| {
        (
            s.st_dev,
            s.st_ino,
            s.st_mtime * 1_000_000_000 + i64::try_from(s.st_mtime_nsec).unwrap_or(0),
            s.st_ctime * 1_000_000_000 + i64::try_from(s.st_ctime_nsec).unwrap_or(0),
            s.st_size,
            s.st_mode,
            s.st_uid,
            s.st_gid,
            s.st_nlink,
        )
    })
}

fn statat(directory: &OwnedFd, name: &str) -> Result<Option<rustix::fs::Stat>, std::io::Error> {
    match rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(stat)),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Write `path` through its pinned parent: a new temporary file, then a
/// rename that fails if the parent or the file changed in between.
fn atomic_local_write(home: &Path, path: &Path, payload: &[u8], mode: u32, replace: bool) -> Result<(), AppError> {
    let parent = path.parent().unwrap_or(Path::new("/"));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let shown = path.display().to_string();
    let directory = directory_fd(parent)?;
    let current = statat(&directory, &name).map_err(|error| local_error(&shown, error))?;
    if let Some(stat) = &current {
        let file_type = rustix::fs::FileType::from_raw_mode(stat.st_mode);
        if file_type != rustix::fs::FileType::RegularFile {
            return Err(AppError::new(format!(
                "Unsafe BK retrieval key: {shown} is not a regular file"
            )));
        }
        if stat.st_uid != uid() || stat.st_gid != gid() || stat.st_nlink != 1 {
            return Err(AppError::new(format!(
                "Unsafe BK retrieval key: {shown} has unsafe ownership or links"
            )));
        }
        if !replace {
            return Err(AppError::new(format!(
                "BK retrieval key appeared during reconciliation: {shown}"
            )));
        }
    }
    let mut temporary = None;
    for _ in 0..32 {
        let candidate = format!(".bk-{}", &uuid::Uuid::new_v4().simple().to_string()[..24]);
        match rustix::fs::openat(
            &directory,
            candidate.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(fd) => {
                temporary = Some((candidate, fd));
                break;
            }
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(local_error(&shown, error)),
        }
    }
    let Some((temporary_name, fd)) = temporary else {
        return Err(AppError::new(format!(
            "Could not create a temporary BK retrieval file in {}",
            parent.display()
        )));
    };
    let outcome = (|| -> Result<(), AppError> {
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode)).map_err(|error| local_error(&shown, error))?;
        let mut file = File::from(fd);
        file.write_all(payload).map_err(|error| local_error(&shown, error))?;
        file.sync_all().map_err(|error| local_error(&shown, error))?;
        let (_, named) = safe_key_directory(home, false)?;
        let pinned = rustix::fs::fstat(&directory).map_err(|error| local_error(&shown, error))?;
        if named.is_none_or(|meta| (meta.dev(), meta.ino()) != (pinned.st_dev, pinned.st_ino)) {
            return Err(AppError::new("BK retrieval directory changed during reconciliation"));
        }
        let latest = statat(&directory, &name).map_err(|error| local_error(&shown, error))?;
        owned_file_info(path, "BK retrieval key")?;
        if identity_of(latest.as_ref()) != identity_of(current.as_ref()) {
            return Err(AppError::new(format!(
                "BK retrieval key changed during reconciliation: {shown}"
            )));
        }
        rustix::fs::renameat(&directory, temporary_name.as_str(), &directory, name.as_str())
            .map_err(|error| local_error(&shown, error))?;
        rustix::fs::fsync(&directory).map_err(|error| local_error(&shown, error))?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = rustix::fs::unlinkat(&directory, temporary_name.as_str(), AtFlags::empty());
    }
    outcome
}

/// A new pair: the public half first, so an interruption leaves an orphan
/// public key the next run replaces.
fn generate_pair(
    home: &Path,
    role: &str,
    vmid: u32,
    directory: &Path,
    private: &Path,
    public: &Path,
) -> Result<(), AppError> {
    let comment = canonical_comment(role, vmid);
    let temporary = tempfile::Builder::new()
        .prefix(".bk-keygen-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(directory)
        .map_err(|error| local_error(&directory.display().to_string(), error))?;
    let temporary_private = temporary.path().join(private.file_name().unwrap_or_default());
    let output = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", &comment, "-f"])
        .arg(&temporary_private)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| AppError::new(format!("Could not generate BK retrieval {role} key: {error}")))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        })
        .trim()
        .to_owned();
        let last = detail
            .lines()
            .last()
            .map(|line| format!(": {line}"))
            .unwrap_or_default();
        return Err(AppError::new(format!(
            "Could not generate BK retrieval {role} key{last}"
        )));
    }
    let private_payload = read_owned_file(&temporary_private, &format!("generated BK {role} key"))?;
    let identity = ssh_keygen_public(&temporary_private.display().to_string())?;
    if identity.2 != comment {
        return Err(AppError::new(format!(
            "Generated BK retrieval {role} key has an unexpected comment"
        )));
    }
    let public_payload = format!("{}\n", canonical_public(&identity, role, vmid)?);
    atomic_local_write(home, public, public_payload.as_bytes(), 0o644, true)?;
    atomic_local_write(home, private, &private_payload, 0o600, false)?;
    Ok(())
}

/// Create or repair both key pairs of `vmid`, keeping valid key bytes.
pub fn reconcile_retrieval_keys(home: &Path, vmid: u32) -> Result<Value, AppError> {
    // Read-only first: a mismatch is refused before anything is made.
    inspect_retrieval_keys(home, vmid)?;
    let (directory, info) = safe_key_directory(home, true)?;
    if info.is_none() {
        return Err(AppError::new("BK retrieval key directory could not be created"));
    }
    let lock = directory_fd(&directory)?;
    rustix::fs::flock(&lock, FlockOperation::LockExclusive).map_err(|_| {
        AppError::new(format!(
            "Could not lock BK retrieval key directory: {}",
            directory.display()
        ))
    })?;
    let outcome = (|| -> Result<(), AppError> {
        let current = inspect_retrieval_keys(home, vmid)?;
        rustix::fs::fchmod(&lock, Mode::from_raw_mode(0o700))
            .map_err(|error| local_error(&directory.display().to_string(), error))?;
        for (role, private, public) in retrieval_key_paths(home, vmid) {
            let pair = &current["pairs"][role];
            let chmod = |path: &Path, mode: u32| {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                    .map_err(|error| local_error(&path.display().to_string(), error))
            };
            if pair["state"] == json!("valid") {
                if pair["private_mode"] != json!(0o600) {
                    chmod(&private, 0o600)?;
                }
                if pair["public_mode"] != json!(0o644) {
                    chmod(&public, 0o644)?;
                }
            } else if pair["private_exists"] == json!(true) && pair["public_exists"] != json!(true) {
                let payload = read_owned_file(&private, &format!("private BK {role} key"))?;
                let identity = derive_identity(&payload)?;
                if pair["private_mode"] != json!(0o600) {
                    chmod(&private, 0o600)?;
                }
                let line = format!("{}\n", canonical_public(&identity, role, vmid)?);
                atomic_local_write(home, &public, line.as_bytes(), 0o644, false)?;
            } else {
                generate_pair(home, role, vmid, &directory, &private, &public)?;
            }
        }
        Ok(())
    })();
    let _ = rustix::fs::flock(&lock, FlockOperation::Unlock);
    outcome?;
    let final_state = inspect_retrieval_keys(home, vmid)?;
    if final_state["ready"] != json!(true) {
        return Err(AppError::new(
            "BK retrieval key reconciliation did not produce two valid key pairs",
        ));
    }
    Ok(final_state)
}

fn guest_authorized_keys(
    workspace: &mut dyn Workspace,
    settings: &Settings,
    vmid: u32,
    public_keys: &Value,
    apply: bool,
    expected_sha256: &Value,
) -> Result<Value, AppError> {
    let mut keys = Map::new();
    for role in ROLES {
        keys.insert(role.to_owned(), json!(public_keys[role].as_str().unwrap_or_default()));
    }
    if keys.values().any(|key| key.as_str().is_none_or(str::is_empty)) {
        keys.clear();
    }
    guest(
        workspace,
        settings,
        "backup-authorized-keys",
        guest_args! {"vmid": vmid, "keys": keys, "apply": apply, "expected_sha256": expected_sha256},
    )
}

/// A shell command on the workspace (or the desktop with `--local`).
fn command(workspace: &mut dyn Workspace, settings: &Settings, line: &str) -> Result<i32, AppError> {
    if workspace.local() {
        return Ok(workspace.local_command(line)?.code);
    }
    Ok(workspace
        .run(&command_environment(settings, line, "sh", &[], false), None, false)?
        .code)
}

fn systemctl(workspace: &mut dyn Workspace, settings: &Settings, arguments: &str) -> Result<i32, AppError> {
    if workspace.local() {
        return Ok(workspace.local_command(&format!("systemctl --user {arguments}"))?.code);
    }
    command(
        workspace,
        settings,
        &format!(
            "env XDG_RUNTIME_DIR=/run/user/{} systemctl --user {arguments}",
            settings.uid
        ),
    )
}

fn require_runtime(workspace: &mut dyn Workspace, settings: &Settings) -> Result<(), AppError> {
    let preparation = if workspace.local() {
        "install local system prerequisites separately"
    } else {
        "install system prerequisites on the target VM separately"
    };
    for tool in ["python3", "file", "git", "systemctl"] {
        require_tool(workspace, settings, tool, &[])?;
    }
    if command(workspace, settings, "python3 -c 'import curses, rich, sqlite3'")? != 0 {
        return Err(AppError::new(format!(
            "BK requires Python modules curses, rich and sqlite3; {preparation}"
        )));
    }
    if systemctl(workspace, settings, "show-environment")? != 0 {
        return Err(AppError::new(
            "The user systemd manager is unavailable; prepare the user session separately",
        ));
    }
    Ok(())
}

fn timer_state(workspace: &mut dyn Workspace, settings: &Settings) -> Result<(bool, bool), AppError> {
    let enabled = systemctl(workspace, settings, "is-enabled backup.timer")? == 0;
    let active = systemctl(workspace, settings, "is-active backup.timer")? == 0;
    Ok((enabled, active))
}

/// BK's files, timer and keys, read without reading BK's own data.
pub fn inspect(
    workspace: &mut dyn Workspace,
    settings: &Settings,

    vmid: u32,
    check_requirements: bool,
) -> Result<Value, AppError> {
    let local = workspace.local();
    let directories: Vec<&str> = DIRECTORIES
        .iter()
        .copied()
        .filter(|path| !local || *path != "backup")
        .collect();
    if check_requirements {
        require_runtime(workspace, settings)?;
        if !local && crate::system::which("ssh-keygen").is_none() {
            return Err(AppError::new("BK retrieval requires desktop ssh-keygen"));
        }
        if !local && command(workspace, settings, "test -x /usr/bin/cat")? != 0 {
            return Err(AppError::new(
                "BK retrieval requires guest /usr/bin/cat; install system prerequisites on the target VM separately",
            ));
        }
    }
    let retrieval = if local {
        json!({"ready": true})
    } else {
        inspect_retrieval_keys(&settings.home, vmid)?
    };
    let authorization = if local {
        json!({"ready": true})
    } else {
        guest_authorized_keys(
            workspace,
            settings,
            vmid,
            &retrieval["public_keys"],
            false,
            &Value::Null,
        )?
    };
    let paths: Vec<Value> = directories
        .iter()
        .map(|path| json!({"relative": path, "directory": true}))
        .collect();
    guest(workspace, settings, "paths", guest_args! {"paths": paths})?;
    let actual = guest(
        workspace,
        settings,
        "managed-files-inspect",
        guest_args! {"paths": managed_paths()},
    )?;
    let configuration = guest(workspace, settings, "backup-config", Map::new())?;
    let files = actual["items"].as_array().cloned().unwrap_or_default();
    let by_path = |path: &str| {
        files
            .iter()
            .find(|item| item["path"] == json!(path))
            .cloned()
            .unwrap_or(json!({}))
    };
    let changed: Vec<String> = ASSETS
        .iter()
        .filter(|asset| {
            let item = by_path(asset.path);
            !truthy(&item["exists"])
                || item["sha256"] != json!(sha256_hex(asset.content))
                || item["mode"] != json!(asset.mode)
        })
        .map(|asset| asset.path.to_owned())
        .collect();
    let mut snapshot_paths: Vec<String> = changed
        .iter()
        .filter(|path| truthy(&by_path(path)["exists"]))
        .cloned()
        .collect();
    if truthy(&authorization["exists"]) && !truthy(&authorization["ready"]) {
        snapshot_paths.push(AUTHORIZED_KEYS_PATH.to_owned());
    }
    let (timer_enabled, timer_active) = timer_state(workspace, settings)?;
    let any_installed = files.iter().any(|item| truthy(&item["exists"]));
    let ready = changed.is_empty()
        && timer_enabled
        && timer_active
        && truthy(&retrieval["ready"])
        && truthy(&authorization["ready"])
        && truthy(&configuration["exists"]);
    let state = if ready {
        "configured"
    } else if !any_installed {
        "not installed"
    } else {
        "needs update"
    };
    Ok(json!({
        "state": state, "ready": ready, "exists": any_installed, "will_overwrite": !snapshot_paths.is_empty(),
        "files": files, "managed_paths": if local { managed_paths() } else { managed_state_paths() },
        "changed_paths": changed, "snapshot_paths": snapshot_paths,
        "timer_enabled": timer_enabled, "timer_active": timer_active,
        "retrieval": retrieval, "authorization": authorization,
        "configuration_exists": configuration["exists"],
    }))
}

/// Install or update BK's files, the keys and the timer.
pub fn apply(
    workspace: &mut dyn Workspace,
    settings: &Settings,

    vmid: u32,
    state: &Value,
    activity: &mut dyn FnMut(&str),
) -> Result<(String, String), AppError> {
    let local = workspace.local();
    if truthy(&state["ready"]) {
        let mut detail = "BK assets and user timer already match".to_owned();
        if !local {
            detail.push_str("; restricted retrieval keys configured");
        }
        return Ok(("already-ready".to_owned(), detail));
    }
    if !local {
        activity("Reconcile desktop BK retrieval keys");
        let retrieval = reconcile_retrieval_keys(&settings.home, vmid)?;
        activity("Install restricted BK public keys");
        guest_authorized_keys(
            workspace,
            settings,
            vmid,
            &retrieval["public_keys"],
            true,
            &state["authorization"].get("sha256").cloned().unwrap_or(Value::Null),
        )?;
    }
    let files = state["files"].as_array().cloned().unwrap_or_default();
    let changed: Vec<String> = match state.get("changed_paths").and_then(Value::as_array) {
        Some(paths) => paths.iter().filter_map(|p| p.as_str().map(str::to_owned)).collect(),
        None => managed_paths(),
    };
    let payloads: Vec<Value> = ASSETS
        .iter()
        .filter(|asset| changed.iter().any(|path| path == asset.path))
        .map(|asset| {
            let expected = files
                .iter()
                .find(|item| item["path"] == json!(asset.path))
                .and_then(|item| item.get("sha256").cloned())
                .unwrap_or(Value::Null);
            json!({
                "path": asset.path,
                "content": base64::engine::general_purpose::STANDARD.encode(asset.content),
                "sha256": sha256_hex(asset.content),
                "expected_sha256": expected,
                "mode": asset.mode,
            })
        })
        .collect();
    activity("Install or update managed BK assets");
    let directories: Vec<&str> = DIRECTORIES
        .iter()
        .copied()
        .filter(|path| !local || *path != "backup")
        .collect();
    guest(
        workspace,
        settings,
        "managed-files-install",
        guest_args! {"directories": directories, "files": payloads},
    )?;
    activity("Create missing BK configuration; preserve existing user settings");
    guest(workspace, settings, "backup-config", guest_args! {"create": true})?;
    let units = [
        ".config/systemd/user/backup.service",
        ".config/systemd/user/backup.timer",
    ];
    if changed.iter().any(|path| units.contains(&path.as_str())) {
        activity("Reload the user systemd manager");
        if systemctl(workspace, settings, "daemon-reload")? != 0 {
            return Err(AppError::new("User systemd daemon-reload failed"));
        }
    }
    activity("Enable and start backup.timer");
    if systemctl(workspace, settings, "enable --now backup.timer")? != 0 {
        return Err(AppError::new("Could not enable and start backup.timer"));
    }
    let (enabled, active) = timer_state(workspace, settings)?;
    if !enabled || !active {
        return Err(AppError::new("backup.timer did not become enabled and active"));
    }
    let verified = inspect(workspace, settings, vmid, false)?;
    if !truthy(&verified["ready"]) {
        return Err(AppError::new("BK installation verification failed"));
    }
    let action = if state["state"] == json!("not installed") {
        "installed"
    } else {
        "updated"
    };
    let detail = format!("BK {action}; backup.timer is enabled and active");
    Ok((
        "succeeded".to_owned(),
        if local {
            detail
        } else {
            format!("{detail}; restricted retrieval keys configured")
        },
    ))
}
