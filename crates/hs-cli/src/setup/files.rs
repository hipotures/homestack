//! A desktop file or directory copied into the home (Python
//! `setup_files.py`).

use super::Settings;
use super::text::home_path;
use crate::AppError;

/// A configured path checked against the desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileItem {
    pub path: String,
    pub relative: String,
    /// `file` or `directory`.
    pub kind: String,
    pub is_directory: bool,
    pub local_path: String,
    /// `/home/<user>/RELATIVE`, with a slash for a directory.
    pub destination: String,
    /// `ready`, `missing` or `type mismatch`.
    pub status: String,
    pub detail: String,
}

/// Where `configured_path` comes from and goes to, and whether it can be
/// copied.
pub fn file_plan_item(settings: &Settings, configured_path: &str) -> Result<FileItem, AppError> {
    let (relative, is_directory) = home_path(configured_path)?;
    let source = settings.home.join(&relative);
    let metadata = std::fs::symlink_metadata(&source);
    let (status, detail) = match metadata {
        Ok(meta) if meta.file_type().is_symlink() => ("type mismatch", "symbolic links are not supported"),
        Err(_) => ("missing", "source does not exist"),
        Ok(meta) if is_directory && !meta.is_dir() => (
            "type mismatch",
            "configured with trailing '/', but source is not a directory",
        ),
        Ok(meta) if !is_directory && !meta.is_file() => (
            "type mismatch",
            "configured as a file, but source is not a regular file",
        ),
        Ok(_) => ("ready", ""),
    };
    Ok(FileItem {
        path: configured_path.to_owned(),
        destination: format!(
            "/home/{}/{relative}{}",
            settings.user,
            if is_directory { "/" } else { "" }
        ),
        relative,
        kind: if is_directory { "directory" } else { "file" }.to_owned(),
        is_directory,
        local_path: source.display().to_string(),
        status: status.to_owned(),
        detail: detail.to_owned(),
    })
}
