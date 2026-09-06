//! `JsonRpcSession`: a `SessionProcess` speaking newline-delimited JSON-RPC 2.0 over a child's
//! stdio (ADR-0001, `docs/spikes/extension-mechanism.md` Prototype A).
//!
//! Wire shape, one JSON object per line:
//!
//! ```text
//! → {"jsonrpc":"2.0","id":<n>,"method":"<m>","params":<p>}
//! ← {"jsonrpc":"2.0","id":<n>,"result":<v>}
//! ← {"jsonrpc":"2.0","id":<n>,"error":{"code":<i>,"message":"<s>","data":<v>?}}
//! ```
//!
//! A JSON-RPC `error` is a **protocol** failure (unknown method, bad params, parse error) and is
//! returned as `RpcResponse::Error`. A failure in *user code* (an exception in the REPL) is a
//! successful call whose `result` carries `ok:false` and a structured `error`; the session never
//! interprets results.
//!
//! Cancellation and timeouts (D15): every call runs under the policy timeout and the caller's
//! token. When either fires the process gets SIGTERM, then SIGKILL after the grace period —
//! mandatory, because a process under `--unshare-pid` is PID 1 in its namespace and ignores an
//! unhandled SIGTERM — the session is marked dead, and the call returns `Timeout`/`Cancelled`.
//! The kernel relaunches a dead session once at the next invoke (§7.9).
//!
//! One call at a time (a mutex serializes callers); stderr is drained into a bounded buffer for
//! diagnostics ([`JsonRpcSession::stderr_tail`]).
//!
//! The child is spawned with `tokio::process::Command` (not `Host::spawn`): the transport needs
//! piped stdin/stdout and `ChildProcess` exposes no pipes. The environment is `env_clear()` plus
//! exactly the scrubbed map the backend computed, and `kill_on_drop` is set.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use kernel::cancel::CancellationToken;
use kernel::sandbox::{RpcRequest, RpcResponse, SandboxError, SessionProcess};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

/// How much of the child's stderr is retained (the tail).
pub const STDERR_TAIL_BYTES: usize = 64 * 1024;

/// Longest a single response line may be before the session is considered broken (64 MiB).
const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;

/// How often the terminator polls for the child to be reaped.
const REAP_POLL: Duration = Duration::from_millis(20);

/// Upper bound on waiting for a SIGKILLed child to be reaped.
const KILL_WAIT: Duration = Duration::from_secs(10);

/// What to spawn. Backends fill this in from the policy; the session does no policy work itself.
#[derive(Clone, Debug)]
pub struct SessionSpec {
    /// Program to exec (`bwrap` for the bwrap backend; the tool's program for `None`).
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Working directory of the spawned process (`None` = inherit).
    pub cwd: Option<PathBuf>,
    /// The complete environment (already scrubbed).
    pub env: BTreeMap<String, String>,
    /// Per-call timeout (`SandboxPolicy.timeout`).
    pub timeout: Duration,
    /// SIGTERM → SIGKILL grace.
    pub grace: Duration,
}

struct Io {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

/// A running JSON-RPC session process. See the module docs.
pub struct JsonRpcSession {
    child: StdMutex<Child>,
    pid: u32,
    io: Mutex<Io>,
    dead: AtomicBool,
    stderr: Arc<StdMutex<Vec<u8>>>,
    timeout: Duration,
    grace: Duration,
    program: String,
}

impl JsonRpcSession {
    /// Spawn the process with piped stdio.
    pub async fn spawn(spec: SessionSpec) -> Result<JsonRpcSession, SandboxError> {
        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .env_clear()
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        let mut child = command
            .spawn()
            .map_err(|e| SandboxError::Launch(format!("spawn `{}`: {e}", spec.program)))?;
        let pid = child
            .id()
            .ok_or_else(|| SandboxError::Launch("child has no pid".to_owned()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| SandboxError::Launch("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| SandboxError::Launch("no stdout".into()))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| SandboxError::Launch("no stderr".into()))?;

        let stderr = Arc::new(StdMutex::new(Vec::new()));
        let sink = stderr.clone();
        tokio::spawn(async move {
            let mut pipe = stderr_pipe;
            let mut buf = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut tail = sink.lock().unwrap_or_else(PoisonError::into_inner);
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > STDERR_TAIL_BYTES {
                    let cut = tail.len() - STDERR_TAIL_BYTES;
                    tail.drain(..cut);
                }
            }
        });

        Ok(JsonRpcSession {
            child: StdMutex::new(child),
            pid,
            io: Mutex::new(Io {
                stdin,
                stdout: BufReader::new(stdout),
                next_id: 0,
            }),
            dead: AtomicBool::new(false),
            stderr,
            timeout: spec.timeout,
            grace: spec.grace,
            program: spec.program,
        })
    }

    /// OS pid of the spawned process (for `bwrap` this is the wrapper's pid).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The retained tail of the child's stderr, lossily decoded.
    pub fn stderr_tail(&self) -> String {
        let tail = self.stderr.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&tail).into_owned()
    }

    /// True iff the child has been reaped (exit status known).
    fn reaped(&self) -> bool {
        let mut child = self.child.lock().unwrap_or_else(PoisonError::into_inner);
        !matches!(child.try_wait(), Ok(None))
    }

    /// SIGTERM, poll for exit up to `grace`, then SIGKILL and wait for the reap. Idempotent.
    async fn kill_process(&self) {
        self.dead.store(true, Ordering::SeqCst);
        if self.reaped() {
            return;
        }
        let pid = Pid::from_raw(self.pid as i32);
        let _ = kill(pid, Signal::SIGTERM);
        let deadline = Instant::now() + self.grace;
        while Instant::now() < deadline {
            if self.reaped() {
                return;
            }
            tokio::time::sleep(REAP_POLL).await;
        }
        let _ = kill(pid, Signal::SIGKILL);
        let deadline = Instant::now() + KILL_WAIT;
        while Instant::now() < deadline && !self.reaped() {
            tokio::time::sleep(REAP_POLL).await;
        }
    }

    fn mark_dead(&self) {
        self.dead.store(true, Ordering::SeqCst);
    }

    fn exited(&self, what: &str) -> SandboxError {
        self.mark_dead();
        let tail = self.stderr_tail();
        if tail.trim().is_empty() {
            SandboxError::Exited
        } else {
            SandboxError::Rpc(format!(
                "{what}: `{}` exited; stderr tail: {}",
                self.program,
                tail.trim_end()
            ))
        }
    }
}

fn parse_response(line: &str, expected_id: u64) -> Result<RpcResponse, SandboxError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|e| SandboxError::Rpc(format!("malformed response line: {e}")))?;
    let obj = value
        .as_object()
        .ok_or_else(|| SandboxError::Rpc("response is not a JSON object".to_owned()))?;
    match obj.get("id") {
        Some(Value::Number(n)) if n.as_u64() == Some(expected_id) => {}
        // A parse error from the guest carries `id: null`; it still answers this call.
        Some(Value::Null) | None if obj.contains_key("error") => {}
        other => {
            return Err(SandboxError::Rpc(format!(
                "response id mismatch: expected {expected_id}, got {}",
                other.cloned().unwrap_or(Value::Null)
            )));
        }
    }
    if let Some(err) = obj.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(-32603);
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let data = err.get("data").filter(|d| !d.is_null()).cloned();
        return Ok(RpcResponse::Error {
            code,
            message,
            data,
        });
    }
    Ok(RpcResponse::Result(
        obj.get("result").cloned().unwrap_or(Value::Null),
    ))
}

#[async_trait]
impl SessionProcess for JsonRpcSession {
    async fn call(
        &self,
        req: RpcRequest,
        cancel: CancellationToken,
    ) -> Result<RpcResponse, SandboxError> {
        if !self.is_alive() {
            return Err(self.exited("call"));
        }
        let mut io = self.io.lock().await;
        io.next_id += 1;
        let id = io.next_id;
        let mut line = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": req.method,
            "params": req.params,
        }))
        .map_err(|e| SandboxError::Rpc(format!("encode request: {e}")))?;
        line.push('\n');
        if io.stdin.write_all(line.as_bytes()).await.is_err() || io.stdin.flush().await.is_err() {
            return Err(self.exited("write request"));
        }

        let mut buf = Vec::new();
        let read = async {
            let mut chunk = Vec::new();
            loop {
                chunk.clear();
                let n = io.stdout.read_until(b'\n', &mut chunk).await?;
                if n == 0 {
                    return Ok::<usize, std::io::Error>(buf.len());
                }
                buf.extend_from_slice(&chunk);
                if buf.ends_with(b"\n") {
                    return Ok(buf.len());
                }
                if buf.len() > MAX_LINE_BYTES {
                    return Err(std::io::Error::other("response line too long"));
                }
            }
        };
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                self.kill_process().await;
                return Err(SandboxError::Cancelled);
            }
            _ = tokio::time::sleep(self.timeout) => {
                self.kill_process().await;
                return Err(SandboxError::Timeout(self.timeout));
            }
            r = read => r,
        };
        match outcome {
            Err(e) => {
                self.kill_process().await;
                Err(SandboxError::Rpc(format!("read response: {e}")))
            }
            Ok(0) => Err(self.exited("read response")),
            Ok(_) => {
                let text = String::from_utf8_lossy(&buf);
                parse_response(text.trim_end(), id)
            }
        }
    }

    fn is_alive(&self) -> bool {
        !self.dead.load(Ordering::SeqCst) && !self.reaped()
    }

    async fn terminate(&self) -> Result<(), SandboxError> {
        self.kill_process().await;
        Ok(())
    }
}
