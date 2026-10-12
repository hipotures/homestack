//! The stored `server.toml`.
//!
//! The server owns the file. It reads it once at start (an invalid file stops
//! the start) and replaces it only through [`ConfigStore::replace`], with the
//! guarantees of Python's `config.publish_config`:
//!
//! - the new text is validated before anything is written,
//! - it goes to a private (0600) temporary file in the same directory, which
//!   is synced and then renamed over the target, and the directory is synced,
//! - the previous file is first copied to `server.toml.backup-<hash>`
//!   (0600; content-addressed instead of Python's timestamp, so uploading the
//!   same text twice keeps one copy),
//! - on any failure the original stays and the temporary file is removed,
//! - a missing directory is created with mode 0700.
//!
//! Replacing is blocking file I/O; callers run it on a blocking thread.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use hs_model::config::{Config, ConfigError, config_version};

/// A validated configuration and its version.
#[derive(Debug)]
pub struct Stored {
    pub text: String,
    /// SHA-256 of `text`.
    pub version: String,
    pub config: Config,
}

/// What `PUT /v1/config` requires of the current configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    /// `If-Match`: a configuration exists, with this version if given
    /// (`None` for `*`).
    Exists(Option<String>),
    /// `If-None-Match: *`: no configuration exists yet.
    Absent,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("cannot read the configuration {path}: {source}")]
    Read { path: String, source: io::Error },
    #[error("invalid configuration {path}: {}: {}", error.field, error.message)]
    Invalid { path: String, error: ConfigError },
}

#[derive(Debug, thiserror::Error)]
pub enum ReplaceError {
    #[error("the precondition does not hold")]
    Precondition,
    #[error("{0}")]
    Invalid(ConfigError),
    #[error("cannot write the configuration: {0}")]
    Io(#[from] io::Error),
}

/// Outcome of a successful replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replaced {
    pub version: String,
    /// `false` when the text was already stored.
    pub changed: bool,
}

type Rename = fn(&Path, &Path) -> io::Result<()>;

pub struct ConfigStore {
    path: PathBuf,
    current: RwLock<Option<Arc<Stored>>>,
    /// Serializes replaces.
    writer: Mutex<()>,
    /// `std::fs::rename`; tests make it fail.
    rename: Rename,
}

impl ConfigStore {
    /// Load `path` if it exists. A file that does not validate is an error.
    pub fn open(path: PathBuf) -> Result<Self, StoreError> {
        let current = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let config = Config::parse(&text).map_err(|error| StoreError::Invalid {
                    path: path.display().to_string(),
                    error,
                })?;
                warn_if_readable_by_others(&path);
                Some(Arc::new(Stored {
                    version: config_version(&text),
                    text,
                    config,
                }))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(StoreError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        Ok(Self {
            path,
            current: RwLock::new(current),
            writer: Mutex::new(()),
            rename: |from, to| std::fs::rename(from, to),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn current(&self) -> Option<Arc<Stored>> {
        self.current.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Check `precondition`, validate `text` and store it.
    pub fn replace(&self, text: String, precondition: &Precondition) -> Result<Replaced, ReplaceError> {
        let _writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let current = self.current();
        let holds = match (precondition, &current) {
            (Precondition::Absent, current) => current.is_none(),
            (Precondition::Exists(None), current) => current.is_some(),
            (Precondition::Exists(Some(version)), Some(current)) => *version == current.version,
            (Precondition::Exists(Some(_)), None) => false,
        };
        if !holds {
            return Err(ReplaceError::Precondition);
        }
        let config = Config::parse(&text).map_err(ReplaceError::Invalid)?;
        let version = config_version(&text);
        if current.as_ref().is_some_and(|current| current.version == version) {
            return Ok(Replaced {
                version,
                changed: false,
            });
        }
        self.write(&text)?;
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(Stored {
            text,
            version: version.clone(),
            config,
        }));
        Ok(Replaced { version, changed: true })
    }

    fn write(&self, text: &str) -> io::Result<()> {
        let dir = match self.path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.to_owned(),
            _ => PathBuf::from("."),
        };
        let name = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the configuration path has no file name"))?;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;

        let mut temporary = Temporary(Some(dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()))));
        let temporary_path = temporary.0.clone().unwrap_or_default();
        write_private(&temporary_path, text.as_bytes())?;

        match std::fs::read(&self.path) {
            Ok(old) => {
                let hash = config_version(&String::from_utf8_lossy(&old));
                let backup = dir.join(format!("{name}.backup-{}", &hash[..16]));
                match write_private(&backup, &old) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        (self.rename)(&temporary_path, &self.path)?;
        temporary.0 = None;
        File::open(&dir)?.sync_all()
    }
}

/// Create `path` (it must not exist) with mode 0600 and sync it.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// A temporary file, removed on drop unless the path was taken out.
struct Temporary(Option<PathBuf>);

impl Drop for Temporary {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn warn_if_readable_by_others(path: &Path) {
    if let Ok(metadata) = std::fs::metadata(path)
        && metadata.permissions().mode() & 0o077 != 0
    {
        tracing::warn!(path = %path.display(), "configuration file is accessible to other users; use mode 0600");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER_TOML: &str = "version = 1\n[node]\nname = \"pve1\"\ngold_vmid = 100\n[storage_layouts]\nhomestack-storage-1 = [\"local\"]\n[root]\ndisk = \"scsi0\"\n[home]\ndisk = \"scsi1\"\ndefault_size = \"20G\"\n[network]\nprefix = \"192.0.2\"\ncidr = 24\ngateway = \"192.0.2.1\"\ndns = [\"192.0.2.53\"]\n[cloud_init]\nsnippet_storage = \"local\"\nsnippet_dir = \"/var/lib/vz/snippets\"\n[user]\nname = \"user\"\nuid = 1000\ngid = 1000\n";

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Port of `test_config.py::ConfigTests::test_atomic_replace_failure_leaves_original_intact`.
    #[test]
    fn a_failed_rename_leaves_the_original_intact() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("server.toml");
        let mut store = ConfigStore::open(path.clone())?;
        let first = store.replace(SERVER_TOML.to_owned(), &Precondition::Absent)?;

        store.rename = |_, _| Err(io::Error::other("replace failed"));
        let newer = SERVER_TOML.replace("gold_vmid = 100", "gold_vmid = 102");
        let result = store.replace(newer, &Precondition::Exists(Some(first.version.clone())));
        assert!(matches!(result, Err(ReplaceError::Io(ref error)) if error.to_string() == "replace failed"));

        assert_eq!(std::fs::read_to_string(&path)?, SERVER_TOML);
        assert_eq!(store.current().map(|c| c.version.clone()), Some(first.version));
        let temporary: Vec<_> = std::fs::read_dir(dir.path())?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(temporary.is_empty(), "{temporary:?}");
        Ok(())
    }
}
