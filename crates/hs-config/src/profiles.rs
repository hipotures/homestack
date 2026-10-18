//! Names and private storage shared by the client and the E2E controller.

use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const DEFAULT: &str = "default";
pub const E2E: &str = "e2e";

/// Use the desktop's running Herdr session while HomeStack keeps its own profile.
pub fn herdr_wrapper(executable: &Path, home: &Path, config_home: &Path) -> String {
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"));
    format!(
        "#!/bin/sh\nexport HOME={}\nexport XDG_CONFIG_HOME={}\nexec {} \"$@\"\n",
        quote(home),
        quote(config_home),
        quote(executable)
    )
}

/// Capture the desktop command before a launcher changes HOME and PATH.
pub fn desktop_herdr_wrapper() -> Option<String> {
    let executable = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|path| path.join("herdr"))
        .find(|path| {
            path.metadata()
                .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        })?;
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let config_home = std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
    Some(herdr_wrapper(&executable, &home, &config_home))
}

pub fn validate(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        || !name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-".contains(&c))
    {
        return Err(io::Error::other(
            "Profile names must start with a lowercase letter or digit and contain only a-z, 0-9, _ or - (maximum 64 characters)",
        ));
    }
    Ok(())
}

pub fn base(home: &Path, config_home: Option<&Path>) -> PathBuf {
    config_home
        .map_or_else(|| home.join(".config"), Path::to_owned)
        .join("homestack")
}

pub fn directory(base: &Path, name: &str) -> io::Result<PathBuf> {
    validate(name)?;
    let parent = base.join("profiles");
    let path = parent.join(name);
    if parent.is_symlink() || path.is_symlink() {
        return Err(io::Error::other("Profile directories must not be symlinks"));
    }
    Ok(path)
}

pub fn environment_base() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    Ok(base(Path::new(&home), config_home.as_deref()))
}

/// A profile directory cannot alias another profile through a symlink.
pub fn private_directory(path: &Path) -> io::Result<()> {
    if path.is_symlink() {
        return Err(io::Error::other(format!(
            "Profile directory {} must not be a symlink",
            path.display()
        )));
    }
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

pub fn write_private(path: &Path, content: &[u8]) -> io::Result<()> {
    if path.is_symlink() {
        return Err(io::Error::other(format!(
            "Profile file {} must not be a symlink",
            path.display()
        )));
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(content)
}

/// Only a run reference persists here; all secrets stay in the disposable VM directory.
#[derive(Debug, PartialEq, Eq)]
pub struct E2eRun {
    pub artifacts: PathBuf,
    pub run_id: String,
}

impl E2eRun {
    pub fn read(profile: &Path) -> io::Result<Self> {
        let path = profile.join("run.toml");
        if path.is_symlink() {
            return Err(io::Error::other("E2E run reference must not be a symlink"));
        }
        let text = std::fs::read_to_string(&path).map_err(|error| io::Error::other(format!(
            "Cannot open profile e2e at {}: {error}. Start the lab with --pause-after verify or register an existing paused run with homestack-e2e profile --artifacts DIRECTORY",
            path.display()
        )))?;
        let value: toml::Table = text.parse().map_err(io::Error::other)?;
        let field = |name: &str| -> io::Result<String> {
            value
                .get(name)
                .and_then(toml::Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| io::Error::other(format!("E2E profile has no {name}")))
        };
        let artifacts = PathBuf::from(field("artifacts")?);
        if !artifacts.is_absolute() {
            return Err(io::Error::other("E2E artifact path must be absolute"));
        }
        Ok(Self {
            artifacts,
            run_id: field("run_id")?,
        })
    }

    pub fn save(&self, profile: &Path) -> io::Result<()> {
        let mut value = toml::Table::new();
        value.insert("artifacts".into(), self.artifacts.display().to_string().into());
        value.insert("run_id".into(), self.run_id.clone().into());
        // Renaming a private temporary file keeps readers from seeing partial references.
        let temporary = profile.join(format!(".run-{}.toml", std::process::id()));
        write_private(
            &temporary,
            toml::to_string(&value).map_err(io::Error::other)?.as_bytes(),
        )?;
        let result = std::fs::rename(&temporary, profile.join("run.toml"));
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        result
    }
}
