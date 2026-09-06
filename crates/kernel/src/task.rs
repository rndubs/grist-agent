//! Tasks (D1): handles returned by tools, updates delivered by wakers, and the record kept in `State`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::artifact::ArtifactHandle;
use crate::content::ToolResultContent;

/// Task identifier; deterministic per `(turn, tool_use_id)`: `t<turn>-<tool_use_id>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub String);

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Task lifecycle states (D1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Accepted, not yet running.
    Pending,
    /// Running.
    Running,
    /// Finished successfully.
    Succeeded,
    /// Finished with an error.
    Failed,
    /// Cancelled by the kernel or an operator.
    Cancelled,
}

impl TaskStatus {
    /// `Succeeded | Failed | Cancelled`.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }
}

/// What a tool returns to start a task. `check_hint` is for the polling sidecar (P3.4) and is
/// NEVER shown to the model: the kernel strips it before building the "task started" tool result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskHandle {
    /// Obtained from `ToolContext::task_id()`; deterministic per (turn, tool_use_id).
    pub id: TaskId,
    /// `Pending` or `Running` only; a terminal status here is a `ToolError::InvalidTaskHandle`.
    pub status: TaskStatus,
    /// Estimated time to completion.
    #[serde(
        default,
        with = "crate::serde_util::opt_duration_secs",
        rename = "eta_secs"
    )]
    pub eta: Option<Duration>,
    /// Opaque to the kernel. E.g. `{"slurm_job_id": "12345"}` or `{"pid": 4242}`.
    pub check_hint: Option<Value>,
    /// One line the model does see, e.g. "sbatch job 12345 (mesh_fine.slurm)".
    pub description: Option<String>,
}

/// Outcome carried by a terminal `TaskUpdate` and copied into the synthetic `TaskResult` block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskOutcome {
    /// Result content.
    pub content: ToolResultContent,
    /// Whether the task failed.
    pub is_error: bool,
    /// Artifacts the task produced.
    #[serde(default)]
    pub artifact_handles: Vec<ArtifactHandle>,
}

/// Who delivered a `TaskUpdate`. `trust_tier` is a placeholder until P3.4 (always `None` in P1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WakerSource {
    /// `"in_process_exit"` (P1), later `"slurm_epilog"`, `"file_watcher"`, `"cron"`, `"webhook"`, `"poll_sidecar"`, `"replay"`.
    pub kind: String,
    /// Trust tier (P3.4).
    pub trust_tier: Option<TrustTier>,
    /// Waker-specific detail, e.g. `{"pid": 4242}`.
    #[serde(default)]
    pub detail: Value,
}

impl WakerSource {
    /// The P1 in-kernel process-exit waker.
    pub fn in_process_exit(detail: Value) -> WakerSource {
        WakerSource {
            kind: "in_process_exit".to_owned(),
            trust_tier: None,
            detail,
        }
    }
}

/// Waker trust tiers, ordered: `Interactive` is most trusted (P3.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustTier {
    /// A human at the client.
    Interactive,
    /// A scheduled job.
    Scheduled,
    /// An inbound webhook.
    Inbound,
}

/// Delivered by a waker (P1: the in-kernel process-exit waker; P3.4: external wakers via the protocol).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskUpdate {
    /// The task.
    pub id: TaskId,
    /// New status.
    pub status: TaskStatus,
    /// Required when `status.is_terminal()`; MUST be `None` otherwise.
    pub outcome: Option<TaskOutcome>,
    /// Updated estimate.
    #[serde(
        default,
        with = "crate::serde_util::opt_duration_secs",
        rename = "eta_secs"
    )]
    pub eta: Option<Duration>,
    /// Updated polling hint.
    pub check_hint: Option<Value>,
    /// Who delivered it.
    pub source: WakerSource,
}

/// The record kept in `State.pending_tasks`. No wall-clock fields (it is hashed).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    /// The task.
    pub id: TaskId,
    /// The `ToolUse` that started it.
    pub tool_use_id: String,
    /// The tool that started it.
    pub tool_name: String,
    /// Current status.
    pub status: TaskStatus,
    /// Turn at which it started.
    pub started_turn: u64,
    /// Turn at which the synthetic result was appended; `None` while open.
    pub completed_turn: Option<u64>,
    /// Estimate.
    #[serde(
        default,
        with = "crate::serde_util::opt_duration_secs",
        rename = "eta_secs"
    )]
    pub eta: Option<Duration>,
    /// Polling hint (never model-visible).
    pub check_hint: Option<Value>,
    /// Model-visible description.
    pub description: Option<String>,
    /// Outcome once terminal.
    pub outcome: Option<TaskOutcome>,
    /// `true` when completion depends on a future registered in this process (`TaskRegistrar`).
    pub in_process_waker: bool,
}

impl Task {
    /// True while `Pending` or `Running`.
    pub fn is_open(&self) -> bool {
        !self.status.is_terminal()
    }
}
