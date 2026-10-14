//! The HomeStack client in Rust (phase 13, `docs/migration/phase-13.md`).
//!
//! The `homestack` command sends the Proxmox operations to the HomeStack
//! server (`hs-client`), asks before any change exactly as the Python CLI
//! does, and keeps the desktop's own effects: the SSH config of each
//! workspace, `known_hosts`, `identity_files` (I1) and the security-key
//! handling of the workspace SSH master (I2).

pub mod app;
pub mod args;
pub mod config;
pub mod fido;
pub mod identity;
pub mod installer;
pub mod local_ssh;
pub mod master;
pub mod picker;
pub mod root_herdr;
pub mod root_ssh;
pub mod system;
pub mod touch;
pub mod view;

/// A failure shown to the user as it is written (Python's `AppError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppError(pub String);

impl AppError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AppError {}

/// `Path.expanduser` for `~` and `~/...` against `home`.
#[must_use]
pub fn expand_home(path: &str, home: &std::path::Path) -> std::path::PathBuf {
    if path == "~" {
        return home.to_owned();
    }
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => std::path::PathBuf::from(path),
    }
}
