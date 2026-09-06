//! One session's kernel behind a command channel. `Kernel::run` needs `&mut self`, so exactly one
//! task owns the kernel; the protocol handlers send [`Cmd`]s and await replies. Cancellation does
//! not go through here: it is `KernelHandle::cancel`, which never blocks (D15).

use kernel::{
    Kernel, KernelError, KernelHandle, Message, ResumeCause, RunStop, SessionStatus, TaskId,
};
use tokio::sync::{mpsc, oneshot};

use super::grist::StatusResponse;

/// What the driver can be asked to do.
pub enum Cmd {
    /// Enqueue a user message and drive the session until it is `Idle`, `Done`, `Failed`, or
    /// suspended on an external waker (a suspension on an in-process waker is driven through:
    /// the waker delivers, the kernel resumes, and `run` continues).
    Prompt(Message, oneshot::Sender<Result<RunStop, KernelError>>),
    /// Apply a resume cause in-process (`Suspended`/`Failed`) and drive as above.
    Resume(ResumeCause, oneshot::Sender<Result<RunStop, KernelError>>),
    /// Snapshot of status, turn and open tasks.
    Status(oneshot::Sender<StatusResponse>),
    /// `Kernel::end` (D2): terminal.
    End(oneshot::Sender<Result<(), KernelError>>),
}

/// The sending side.
#[derive(Clone)]
pub struct Driver {
    tx: mpsc::Sender<Cmd>,
    handle: KernelHandle,
}

impl Driver {
    /// Spawn the owning task for `kernel`.
    pub fn spawn(kernel: Kernel, log_path: String) -> Driver {
        let handle = kernel.handle();
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(drive(kernel, rx, log_path));
        Driver { tx, handle }
    }

    /// The kernel handle (cancel, subscribe).
    pub fn handle(&self) -> &KernelHandle {
        &self.handle
    }

    async fn send<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> Cmd) -> Option<T> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(make(tx)).await.ok()?;
        rx.await.ok()
    }

    /// See [`Cmd::Prompt`]. `None` when the driver is gone (the session ended).
    pub async fn prompt(&self, msg: Message) -> Option<Result<RunStop, KernelError>> {
        self.send(|tx| Cmd::Prompt(msg, tx)).await
    }

    /// See [`Cmd::Resume`].
    pub async fn resume(&self, cause: ResumeCause) -> Option<Result<RunStop, KernelError>> {
        self.send(|tx| Cmd::Resume(cause, tx)).await
    }

    /// See [`Cmd::Status`].
    pub async fn status(&self) -> Option<StatusResponse> {
        self.send(Cmd::Status).await
    }

    /// See [`Cmd::End`].
    pub async fn end(&self) -> Option<Result<(), KernelError>> {
        self.send(Cmd::End).await
    }
}

/// Run until the session stops for a reason the client must hear about.
async fn run_through(kernel: &mut Kernel) -> Result<RunStop, KernelError> {
    loop {
        match kernel.run().await? {
            RunStop::Suspended(s) if s.in_process_wakers > 0 => continue,
            stop => return Ok(stop),
        }
    }
}

fn status_of(kernel: &Kernel, log_path: &str) -> StatusResponse {
    let mut ids: Vec<TaskId> = kernel.state().open_tasks().map(|t| t.id.clone()).collect();
    ids.sort();
    let wakers = kernel
        .state()
        .open_tasks()
        .filter(|t| t.in_process_waker)
        .count() as u32;
    StatusResponse {
        status: kernel.status(),
        turn: kernel.state().turn,
        pending_task_ids: ids,
        in_process_wakers: wakers,
        log_path: log_path.to_owned(),
    }
}

async fn drive(mut kernel: Kernel, mut rx: mpsc::Receiver<Cmd>, log_path: String) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Prompt(msg, reply) => {
                let r = match kernel.handle().enqueue_user_message(msg) {
                    Ok(()) => run_through(&mut kernel).await,
                    Err(e) => Err(e),
                };
                let _ = reply.send(r);
            }
            Cmd::Resume(cause, reply) => {
                let r = match kernel.status() {
                    SessionStatus::Suspended | SessionStatus::Failed => {
                        match kernel.resume(cause).await {
                            Ok(()) => run_through(&mut kernel).await,
                            Err(e) => Err(e),
                        }
                    }
                    // Already resumed by `Kernel::open`, or nothing to resume: just drive.
                    _ => run_through(&mut kernel).await,
                };
                let _ = reply.send(r);
            }
            Cmd::Status(reply) => {
                let _ = reply.send(status_of(&kernel, &log_path));
            }
            Cmd::End(reply) => {
                let r = kernel.end().await;
                let _ = reply.send(r);
                return;
            }
        }
    }
}
