//! Task updates and task cancellation (§4.1, §4.2).

use serde_json::Value;

use super::{Kernel, KernelError};
use crate::content::{ContentBlock, Message, Role, ToolResultContent};
use crate::event::{
    AppliedAt, AppliedPoint, CancelScopeKind, CancelledPayload, EventBody, LoopPhase,
    TaskUpdatePayload, WarningPayload,
};
use crate::task::{TaskId, TaskOutcome, TaskStatus, TaskUpdate};

/// What applying a `TaskUpdate` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Applied {
    /// Ignored (terminal / unknown / missing outcome); a `warning` was logged.
    Ignored,
    /// Status/eta/check_hint updated; no message appended.
    Progress,
    /// The task reached a terminal status and a synthetic `TaskResult` was appended.
    Terminal,
}

impl Kernel {
    /// §4.1: apply one update, logging `task_update` (or a `warning` plus an ignored `task_update`).
    pub(super) async fn apply_task_update(
        &mut self,
        mut u: TaskUpdate,
        at: AppliedPoint,
    ) -> Result<Applied, KernelError> {
        self.sh
            .redact(&mut u)
            .map_err(|e| KernelError::Internal(format!("redaction: {e}")))?;
        let turn = self.state.turn;
        let from_status = self.state.pending_tasks.get(&u.id).map(|t| t.status);
        let ignored: Option<(&str, &str)> = match from_status {
            None => Some(("task_update_unknown", "unknown_task")),
            Some(s) if s.is_terminal() => Some(("task_update_ignored", "terminal")),
            Some(_) if u.status.is_terminal() && u.outcome.is_none() => {
                Some(("task_update_invalid", "missing_outcome"))
            }
            Some(_) => None,
        };
        if let Some((class, reason)) = ignored {
            self.log(EventBody::Warning(WarningPayload::kernel(
                turn,
                class,
                format!("task update for {} ignored: {reason}", u.id),
                Some(serde_json::json!({ "task_id": u.id, "status": u.status })),
            )))
            .await?;
            self.log(EventBody::TaskUpdate(TaskUpdatePayload {
                turn,
                task_id: u.id.clone(),
                from_status,
                status: u.status,
                outcome: u.outcome.clone(),
                eta_secs: u.eta.map(|d| d.as_secs()),
                check_hint: u.check_hint.clone(),
                waker: u.source.clone(),
                applied: AppliedAt {
                    turn,
                    at: AppliedPoint::Ignored,
                },
                ignored_reason: Some(reason.to_owned()),
            }))
            .await?;
            return Ok(Applied::Ignored);
        }
        let applied = {
            let task = self
                .state
                .pending_tasks
                .get_mut(&u.id)
                .ok_or_else(|| KernelError::Internal("task vanished".to_owned()))?;
            if u.eta.is_some() {
                task.eta = u.eta;
            }
            if u.check_hint.is_some() {
                task.check_hint = u.check_hint.clone();
            }
            match u.status {
                TaskStatus::Pending => Applied::Progress,
                TaskStatus::Running => {
                    task.status = TaskStatus::Running;
                    Applied::Progress
                }
                TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled => {
                    let outcome = u
                        .outcome
                        .clone()
                        .ok_or_else(|| KernelError::Internal("outcome checked above".to_owned()))?;
                    task.status = u.status;
                    task.completed_turn = Some(turn);
                    let is_error = outcome.is_error || u.status != TaskStatus::Succeeded;
                    task.outcome = Some(outcome.clone());
                    let block = ContentBlock::TaskResult {
                        task_id: task.id.clone(),
                        tool_use_id: task.tool_use_id.clone(),
                        status: u.status,
                        content: outcome.content,
                        is_error,
                    };
                    self.state.messages.push(Message {
                        role: Role::User,
                        content: vec![block],
                    });
                    self.sh.registrar.abort(&u.id);
                    Applied::Terminal
                }
            }
        };
        self.log(EventBody::TaskUpdate(TaskUpdatePayload {
            turn,
            task_id: u.id.clone(),
            from_status,
            status: u.status,
            outcome: u.outcome,
            eta_secs: u.eta.map(|d| d.as_secs()),
            check_hint: u.check_hint,
            waker: u.source,
            applied: AppliedAt { turn, at },
            ignored_reason: None,
        }))
        .await?;
        Ok(applied)
    }

    /// The terminal `Cancelled` transition of §4.1 in `State` only (no event): drops the in-process
    /// future, sets the outcome, appends the synthetic `TaskResult`. Returns `false` if the task is
    /// not open.
    pub(super) fn cancel_task_in_state(&mut self, id: &TaskId, content: Value) -> bool {
        self.sh.registrar.abort(id);
        let turn = self.state.turn;
        let Some(task) = self.state.pending_tasks.get_mut(id) else {
            return false;
        };
        if !task.is_open() {
            return false;
        }
        let outcome = TaskOutcome {
            content: ToolResultContent::Json(content),
            is_error: true,
            artifact_handles: Vec::new(),
        };
        task.status = TaskStatus::Cancelled;
        task.completed_turn = Some(turn);
        task.outcome = Some(outcome.clone());
        let block = ContentBlock::TaskResult {
            task_id: task.id.clone(),
            tool_use_id: task.tool_use_id.clone(),
            status: TaskStatus::Cancelled,
            content: outcome.content,
            is_error: true,
        };
        self.state.messages.push(Message {
            role: Role::User,
            content: vec![block],
        });
        true
    }

    /// `KernelHandle::cancel(CancelScope::Task)` as applied by the loop: the §4.1 transition plus a
    /// `cancelled` event. A cancel that finds no open task is a no-op and logs nothing (§7.1).
    pub(super) async fn apply_cancel_task(
        &mut self,
        id: &TaskId,
        phase: LoopPhase,
    ) -> Result<bool, KernelError> {
        if !self.cancel_task_in_state(id, serde_json::json!({ "cancelled": true })) {
            return Ok(false);
        }
        self.log(EventBody::Cancelled(CancelledPayload {
            turn: self.state.turn,
            scope: CancelScopeKind::Task,
            tool_use_id: None,
            task_id: Some(id.clone()),
            phase,
            signalled: false,
            skipped_tool_use_ids: Vec::new(),
        }))
        .await?;
        Ok(true)
    }
}
