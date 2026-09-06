//! Kernel-internal plumbing: the inbox, cancellation state, the in-process task waker
//! (`TaskRegistrar`), the post-write event broadcaster, and the extension event sink.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use futures_core::future::BoxFuture;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use super::KernelHandle;
use crate::cancel::CancellationToken;
use crate::content::Message;
use crate::event::{Event, EventBody, WarningPayload};
use crate::log::{EventLog, LogError};
use crate::middleware::{ExtensionEvent, ExtensionEventSink};
use crate::task::{TaskId, TaskOutcome, TaskStatus, TaskUpdate, WakerSource};
use crate::tool::{TaskRegistrar, ToolError};

/// Lock a mutex, recovering from poisoning (the guarded data is always left consistent).
pub(super) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// What the handle sends to the kernel.
pub(super) enum Inbound {
    /// `KernelHandle::enqueue_user_message`.
    UserMessage(Message),
    /// `KernelHandle::deliver_task_update`.
    TaskUpdate(TaskUpdate),
    /// `KernelHandle::cancel(CancelScope::Task)`: the terminal transition of §4.1.
    CancelTask(TaskId),
}

/// The three token levels of §3.13, shared between the kernel and its handles.
pub(super) struct CancelState {
    /// The session token; `Kernel::end` cancels it.
    pub session: CancellationToken,
    /// The current turn's child token, `Some` only while a turn runs.
    pub turn: Option<CancellationToken>,
    /// Per-invocation child tokens keyed by `tool_use_id`, present only while the tool runs.
    pub tools: HashMap<String, CancellationToken>,
}

impl CancelState {
    pub(super) fn new() -> CancelState {
        CancelState {
            session: CancellationToken::new(),
            turn: None,
            tools: HashMap::new(),
        }
    }
}

/// Live in-process waker futures keyed by task id.
pub(super) type WakerMap = Arc<Mutex<HashMap<TaskId, JoinHandle<()>>>>;

/// The P1 in-process waker: `watch` spawns the completion future on tokio and, when it resolves,
/// delivers a `TaskUpdate{source.kind: "in_process_exit"}` through the handle (§3.6).
pub(super) struct Registrar {
    handle: Mutex<Option<KernelHandle>>,
    wakers: WakerMap,
    watched: Mutex<HashSet<TaskId>>,
}

impl Registrar {
    pub(super) fn new(wakers: WakerMap) -> Registrar {
        Registrar {
            handle: Mutex::new(None),
            wakers,
            watched: Mutex::new(HashSet::new()),
        }
    }

    /// The handle is created after the registrar (they reference each other); set once.
    pub(super) fn set_handle(&self, handle: KernelHandle) {
        *lock(&self.handle) = Some(handle);
    }

    /// True iff a future was ever registered for `id` in this process.
    pub(super) fn was_watched(&self, id: &TaskId) -> bool {
        lock(&self.watched).contains(id)
    }

    /// Drop the future registered for `id`, if any.
    pub(super) fn abort(&self, id: &TaskId) {
        if let Some(jh) = lock(&self.wakers).remove(id) {
            jh.abort();
        }
    }

    /// Drop every registered future.
    pub(super) fn abort_all(&self) {
        for (_, jh) in lock(&self.wakers).drain() {
            jh.abort();
        }
    }
}

impl TaskRegistrar for Registrar {
    fn watch(&self, id: TaskId, done: BoxFuture<'static, TaskOutcome>) -> Result<(), ToolError> {
        let handle = lock(&self.handle)
            .clone()
            .ok_or_else(|| ToolError::Internal("kernel handle not yet available".into()))?;
        let wakers = self.wakers.clone();
        let task_id = id.clone();
        lock(&self.watched).insert(id.clone());
        let jh = tokio::spawn(async move {
            let outcome = done.await;
            lock(&wakers).remove(&task_id);
            let status = if outcome.is_error {
                TaskStatus::Failed
            } else {
                TaskStatus::Succeeded
            };
            // A refused delivery (session `Done`) is not an error for the waker.
            let _ = handle.deliver_task_update(TaskUpdate {
                id: task_id,
                status,
                outcome: Some(outcome),
                eta: None,
                check_hint: None,
                source: WakerSource::in_process_exit(Value::Null),
            });
        });
        if let Some(old) = lock(&self.wakers).insert(id, jh) {
            old.abort();
        }
        Ok(())
    }
}

/// Appends to the event log and broadcasts every written event (`KernelHandle::subscribe`).
pub(super) struct Logger {
    pub log: Arc<dyn EventLog>,
    pub events: broadcast::Sender<Arc<Event>>,
}

impl Logger {
    pub(super) async fn log(&self, body: EventBody) -> Result<Event, LogError> {
        let ev = self.log.append(body).await?;
        // No receivers is not an error.
        let _ = self.events.send(Arc::new(ev.clone()));
        Ok(ev)
    }
}

/// Backs `HookContext::emit` / `request_compaction`. Emitted events are queued and flushed by the
/// kernel right after the hook returns, so they land at the point the hook ran (§1.4 ordering).
pub(super) struct Emitter {
    queue: Mutex<Vec<EventBody>>,
    compaction: Mutex<Option<String>>,
    allow_compaction: AtomicBool,
    turn: AtomicU64,
}

impl Emitter {
    pub(super) fn new() -> Emitter {
        Emitter {
            queue: Mutex::new(Vec::new()),
            compaction: Mutex::new(None),
            allow_compaction: AtomicBool::new(false),
            turn: AtomicU64::new(0),
        }
    }

    pub(super) fn set_turn(&self, turn: u64) {
        self.turn.store(turn, Ordering::Relaxed);
    }

    pub(super) fn allow_compaction(&self, allow: bool) {
        self.allow_compaction.store(allow, Ordering::Relaxed);
    }

    pub(super) fn take_compaction(&self) -> Option<String> {
        lock(&self.compaction).take()
    }

    pub(super) fn take_queue(&self) -> Vec<EventBody> {
        std::mem::take(&mut *lock(&self.queue))
    }
}

impl ExtensionEventSink for Emitter {
    fn emit(&self, ev: ExtensionEvent) -> Result<(), LogError> {
        lock(&self.queue).push(ev.into());
        Ok(())
    }

    fn request_compaction(&self, strategy: &str) {
        if self.allow_compaction.load(Ordering::Relaxed) {
            *lock(&self.compaction) = Some(strategy.to_owned());
        } else {
            lock(&self.queue).push(EventBody::Warning(WarningPayload::kernel(
                self.turn.load(Ordering::Relaxed),
                "compaction_request_ignored",
                "request_compaction is honored only from before_model",
                Some(serde_json::json!({ "strategy": strategy })),
            )));
        }
    }
}
