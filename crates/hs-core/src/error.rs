//! Why an operation failed.

use hs_pve::PveError;

/// Messages follow Python's `AppError` texts and may contain names from the
/// request; the server replaces them with fixed messages.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// Recovery context must retain its causes so API sanitization can still
    /// recognize Proxmox failures after lifecycle helpers add instructions.
    #[error("{message}")]
    Context { message: String, causes: Vec<CoreError> },
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
    /// The VM is outside the instance's pool (gate before phase 5).
    #[error("{0}")]
    OutsidePool(String),
    /// A step of an operation failed (Python's `AppError` of that step).
    #[error("{0}")]
    Failed(String),
    /// Another server instance holds the VM (phase 11).
    #[error("{0}")]
    Busy(String),
}

impl CoreError {
    /// A Proxmox cause, including errors wrapped with lifecycle recovery context.
    #[must_use]
    pub fn pve_error(&self) -> Option<&PveError> {
        match self {
            Self::Pve(error) => Some(error),
            Self::Context { causes, .. } => causes.iter().find_map(Self::pve_error),
            _ => None,
        }
    }
}
