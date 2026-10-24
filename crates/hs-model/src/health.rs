//! Process health and monotonic serving-path heartbeat observations.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Body of the health endpoints. Remote dependencies are not liveness checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Liveness {
    pub status: LivenessStatus,
    pub checks: BTreeMap<String, Vec<HeartbeatObservation>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LivenessStatus {
    Pass,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HeartbeatObservation {
    pub status: LivenessStatus,
    #[serde(rename = "observedValue")]
    pub observed_value: u64,
    #[serde(rename = "observedUnit", skip_serializing_if = "Option::is_none")]
    pub observed_unit: Option<String>,
}
