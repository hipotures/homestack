//! Descriptor-relative desktop storage. No secrets are serialized into settings or recovery records.
use super::pangolin_key::Credential;
use rustix::fs::{self, AtFlags, FlockOperation, Mode, OFlags};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use zeroize::Zeroizing;

const IO: &str = "Secure local storage failed. Check the private destination and F7 recovery details.";
const UNSAFE: &str = "Use an absolute private path outside Git, without symlinks, owned by this user (directory 0700, file 0600 or stricter).";
pub(super) type Result<T> = std::result::Result<T, String>;

/// Keep each ancestor open while walking; no pathname-based check/open races.
fn parent(path: &Path, create: bool) -> Result<(OwnedFd, String)> {
    if !path.is_absolute()
        || path
            .to_str()
            .is_none_or(|s| s.chars().any(char::is_control) || s.split('/').any(|p| p == "." || p == ".."))
    {
        return Err(UNSAFE.into());
    }
    let mut parts = path.components().peekable();
    if parts.next() != Some(Component::RootDir) {
        return Err(UNSAFE.into());
    }
    let mut dir = fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| IO)?;
    let uid = rustix::process::getuid().as_raw();
    loop {
        // Includes worktrees (.git file) and ordinary repositories (.git directory).
        if fs::statat(&dir, ".git", AtFlags::SYMLINK_NOFOLLOW).is_ok()
            || (fs::statat(&dir, "HEAD", AtFlags::SYMLINK_NOFOLLOW).is_ok()
                && fs::statat(&dir, "objects", AtFlags::SYMLINK_NOFOLLOW).is_ok())
        {
            return Err("Credential paths inside Git repositories are refused.".into());
        }
        let Some(Component::Normal(part)) = parts.next() else {
            return Err(UNSAFE.into());
        };
        if parts.peek().is_none() {
            let info = fs::fstat(&dir).map_err(|_| IO)?;
            if info.st_uid != uid || info.st_mode & 0o077 != 0 {
                return Err(UNSAFE.into());
            }
            return Ok((dir, part.to_str().ok_or(UNSAFE)?.to_owned()));
        }
        if create {
            match fs::mkdirat(&dir, part, Mode::from_raw_mode(0o700)) {
                Ok(()) => fs::fsync(&dir).map_err(|_| IO)?,
                Err(rustix::io::Errno::EXIST) => {}
                Err(_) => return Err(IO.into()),
            }
        }
        let next = fs::openat(
            &dir,
            part,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| UNSAFE)?;
        let info = fs::fstat(&next).map_err(|_| IO)?;
        let sticky_root = info.st_uid == 0 && info.st_mode & 0o1000 != 0;
        if (info.st_uid != uid && info.st_uid != 0) || (info.st_mode & 0o022 != 0 && !sticky_root) {
            return Err(UNSAFE.into());
        }
        dir = next;
    }
}
fn file(dir: &OwnedFd, name: &str, create: bool) -> Result<File> {
    let flags = OFlags::NOFOLLOW
        | OFlags::CLOEXEC
        | OFlags::NONBLOCK
        | if create {
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL
        } else {
            OFlags::RDONLY
        };
    let file = File::from(fs::openat(dir, name, flags, Mode::from_raw_mode(0o600)).map_err(|_| IO)?);
    let info = file.metadata().map_err(|_| IO)?;
    if !info.is_file()
        || info.uid() != rustix::process::getuid().as_raw()
        || info.mode() & 0o177 != 0
        || info.nlink() != 1
    {
        return Err(UNSAFE.into());
    }
    Ok(file)
}
fn bounded(mut file: File, max: u64) -> Result<Zeroizing<String>> {
    if file.metadata().map_err(|_| IO)?.len() > max {
        return Err("Credential or recovery file exceeds its size limit.".into());
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(max as usize + 1));
    Read::by_ref(&mut file)
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| IO)?;
    if bytes.len() > max as usize {
        return Err(IO.into());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "Invalid credential file content.")?;
    Ok(Zeroizing::new(text.to_owned()))
}
pub(super) fn read_credential(path: &Path) -> Result<Credential> {
    let (dir, name) = parent(path, false)?;
    let content = bounded(file(&dir, &name, false)?, 4096)?;
    // The established file contains ID.secret and a newline, regardless of suffix.
    let mut credential = Credential::default();
    credential.insert(content.trim());
    credential
        .key_id()
        .map_err(|_| "Invalid credential content. Expected the complete ID.secret token.")?;
    Ok(credential)
}

pub(super) struct Storage {
    dir: OwnedFd,
    // Held for the entire operation, including all network requests.
    _lock: File,
    key: String,
    pending: String,
    recovery: String,
    lock: String,
}
impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        let (dir, name) = parent(path, true)?;
        let legacy = name == "homestack.key";
        let pending = if legacy {
            "credential.pending".into()
        } else {
            format!("{name}.pending")
        };
        let recovery = if legacy {
            "recovery.json".into()
        } else {
            format!("{name}.recovery.json")
        };
        let lock_name = if legacy {
            "operation.lock".into()
        } else {
            format!("{name}.lock")
        };
        let lock = File::from(
            fs::openat(
                &dir,
                lock_name.as_str(),
                OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|_| IO)?,
        );
        let info = lock.metadata().map_err(|_| IO)?;
        if !info.is_file()
            || info.uid() != rustix::process::getuid().as_raw()
            || info.mode() & 0o177 != 0
            || info.nlink() != 1
        {
            return Err(UNSAFE.into());
        }
        fs::flock(&lock, FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| "Another HomeStack key operation holds this destination.")?;
        Ok(Self {
            dir,
            _lock: lock,
            key: name,
            pending,
            recovery,
            lock: lock_name,
        })
    }
    pub fn load<T: serde::de::DeserializeOwned>(&self) -> Result<Option<T>> {
        match fs::statat(&self.dir, self.recovery.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(_) => Err(IO.into()),
            Ok(_) => {
                let content = bounded(file(&self.dir, self.recovery.as_str(), false)?, 16384)?;
                serde_json::from_str(&content)
                    .map(Some)
                    .map_err(|_| "Invalid recovery record. Resolve it explicitly before provisioning.".into())
            }
        }
    }
    pub fn save<T: serde::Serialize>(&self, record: &T) -> Result<()> {
        let name = format!("state-{}.tmp", uuid::Uuid::new_v4());
        let mut out = file(&self.dir, &name, true)?;
        let result = (|| {
            serde_json::to_writer(&mut out, record).map_err(|_| IO)?;
            out.sync_all().map_err(|_| IO)?;
            fs::renameat(&self.dir, name.as_str(), &self.dir, self.recovery.as_str()).map_err(|_| IO)?;
            fs::fsync(&self.dir).map_err(|_| IO)?;
            Ok(())
        })();
        let _ = fs::unlinkat(&self.dir, name.as_str(), AtFlags::empty());
        result
    }
    pub fn reserve(&self) -> Result<()> {
        if fs::statat(&self.dir, self.key.as_str(), AtFlags::SYMLINK_NOFOLLOW).is_ok() {
            return Err("Credential destination already exists; it will never be overwritten.".into());
        }
        let out = file(&self.dir, self.pending.as_str(), true)?;
        out.sync_all().map_err(|_| IO)?;
        fs::fsync(&self.dir).map_err(|_| IO)?;
        Ok(())
    }
    pub fn has_credential(&self) -> bool {
        fs::statat(&self.dir, self.key.as_str(), AtFlags::SYMLINK_NOFOLLOW).is_ok()
    }
    pub fn has_pending(&self) -> bool {
        fs::statat(&self.dir, self.pending.as_str(), AtFlags::SYMLINK_NOFOLLOW).is_ok()
    }
    /// Before mutation only, an empty reservation can safely be released.
    pub fn release(&self) -> Result<()> {
        if self.has_pending() {
            if file(&self.dir, self.pending.as_str(), false)?
                .metadata()
                .map_err(|_| IO)?
                .len()
                != 0
            {
                return Err("Pending credential requires recovery; creation is blocked.".into());
            }
            fs::unlinkat(&self.dir, self.pending.as_str(), AtFlags::empty()).map_err(|_| IO)?;
            fs::fsync(&self.dir).map_err(|_| IO)?;
        }
        Ok(())
    }
    pub fn persist(&self, credential: &Credential) -> Result<()> {
        credential.key_id().map_err(|_| IO)?;
        // A durable temporary inode is published atomically, without overwriting any file.
        let mut out = File::from(
            fs::openat(
                &self.dir,
                self.pending.as_str(),
                OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .map_err(|_| IO)?,
        );
        let info = out.metadata().map_err(|_| IO)?;
        if !info.is_file()
            || info.uid() != rustix::process::getuid().as_raw()
            || info.mode() & 0o177 != 0
            || info.len() != 0
            || info.nlink() != 1
        {
            return Err(IO.into());
        }
        out.write_all(credential.value.as_bytes()).map_err(|_| IO)?;
        out.write_all(b"\n").map_err(|_| IO)?;
        out.sync_all().map_err(|_| IO)?;
        fs::linkat(
            &self.dir,
            self.pending.as_str(),
            &self.dir,
            self.key.as_str(),
            AtFlags::empty(),
        )
        .map_err(|_| IO)?;
        fs::fsync(&self.dir).map_err(|_| IO)?;
        fs::unlinkat(&self.dir, self.pending.as_str(), AtFlags::empty()).map_err(|_| IO)?;
        fs::fsync(&self.dir).map_err(|_| IO)?;
        Ok(())
    }
    /// Delete the credential, any pending credential, the recovery record and
    /// the lock's name (the lock itself is held until this is dropped).
    pub fn remove(&self) -> Result<()> {
        for name in [&self.key, &self.pending, &self.recovery, &self.lock] {
            match fs::unlinkat(&self.dir, name.as_str(), AtFlags::empty()) {
                Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                Err(_) => return Err(IO.into()),
            }
        }
        fs::fsync(&self.dir).map_err(|_| IO)?;
        Ok(())
    }
    /// Recover a crash between writing the pending inode and publishing its name.
    pub fn recover_credential(&self, path: &Path) -> Result<Credential> {
        if !self.has_pending() {
            return read_credential(path);
        }
        let pending = path.with_file_name(self.pending.as_str());
        // A crash after linkat leaves two names for the same inode. Verify both before removing one.
        if let Ok(key) = fs::statat(&self.dir, self.key.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            let temp = fs::statat(&self.dir, self.pending.as_str(), AtFlags::SYMLINK_NOFOLLOW).map_err(|_| IO)?;
            if key.st_ino != temp.st_ino || key.st_dev != temp.st_dev {
                return Err(IO.into());
            }
        } else {
            read_credential(&pending)?;
            file(&self.dir, self.pending.as_str(), false)?
                .sync_all()
                .map_err(|_| IO)?;
            fs::linkat(
                &self.dir,
                self.pending.as_str(),
                &self.dir,
                self.key.as_str(),
                AtFlags::empty(),
            )
            .map_err(|_| IO)?;
        }
        fs::fsync(&self.dir).map_err(|_| IO)?;
        fs::unlinkat(&self.dir, self.pending.as_str(), AtFlags::empty()).map_err(|_| IO)?;
        fs::fsync(&self.dir).map_err(|_| IO)?;
        read_credential(path)
    }
}
