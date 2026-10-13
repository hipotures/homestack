//! Installing and removing HomeStack in Proxmox VE from the desktop
//! (phase 13c, `docs/migration/phase-13c.md`): what `deploy/pve/homestack-pve.sh`
//! did on a node, driven through a root shell reached once over SSH or a
//! Herdr pane.
//!
//! The crate does no I/O of its own: every command to the node goes through
//! [`root::Root`], and the only thing written on the desktop is the secret
//! file of a new token.

pub mod apply;
pub mod plan;
pub mod probe;
pub mod root;
pub mod settings;
pub mod shell;

mod checks;
mod snippets;
mod state;

pub use apply::{Applied, apply};
pub use plan::{Action, Plan, Step, make_plan};
pub use probe::{Access, Node, Topology, probe};
pub use root::{LocalShell, Output, Root};
pub use settings::{Command, Settings};

/// Why an install or uninstall did not go on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// One failure, shown as written.
    Failed(String),
    /// What belongs to the cluster's owner is not ready: nothing was
    /// changed.
    Checks { errors: Vec<String>, warnings: Vec<String> },
}

impl InstallError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(message) => f.write_str(message),
            Self::Checks { errors, warnings } => {
                for warning in warnings {
                    writeln!(f, "warning: {warning}")?;
                }
                for error in errors {
                    writeln!(f, "error: {error}")?;
                }
                f.write_str("nothing changed: the cluster's owner fixes the errors above first")
            }
        }
    }
}

impl std::error::Error for InstallError {}
