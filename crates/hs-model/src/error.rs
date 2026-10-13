//! Error document returned by every failing API request.
//!
//! The shape follows mypowers: a schema version, a stable machine-readable
//! code, a fixed human-readable message, whether retrying can help, and the
//! request ID. Messages never echo request input.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Version of the error document schema.
pub const SCHEMA_VERSION: u32 = 1;

/// Stable error codes of the HomeStack API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Missing or invalid bearer token.
    Unauthorized,
    /// No such resource or route.
    NotFound,
    /// The route exists but not for this method.
    MethodNotAllowed,
    /// A path or query parameter failed validation.
    ValidationFailed,
    /// The request body is larger than the server accepts.
    PayloadTooLarge,
    /// The request needs a precondition header (`If-Match`,
    /// `If-None-Match` or `HomeStack-Config-Version`) it did not send.
    PreconditionRequired,
    /// `If-Match` or `If-None-Match` does not hold for the stored
    /// configuration.
    PreconditionFailed,
    /// The uploaded configuration is invalid; `field` says where.
    ConfigInvalid,
    /// The server has no configuration yet.
    ConfigMissing,
    /// The request expects another configuration version than the server
    /// has (`HomeStack-Config-Version`).
    ConfigVersionMismatch,
    /// No configured Proxmox API endpoint could be reached.
    PveUnavailable,
    /// Proxmox rejected the server's API token.
    PveAuthFailed,
    /// The server's API token lacks a privilege for this request.
    PvePermissionDenied,
    /// Proxmox could not reach the node the request was addressed to.
    PveNodeUnavailable,
    /// Proxmox returned an error or a response the server could not use.
    PveError,
    /// The plan was made from a cluster state that has changed since, or
    /// the request names another digest; make a new plan.
    PlanChanged,
    /// The plan has already been applied; make a new plan.
    PlanApplied,
    /// Applying the plan needs a secret the request did not send (the
    /// plan's `summary.secrets_required`), or sent one the plan does not
    /// need.
    SecretRequired,
    /// Another job is working on one of the plan's VMs.
    Busy,
    /// The VM is not in the instance's pool; the server changes only the
    /// VMs of its pool.
    OutsidePool,
    /// The operation is valid but the cluster's state does not allow it
    /// (e.g. the VMID is taken, the VM is not a workspace).
    OperationRefused,
    /// An unexpected server-side failure.
    Internal,
}

impl ErrorCode {
    /// Whether the same request may succeed later without any change.
    #[must_use]
    pub const fn retryable(self) -> bool {
        match self {
            Self::PveUnavailable | Self::PveNodeUnavailable | Self::Busy => true,
            Self::Unauthorized
            | Self::NotFound
            | Self::MethodNotAllowed
            | Self::ValidationFailed
            | Self::PayloadTooLarge
            | Self::PreconditionRequired
            | Self::PreconditionFailed
            | Self::ConfigInvalid
            | Self::ConfigMissing
            | Self::ConfigVersionMismatch
            | Self::PveAuthFailed
            | Self::PvePermissionDenied
            | Self::PveError
            | Self::PlanChanged
            | Self::OutsidePool
            | Self::OperationRefused
            | Self::PlanApplied
            | Self::SecretRequired
            | Self::Internal => false,
        }
    }
}

/// Body of every error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ErrorBody {
    pub schema_version: u32,
    pub error: ErrorDetail,
}

/// The error itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ErrorDetail {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    pub request_id: String,
    /// The configuration field a `config_invalid` error is about, e.g.
    /// `repo.owner`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

impl ErrorBody {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>, request_id: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            error: ErrorDetail {
                code,
                message: message.into(),
                retryable: code.retryable(),
                request_id: request_id.into(),
                field: None,
            },
        }
    }

    /// Name the configuration field the error is about.
    #[must_use]
    pub fn with_field(mut self, field: Option<String>) -> Self {
        self.error.field = field;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_body_matches_the_documented_shape() -> Result<(), serde_json::Error> {
        let body = ErrorBody::new(ErrorCode::PveUnavailable, "No Proxmox endpoint is reachable.", "rid");
        let json = serde_json::to_value(&body)?;
        assert_eq!(
            json,
            serde_json::json!({
                "schema_version": 1,
                "error": {
                    "code": "pve_unavailable",
                    "message": "No Proxmox endpoint is reachable.",
                    "retryable": true,
                    "request_id": "rid"
                }
            })
        );
        Ok(())
    }

    #[test]
    fn only_availability_errors_are_retryable() {
        assert!(ErrorCode::PveUnavailable.retryable());
        assert!(ErrorCode::PveNodeUnavailable.retryable());
        assert!(!ErrorCode::PveAuthFailed.retryable());
        assert!(!ErrorCode::Unauthorized.retryable());
    }
}
