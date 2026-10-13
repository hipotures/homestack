//! What the user chose for an install or uninstall (the options of
//! `homestack-pve.sh`).

use std::path::PathBuf;

/// What to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Install,
    Uninstall,
}

impl Command {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Uninstall => "uninstall",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// The test part (pool `homestack-test`, token `test`) instead of the
    /// production part (pool `homestack`, token `server`).
    pub test: bool,
    /// Storages of `[storage_layouts]` (content `images`).
    pub storages: Vec<String>,
    /// Storage of the vendor snippets (content `snippets`).
    pub snippet_storage: Option<String>,
    /// Storages of the server containers' root disks (content `rootdir`).
    pub service_storages: Vec<String>,
    /// Storage of the server image (content `vztmpl`).
    pub template_storage: Option<String>,
    /// Gold; the only VM tagged `homestack-gold` when not given.
    pub gold: Option<u32>,
    /// Uninstall: remove snippets that VMs still name.
    pub force: bool,
    /// Where the desktop writes the secret of a new token.
    pub token_dir: PathBuf,
}
