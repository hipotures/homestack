//! Crash integrity for TLS: publish complete immutable generations, then select
//! one with a single rename. Caddy uses pinned paths into that generation.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use crate::secrets::TlsPair;
use hs_model::jobs::Secret;

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid private TLS state")
}

fn sync(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

fn private_directory(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => return Err(invalid()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            step("create-private-directory", || {
                fs::DirBuilder::new().mode(0o700).create(dir)
            })?;
            step("sync-private-parent", || sync(dir.parent().ok_or_else(invalid)?))?;
        }
        Err(error) => return Err(error),
    }
    step("private-directory-mode", || {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
    })?;
    step("sync-private-directory", || sync(dir))
}

fn read_private(path: &Path) -> io::Result<String> {
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )?;
    let mut file = File::from(fd);
    let meta = file.metadata()?;
    if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 || meta.nlink() != 1 || meta.len() > 64 * 1024 {
        return Err(invalid());
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

fn read_pair(dir: &Path) -> io::Result<TlsPair> {
    TlsPair::checked(
        read_private(&dir.join("cert.pem"))?,
        Secret::new(read_private(&dir.join("key.pem"))?),
    )
    .map_err(|_| invalid())
}

fn read_generation(path: &Path) -> io::Result<TlsPair> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() || meta.permissions().mode() & 0o077 != 0 {
        return Err(invalid());
    }
    read_pair(path)
}

#[derive(Clone)]
pub(crate) struct Generation {
    dir: PathBuf,
    name: String,
}
impl Generation {
    pub(crate) fn path(&self, file: &str) -> PathBuf {
        self.dir.join(&self.name).join(file)
    }
}

fn generation_name(name: &str) -> bool {
    name.strip_prefix("generation-")
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        .is_some_and(|id| name == format!("generation-{id}"))
}

fn selected(dir: &Path) -> io::Result<Option<Generation>> {
    let current = dir.join("current");
    match fs::symlink_metadata(&current) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
        Ok(meta) if !meta.file_type().is_symlink() => return Err(invalid()),
        Ok(_) => {}
    }
    let target = fs::read_link(current)?;
    let name = target
        .to_str()
        .filter(|name| generation_name(name))
        .ok_or_else(invalid)?;
    let generation = Generation {
        dir: dir.to_owned(),
        name: name.to_owned(),
    };
    read_generation(&dir.join(name))?;
    Ok(Some(generation))
}

/// Fault/termination hooks run only in the focused filesystem safeguard.
fn step<T>(name: &str, work: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    #[cfg(not(test))]
    let _ = name;
    #[cfg(test)]
    tests::before(name)?;
    let result = work()?;
    #[cfg(test)]
    tests::after(name);
    Ok(result)
}

fn stage(dir: &Path, pair: &TlsPair) -> io::Result<Generation> {
    pair.validate().map_err(|_| invalid())?;
    let id = uuid::Uuid::new_v4();
    let staging = dir.join(format!(".staging-{id}"));
    step("create-generation", || {
        fs::DirBuilder::new().mode(0o700).create(&staging)
    })?;
    for (name, bytes) in [
        ("key.pem", pair.key.expose().as_bytes()),
        ("cert.pem", pair.cert.as_bytes()),
    ] {
        let mut file = step(name, || {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(staging.join(name))
        })?;
        step("write-file", || file.write_all(bytes))?;
        step("sync-file", || file.sync_all())?;
    }
    step("sync-generation", || sync(&staging))?;
    let name = format!("generation-{id}");
    step("publish-generation", || fs::rename(&staging, dir.join(&name)))?;
    step("sync-published-generation", || sync(dir))?;
    Ok(Generation {
        dir: dir.to_owned(),
        name,
    })
}

/// The only publication that changes the active pair. Even a crash before the
/// directory fsync can recover only the old or new selector, both complete.
pub(crate) fn select(dir: &Path, generation: Option<&Generation>) -> io::Result<()> {
    if let Some(generation) = generation {
        if generation.dir != dir || !generation_name(&generation.name) {
            return Err(invalid());
        }
        read_generation(&dir.join(&generation.name))?;
        let temporary = dir.join(format!(".current-{}", uuid::Uuid::new_v4()));
        step("stage-selector", || symlink(&generation.name, &temporary))?;
        step("select-generation", || fs::rename(&temporary, dir.join("current")))?;
    } else {
        step("remove-selector", || match fs::remove_file(dir.join("current")) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        })?;
    }
    step("sync-selector", || sync(dir))
}

pub(crate) struct Rotation {
    pub(crate) previous: Option<Generation>,
    pub(crate) active: Generation,
}

pub(crate) fn rotate(dir: &Path, pair: &TlsPair) -> io::Result<Rotation> {
    private_directory(dir)?;
    let previous = selected(dir)?;
    let active = stage(dir, pair)?;
    if let Err(error) = select(dir, Some(&active)) {
        // The selector rename may have succeeded before its sync failed. Keep
        // both generations and durably restore the old selection if possible.
        if let Err(restore) = select(dir, previous.as_ref()) {
            tracing::error!(kind = ?restore.kind(), "could not restore the previous TLS selection");
        }
        return Err(error);
    }
    Ok(Rotation { previous, active })
}

/// Runs before the server listens. Unselected staging/generation directories
/// are never inferred to be active; interruption cannot mix their files.
pub(crate) fn bootstrap(state_dir: &Path, pair: Option<&TlsPair>) -> io::Result<()> {
    private_directory(state_dir)?;
    let dir = state_dir.join("tls");
    private_directory(&dir)?;
    if selected(&dir)?.is_some() {
        return Ok(());
    }
    let exists = |name: &str| match fs::symlink_metadata(dir.join(name)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    };
    if exists("cert.pem")? || exists("key.pem")? {
        // Existing installations keep the exact stored pair, never bootstrap
        // input in its place. A partial, mismatched or unsafe legacy pair
        // fails closed. Leave both originals untouched so interruption before
        // durable selection can always restart this publication from them.
        let legacy = read_pair(&dir)?;
        rotate(&dir, &legacy)?;
        return Ok(());
    }
    if let Some(pair) = pair {
        rotate(&dir, pair)?;
    } else if fs::read_dir(&dir)?.next().is_some() {
        // No selector can authorize an orphan generation, even when its two
        // files happen to be complete. Keep it for explicit recovery.
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
