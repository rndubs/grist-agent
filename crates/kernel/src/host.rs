//! `Host` (§3.9): filesystem, process spawn, network, secrets-as-handles (D10), `ask_user` (D17).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::capability::{FsMode, NetAllow};
use crate::sandbox::PolicyError;

/// Filesystem view of a `SandboxPolicy` (§3.12). In-process tools (`read`, `write`, `edit`) are
/// sandboxed by these checks (D5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FsPolicy {
    /// Mounts, sorted by path; the most specific path wins.
    pub mounts: Vec<Mount>,
}

/// One bind mount.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    /// Absolute, normalized path.
    pub path: PathBuf,
    /// Access mode.
    pub mode: FsMode,
}

impl FsPolicy {
    /// Ok iff `path` (normalized, symlinks resolved by the Host before calling) is under a mount whose
    /// mode permits `mode`. Overlapping mounts: the most specific (longest) path decides.
    pub fn check(&self, path: &Path, mode: FsMode) -> Result<(), PolicyError> {
        let denied = || PolicyError::PathDenied {
            path: path.display().to_string(),
            mode,
        };
        let best = self
            .mounts
            .iter()
            .filter(|m| path.starts_with(&m.path))
            .max_by_key(|m| m.path.components().count());
        match best {
            Some(m) if mode <= m.mode => Ok(()),
            _ => Err(denied()),
        }
    }
}

/// Process view of a `SandboxPolicy`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcPolicy {
    /// Programs the process may exec; empty means anything on the mounted PATH.
    pub programs: BTreeSet<String>,
    /// Environment variable names passed through.
    pub env_allowlist: BTreeSet<String>,
    /// Wall-clock limit.
    #[serde(with = "crate::serde_util::duration_secs", rename = "timeout_secs")]
    pub timeout: Duration,
}

/// Network view of a `SandboxPolicy`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NetPolicy {
    /// Master switch.
    pub enabled: bool,
    /// Allowed hosts when enabled.
    pub allow: NetAllow,
}

/// One directory entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirEntry {
    /// File name (no directory part).
    pub name: String,
    /// Directory or not.
    pub is_dir: bool,
    /// Size in bytes.
    pub size: u64,
}

/// File metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    /// Directory or not.
    pub is_dir: bool,
    /// Size in bytes.
    pub size: u64,
    /// Modification time, RFC 3339.
    pub modified: Option<String>,
}

/// A process to run. Env is exactly `env` (scrubbed; no inheritance), see D10.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Command {
    /// Program name or path.
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: Option<PathBuf>,
    /// The complete environment.
    pub env: BTreeMap<String, String>,
    /// Bytes to feed on stdin.
    pub stdin: Option<Vec<u8>>,
}

impl Command {
    /// A command with no args, no cwd, an empty env, and no stdin.
    pub fn new(program: impl Into<String>) -> Command {
        Command {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            stdin: None,
        }
    }
}

/// What a finished process produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessOutput {
    /// Exit code, if it exited normally.
    pub exit_code: Option<i32>,
    /// Terminating signal, if any.
    pub signal: Option<i32>,
    /// Captured stdout.
    pub stdout: Vec<u8>,
    /// Captured stderr.
    pub stderr: Vec<u8>,
    /// Whether the timeout fired.
    pub timed_out: bool,
    /// Wall time.
    #[serde(with = "crate::serde_util::duration_ms", rename = "duration_ms")]
    pub duration: Duration,
}

/// A child the Host spawned (used by `run_script` and the in-process waker).
#[async_trait]
pub trait ChildProcess: Send + Sync {
    /// OS pid, if known.
    fn pid(&self) -> Option<u32>;
    /// Wait for exit and collect output.
    async fn wait(&self) -> Result<ProcessOutput, HostError>;
    /// SIGTERM, then SIGKILL after `grace`.
    async fn terminate(&self, grace: Duration) -> Result<(), HostError>;
}

/// HTTP method.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// GET
    Get,
    /// POST
    Post,
    /// PUT
    Put,
    /// DELETE
    Delete,
    /// PATCH
    Patch,
    /// HEAD
    Head,
}

/// An outbound HTTP request.
pub struct HttpRequest {
    /// Method.
    pub method: HttpMethod,
    /// Absolute URL.
    pub url: String,
    /// Headers.
    pub headers: Vec<(String, String)>,
    /// Body bytes.
    pub body: Vec<u8>,
    /// Per-request timeout.
    pub timeout: Option<Duration>,
}

/// An HTTP response with a streamed body.
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// Headers.
    pub headers: Vec<(String, String)>,
    /// Chunked body so providers can parse SSE incrementally.
    pub body:
        std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<Vec<u8>, HostError>> + Send>>,
}

/// Network access as the Host provides it (proxy, CA bundle, and `host::remote-client` routing live behind this).
#[async_trait]
pub trait NetHandle: Send + Sync {
    /// Send one request.
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, HostError>;
}

/// Opaque reference to a secret. Cloneable, loggable (prints only the name), useless without a resolver.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretHandle {
    name: String,
    /// Host-specific locator, never serialized.
    #[serde(skip)]
    locator: Option<String>,
}

impl SecretHandle {
    /// A handle with no locator.
    pub fn new(name: impl Into<String>) -> SecretHandle {
        SecretHandle {
            name: name.into(),
            locator: None,
        }
    }

    /// A handle carrying a host-specific locator (an env var name, a file path, ...).
    pub fn with_locator(name: impl Into<String>, locator: impl Into<String>) -> SecretHandle {
        SecretHandle {
            name: name.into(),
            locator: Some(locator.into()),
        }
    }

    /// The secret's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The host-specific locator, if any. Only the host that issued the handle interprets it.
    pub fn locator(&self) -> Option<&str> {
        self.locator.as_deref()
    }
}

impl std::fmt::Debug for SecretHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretHandle({})", self.name)
    }
}

/// A resolved secret. `Debug`/`Display` print `SecretString(***)`. Does not implement `Serialize`.
/// Best-effort zeroed on drop.
pub struct SecretString(Vec<u8>);

impl SecretString {
    /// Wrap a value.
    pub fn new(value: impl Into<String>) -> SecretString {
        SecretString(value.into().into_bytes())
    }

    /// The secret value.
    pub fn expose(&self) -> &str {
        std::str::from_utf8(&self.0).expect("constructed from a String")
    }
}

impl Clone for SecretString {
    fn clone(&self) -> Self {
        SecretString(self.0.clone())
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            *b = 0;
        }
        std::hint::black_box(&self.0);
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString(***)")
    }
}

impl std::fmt::Display for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString(***)")
    }
}

/// A question for the user (D17).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AskUserRequest {
    /// Stable per question within a session (`format!("q{turn}-{tool_use_id}")`).
    pub question_id: String,
    /// The question.
    pub question: String,
    /// Optional fixed choices; empty means free text.
    #[serde(default)]
    pub options: Vec<String>,
    /// Whether free text is accepted alongside `options`.
    pub allow_free_text: bool,
}

/// The user's answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserAnswer {
    /// Echo of the question id.
    pub question_id: String,
    /// `None` when the user declined.
    pub answer: Option<String>,
}

/// The host surface tools and the kernel use. No secret values are reachable through it (D10).
#[async_trait]
pub trait Host: Send + Sync {
    /// `"native"` | `"remote-client"`
    fn name(&self) -> &str;

    /// Read a file. Every call takes the policy and MUST enforce it (D5). Paths are absolute.
    async fn read_file(&self, policy: &FsPolicy, path: &Path) -> Result<Vec<u8>, HostError>;
    /// Write a file (create or truncate).
    async fn write_file(
        &self,
        policy: &FsPolicy,
        path: &Path,
        bytes: &[u8],
    ) -> Result<(), HostError>;
    /// List a directory.
    async fn list_dir(&self, policy: &FsPolicy, path: &Path) -> Result<Vec<DirEntry>, HostError>;
    /// Stat a path.
    async fn stat(&self, policy: &FsPolicy, path: &Path) -> Result<Metadata, HostError>;
    /// Remove a file or empty directory.
    async fn remove(&self, policy: &FsPolicy, path: &Path) -> Result<(), HostError>;

    /// Spawn WITHOUT a sandbox (used only by the kernel for in-process waker helpers and by the
    /// sandbox crate's launchers to exec `bwrap` itself). Tools use `SandboxBackend`, never this.
    async fn spawn(
        &self,
        policy: &ProcPolicy,
        cmd: Command,
    ) -> Result<Box<dyn ChildProcess>, HostError>;

    /// Network access under a policy.
    fn network(&self, policy: &NetPolicy) -> Result<Arc<dyn NetHandle>, HostError>;

    /// Returns a handle; never the value (D10). Errors if the secret is unknown to the host.
    fn secret(&self, name: &str) -> Result<SecretHandle, HostError>;

    /// D17. The kernel logs `ask_user` before and `user_answer` after this call.
    async fn ask_user(&self, req: AskUserRequest) -> Result<UserAnswer, HostError>;
}

/// PROVIDER-ONLY (D10). Not a supertrait of `Host` on purpose: a `&dyn Host` (what tools and hooks
/// hold) has no path to a secret value. The launcher hands `Arc<dyn SecretResolver>` to provider
/// clients only. The `host` crate's native type implements both traits.
pub trait SecretResolver: Send + Sync {
    /// Resolves and, as a side effect, registers the value with the `Redactor` (§3.14) so every
    /// payload written afterwards is scrubbed of it.
    fn resolve_secret(&self, handle: &SecretHandle) -> Result<SecretString, HostError>;
}

/// Host errors.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// Refused by policy.
    #[error("denied by policy: {0}")]
    Denied(#[from] PolicyError),
    /// No such path.
    #[error("not found: {0}")]
    NotFound(String),
    /// I/O failure.
    #[error("io error: {0}")]
    Io(String),
    /// The host knows no secret by that name.
    #[error("unknown secret `{0}`")]
    UnknownSecret(String),
    /// No user is attached.
    #[error("user interaction unavailable: {0}")]
    NoUser(String),
    /// Network failure.
    #[error("network error: {0}")]
    Net(String),
    /// Cancelled.
    #[error("cancelled")]
    Cancelled,
}
