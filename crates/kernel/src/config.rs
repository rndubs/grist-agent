//! Plain-data configuration and outcome types from `kernel-interface.md` §3.15 that other
//! modules (events, middleware) reference. The `Kernel` itself lives in `kernel.rs` (P1.2).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::content::Message;
use crate::hash::Hash;
use crate::task::{TaskId, TaskUpdate};

/// Result spill configuration (D12, §7.4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpillConfig {
    /// From the profile (`spill_cap_bytes`); clamped by the kernel to 1 KiB ..= 1 MiB so a profile can
    /// tune but never disable spill (D12). Default 16 KiB.
    pub cap_bytes: u64,
    /// Default 2048.
    pub head_bytes: u64,
    /// Default 2048.
    pub tail_bytes: u64,
}

impl SpillConfig {
    /// Smallest permitted `cap_bytes` (1 KiB).
    pub const MIN_CAP_BYTES: u64 = 1024;
    /// Largest permitted `cap_bytes` (1 MiB).
    pub const MAX_CAP_BYTES: u64 = 1024 * 1024;

    /// A copy with `cap_bytes` clamped into `[MIN_CAP_BYTES, MAX_CAP_BYTES]`; `true` if it changed.
    pub fn clamped(&self) -> (SpillConfig, bool) {
        let cap = self
            .cap_bytes
            .clamp(Self::MIN_CAP_BYTES, Self::MAX_CAP_BYTES);
        (
            SpillConfig {
                cap_bytes: cap,
                ..self.clone()
            },
            cap != self.cap_bytes,
        )
    }
}

impl Default for SpillConfig {
    fn default() -> Self {
        SpillConfig {
            cap_bytes: 16 * 1024,
            head_bytes: 2048,
            tail_bytes: 2048,
        }
    }
}

/// Provider retry policy (D15, §7.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Total attempts including the first. Default 5.
    pub max_attempts: u32,
    /// Default 500 ms.
    #[serde(with = "crate::serde_util::duration_ms", rename = "base_delay_ms")]
    pub base_delay: Duration,
    /// Default 30 s.
    #[serde(with = "crate::serde_util::duration_ms", rename = "max_delay_ms")]
    pub max_delay: Duration,
    /// Default 2.0.
    pub multiplier: f64,
    /// Full jitter: `delay = rand(0, min(max_delay, base * multiplier^(attempt-1)))`. Default true.
    pub jitter: bool,
    /// Per-attempt request timeout handed to the provider. Default 300 s.
    #[serde(
        with = "crate::serde_util::duration_secs",
        rename = "request_timeout_secs"
    )]
    pub request_timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            multiplier: 2.0,
            jitter: true,
            request_timeout: Duration::from_secs(300),
        }
    }
}

/// Why a kernel is being resumed in a new process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum ResumeCause {
    /// A waker delivered a task update.
    TaskUpdate(TaskUpdate),
    /// The user sent a message.
    UserMessage(Message),
    /// Operator/UI asked to resume a `Failed` or `Suspended` session with nothing new to deliver.
    Operator,
    /// Crash recovery (§7.3): the log did not end cleanly.
    Recovery,
}

/// What a suspension is waiting on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Suspension {
    /// Open tasks.
    pub pending_task_ids: Vec<TaskId>,
    /// Tasks whose waker lives in this process. If > 0 the launcher SHOULD keep the process alive
    /// (P1 `run_script`); tasks with external wakers (P3.4) let the process exit.
    pub in_process_wakers: usize,
    /// The checkpoint to resume from.
    pub checkpoint_hash: Hash,
}

/// Outcome of one `run_turn`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TurnOutcome {
    /// Turn had tool calls (or queued input was drained); the session is still `Running`.
    Continue,
    /// Waiting for the user.
    Idle,
    /// Waiting for a waker.
    Suspended(Suspension),
    /// The turn failed; the session is `Failed`.
    Failed {
        /// `turn_failed.error_class`.
        error_class: String,
    },
    /// The turn was cancelled; the session is `Idle`.
    Cancelled,
}

/// Why `Kernel::run` returned.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stopped", rename_all = "snake_case")]
pub enum RunStop {
    /// Waiting for the user.
    Idle,
    /// Waiting for a waker.
    Suspended(Suspension),
    /// Explicitly ended.
    Done,
    /// Failed; resumable from the checkpoint.
    Failed {
        /// `turn_failed.error_class`.
        error_class: String,
    },
}
