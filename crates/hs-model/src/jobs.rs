//! Plans, jobs and their event stream (mapping 8.4).
//!
//! A change to the cluster is planned first (`POST /v1/plans`), then applied
//! (`POST /v1/plans/{id}/apply`), which plans again, compares the digests and
//! starts a job. The job belongs to the server, not to the request: a client
//! follows it with `GET /v1/jobs/{id}` or the WebSocket
//! `GET /v1/jobs/{id}/events`. Times are RFC 3339 UTC with milliseconds.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

/// `POST /v1/plans`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanRequest {
    /// Name of the operation, e.g. `workspace.create`.
    pub operation: String,
    /// The operation's parameters; each operation documents its own.
    #[serde(default)]
    pub parameters: Value,
}

/// A plan: what an operation would do, computed from the cluster now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Plan {
    pub id: String,
    pub operation: String,
    /// SHA-256 (hex) of the operation, its parameters, targets and summary.
    /// Applying needs it, and the plan made again before execution must
    /// have the same one.
    pub digest: String,
    pub created_at: String,
    /// After this the plan cannot be applied.
    pub expires_at: String,
    /// VMIDs the operation changes; one job at a time per VM.
    pub targets: Vec<u32>,
    /// What will be done, as the operation describes it.
    pub summary: Value,
}

/// `POST /v1/plans/{id}/apply`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyRequest {
    /// The `digest` of the plan the client reviewed.
    pub digest: String,
    /// What the plan's `summary.secrets_required` names, and nothing else.
    /// Secrets are never part of a plan, a job or a log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<ApplySecrets>,
}

/// Secrets an operation needs to run (decision 6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplySecrets {
    /// The passphrase of the LUKS layer `workspace.resize` grows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub luks_passphrase: Option<Secret>,
}

impl ApplySecrets {
    /// The names of the secrets given.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.luks_passphrase.iter().map(|_| "luks_passphrase").collect()
    }
}

/// A secret string: printed as `<redacted>`, wiped from memory when
/// dropped.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub const fn new(value: String) -> Self {
        Self(value)
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
}

impl JobState {
    #[must_use]
    pub const fn finished(self) -> bool {
        match self {
            Self::Succeeded | Self::Failed => true,
            Self::Queued | Self::Running => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Running,
    Done,
    Failed,
}

/// A named step of a job, as Python's `start_step` / `finish_step`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct JobStep {
    pub name: String,
    pub state: StepState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Stopped,
}

/// A Proxmox task the job started or waited for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct JobTask {
    pub node: String,
    pub upid: String,
    /// Proxmox task type, e.g. `qmstart`.
    pub kind: String,
    pub vmid: Option<u32>,
    pub status: TaskStatus,
    /// `OK`, `WARNINGS: N` or the error, once stopped.
    pub exitstatus: Option<String>,
}

/// The latest transfer progress of a running task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct JobProgress {
    pub percent: f64,
    /// e.g. `5.8 / 16.0 GiB`
    pub detail: String,
}

/// Why a job failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct JobError {
    /// `operation_failed`, or an API error code such as
    /// `pve_permission_denied`.
    pub code: String,
    pub message: String,
}

/// `GET /v1/jobs/{id}`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Job {
    pub id: String,
    pub operation: String,
    pub plan_id: String,
    pub state: JobState,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub steps: Vec<JobStep>,
    pub tasks: Vec<JobTask>,
    pub progress: Option<JobProgress>,
    /// The last lines of the Proxmox task logs (at most 200).
    pub log: Vec<String>,
    /// What the operation returns on success.
    pub result: Option<Value>,
    pub error: Option<JobError>,
}

/// What an operation reports while it runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobEvent {
    StepStarted {
        name: String,
    },
    StepFinished {
        name: String,
        ok: bool,
    },
    TaskStarted {
        node: String,
        upid: String,
        kind: String,
        vmid: Option<u32>,
    },
    TaskLog {
        upid: String,
        lines: Vec<String>,
    },
    TaskFinished {
        upid: String,
        exitstatus: String,
    },
    Progress {
        percent: f64,
        detail: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StreamMessageType {
    /// The whole job, first on every connection.
    Snapshot,
    /// The whole job after a change; the last one has a finished state.
    Job,
    /// Sent at least every five seconds while nothing changes.
    Heartbeat,
}

/// One message of `GET /v1/jobs/{id}/events`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct StreamMessage {
    pub schema_version: u32,
    #[serde(rename = "type")]
    pub kind: StreamMessageType,
    /// Changes when the server restarts; streams are not replayed.
    pub server_instance_id: String,
    /// 1, 2, 3, ... per connection.
    pub stream_sequence: u64,
    pub server_time: String,
    /// The job; `null` for heartbeats.
    pub data: Option<Job>,
}
