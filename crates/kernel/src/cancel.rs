//! Cancellation (§3.13, D15).

use serde::{Deserialize, Serialize};

pub use tokio_util::sync::CancellationToken;

use crate::task::TaskId;

/// What a cancel request targets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum CancelScope {
    /// The current turn: in-flight model call or tool; remaining tool calls are not started.
    Turn,
    /// One in-flight tool invocation; the turn continues with the next call.
    Tool {
        /// The invocation to cancel.
        tool_use_id: String,
    },
    /// An open task (D1); its waker's future is dropped and the task becomes `Cancelled`.
    Task {
        /// The task to cancel.
        task_id: TaskId,
    },
}
