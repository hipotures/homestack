//! Why an operation failed.

use hs_pve::PveError;

/// Messages follow Python's `AppError` texts and may contain names from the
/// request; the server replaces them with fixed messages.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Pve(#[from] PveError),
    /// The requested VM or workspace does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The request itself is invalid.
    #[error("{0}")]
    Invalid(String),
    /// The request is valid but refers to the wrong kind of VM.
    #[error("{0}")]
    Refused(String),
    /// A workspace name matches more than one VM.
    #[error("{0}")]
    Ambiguous(String),
    /// Proxmox answered with data HomeStack cannot use.
    #[error("{0}")]
    Unusable(String),
}
