//! Liveness probe document.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Body of `GET /health/live`. It only says the HTTP server answers; it does
/// not check Proxmox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Liveness {
    pub status: LivenessStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LivenessStatus {
    Ok,
}

impl Liveness {
    pub const OK: Self = Self {
        status: LivenessStatus::Ok,
    };
}
