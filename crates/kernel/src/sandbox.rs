//! Sandbox policy (§3.12): the pure derivation from capability atoms lives here so the kernel can
//! derive every tool's policy at construction; the `sandbox` crate turns a `SandboxPolicy` into
//! `bwrap` arguments and provides the launchers (P1.7).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cancel::CancellationToken;
use crate::capability::{Capability, FsMode, NetAllow};
use crate::hash::{Hash, HashError};
use crate::host::{Command, FsPolicy, NetPolicy, ProcPolicy, ProcessOutput};

pub use crate::host::Mount;

/// The inner per-tool policy (§8 of the dev plan). Declarative; the evolve loop may change the
/// grants it is derived from, never the derivation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    /// Sorted by path. Overlapping mounts: the most specific path wins (bwrap semantics).
    pub mounts: Vec<Mount>,
    /// Size of the tmpfs mounted at `/tmp` (also `$HOME` and the default cwd) in MiB. Never 0.
    pub scratch_tmpfs_mb: u64,
    /// Network policy.
    pub net: NetPolicy,
    /// Wall-clock limit per invocation / RPC call.
    #[serde(with = "crate::serde_util::duration_secs", rename = "timeout_secs")]
    pub timeout: Duration,
    /// Environment variable NAMES copied from the kernel's environment into the sandbox. Everything
    /// else is dropped (D10). Names matching `SECRET_LIKE_ENV` are rejected at derivation.
    pub env_allowlist: BTreeSet<String>,
    /// Programs the tool may exec (from `Proc` atoms). Empty means "anything on the mounted PATH";
    /// enforcement of the program list is a P1.7 launcher concern (argv[0] check), not a bwrap feature.
    pub programs: BTreeSet<String>,
}

impl SandboxPolicy {
    /// The filesystem view.
    pub fn fs(&self) -> FsPolicy {
        FsPolicy {
            mounts: self.mounts.clone(),
        }
    }

    /// The process view.
    pub fn proc_(&self) -> ProcPolicy {
        ProcPolicy {
            programs: self.programs.clone(),
            env_allowlist: self.env_allowlist.clone(),
            timeout: self.timeout,
        }
    }

    /// `Hash::of_canonical_json(self)`.
    pub fn hash(&self) -> Result<Hash, HashError> {
        Hash::of_canonical_json(self)
    }
}

/// Limits applied by derivation: the resolved profile's `[sandbox]` table (`profile-schema.md` §3.9:
/// `timeout_s`, `scratch_tmpfs_mb`, `env_allow`, `network`). Limits can only narrow what atoms allow.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SandboxLimits {
    /// Default 600 s; clamped to 1 s ..= 24 h.
    #[serde(with = "crate::serde_util::duration_secs", rename = "timeout_s")]
    pub timeout: Duration,
    /// Default 256.
    pub scratch_tmpfs_mb: u64,
    /// Default `{"PATH", "LANG", "LC_ALL", "TERM", "TZ"}`; `HOME=/tmp` is always set by the launcher.
    #[serde(rename = "env_allow")]
    pub env_allowlist: BTreeSet<String>,
    /// Master switch (default `false`): when `false`, `Net` atoms are masked and the sandbox gets no network.
    pub network: bool,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        SandboxLimits {
            timeout: Duration::from_secs(600),
            scratch_tmpfs_mb: 256,
            env_allowlist: ["PATH", "LANG", "LC_ALL", "TERM", "TZ"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            network: false,
        }
    }
}

/// Env names never allowed through, checked case-insensitively as substrings:
/// `KEY`, `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD`, `CREDENTIAL`, `AUTH`.
pub const SECRET_LIKE_ENV: &[&str] = &[
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "AUTH",
];

/// True iff `name` matches `SECRET_LIKE_ENV` (case-insensitive substring).
pub fn env_name_is_secret_like(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_LIKE_ENV.iter().any(|pat| upper.contains(pat))
}

/// Shortest permitted timeout.
const MIN_TIMEOUT: Duration = Duration::from_secs(1);
/// Longest permitted timeout (24 h).
const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 3600);

/// `derive_policy_with(caps, grants, &SandboxLimits::default())`.
pub fn derive_policy(
    caps: &[Capability],
    grants: &[Capability],
) -> Result<SandboxPolicy, PolicyError> {
    derive_policy_with(caps, grants, &SandboxLimits::default())
}

/// Pure. Every atom in `caps` MUST be `covered_by(grants)` or the result is `PolicyError::Exceeds`.
///
/// Mapping: `Fs{path, mode}` → `Mount{path, mode}`; no `Fs` atom → no mounts besides scratch and the
/// launcher's read-only base image. Any `Net` atom (with `limits.network == true`) → `net.enabled = true`
/// with the union of allowlists (`Any` absorbs); no `Net` atom, or `limits.network == false` → `net.enabled
/// = false`. `Proc{p}` → `programs ∪ {p}`. `Tool`, `Spawn`, `Secret` atoms do not affect the sandbox (they
/// are enforced by the registry, `ext`, and the provider boundary respectively; a `Secret` atom in `caps`
/// is `PolicyError::SecretInSandbox`, since `caps` are what a sandboxed tool asked for).
///
/// The session **envelope** is `derive_policy_with(grants_minus_secret_atoms, grants, limits)`; its
/// hash is `State.sandbox_policy_hash`. Per-tool policies are by construction narrower than the envelope.
pub fn derive_policy_with(
    caps: &[Capability],
    grants: &[Capability],
    limits: &SandboxLimits,
) -> Result<SandboxPolicy, PolicyError> {
    let mut mounts: BTreeMap<std::path::PathBuf, FsMode> = BTreeMap::new();
    let mut programs = BTreeSet::new();
    let mut net_any = false;
    let mut net_hosts: BTreeSet<String> = BTreeSet::new();
    let mut net_present = false;

    for cap in caps {
        if let Capability::Secret { .. } = cap {
            return Err(PolicyError::SecretInSandbox);
        }
        if !cap.covered_by(grants) {
            return Err(PolicyError::Exceeds {
                cap: cap.to_string(),
            });
        }
        match cap {
            Capability::Fs { path, mode } => {
                let entry = mounts.entry(path.clone()).or_insert(*mode);
                if *mode > *entry {
                    *entry = *mode;
                }
            }
            Capability::Net { allow } => {
                net_present = true;
                match allow {
                    NetAllow::Any => net_any = true,
                    NetAllow::Hosts(hosts) => net_hosts.extend(hosts.iter().cloned()),
                }
            }
            Capability::Proc { program } => {
                programs.insert(program.clone());
            }
            Capability::Tool { .. } | Capability::Spawn { .. } | Capability::Secret { .. } => {}
        }
    }

    let mut env_allowlist = BTreeSet::new();
    for name in &limits.env_allowlist {
        if env_name_is_secret_like(name) {
            return Err(PolicyError::EnvNameForbidden(name.clone()));
        }
        env_allowlist.insert(name.clone());
    }

    let net = if limits.network && net_present {
        NetPolicy {
            enabled: true,
            allow: if net_any {
                NetAllow::Any
            } else {
                NetAllow::Hosts(net_hosts)
            },
        }
    } else {
        NetPolicy {
            enabled: false,
            allow: NetAllow::Hosts(BTreeSet::new()),
        }
    };

    Ok(SandboxPolicy {
        mounts: mounts
            .into_iter()
            .map(|(path, mode)| Mount { path, mode })
            .collect(),
        scratch_tmpfs_mb: limits.scratch_tmpfs_mb.max(1),
        net,
        timeout: limits.timeout.clamp(MIN_TIMEOUT, MAX_TIMEOUT),
        env_allowlist,
        programs,
    })
}

/// Policy errors: derivation and enforcement.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum PolicyError {
    /// A requested atom is not covered by any grant.
    #[error("capability `{cap}` is not covered by any grant")]
    Exceeds {
        /// The atom, canonical string form.
        cap: String,
    },
    /// A path check failed.
    #[error("path `{path}` is not permitted for {mode:?}")]
    PathDenied {
        /// The path.
        path: String,
        /// The requested mode.
        mode: FsMode,
    },
    /// A program is not in the policy.
    #[error("program `{0}` is not permitted")]
    ProgramDenied(String),
    /// A host is not in the allowlist.
    #[error("network is not permitted for host `{0}`")]
    NetDenied(String),
    /// A secret-shaped env name in the allowlist.
    #[error("environment variable `{0}` looks like a secret and cannot be allowlisted")]
    EnvNameForbidden(String),
    /// A sandboxed tool asked for a `secret:` atom.
    #[error("a `secret:` atom cannot be requested by a sandboxed tool")]
    SecretInSandbox,
}

/// JSON-RPC 2.0 shaped call into a `Session` process (matches the P0.3 prototype).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RpcRequest {
    /// Method name.
    pub method: String,
    /// Parameters.
    pub params: Value,
}

/// JSON-RPC 2.0 shaped response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcResponse {
    /// Success.
    Result(Value),
    /// Protocol-level error.
    Error {
        /// JSON-RPC error code.
        code: i64,
        /// Message.
        message: String,
        /// Extra data.
        data: Option<Value>,
    },
}

/// A running `Session`-kind tool process.
#[async_trait]
pub trait SessionProcess: Send + Sync {
    /// One RPC call.
    async fn call(
        &self,
        req: RpcRequest,
        cancel: CancellationToken,
    ) -> Result<RpcResponse, SandboxError>;
    /// Whether the process is still running.
    fn is_alive(&self) -> bool;
    /// SIGTERM, then SIGKILL after the backend's grace period.
    async fn terminate(&self) -> Result<(), SandboxError>;
}

/// The sandbox enforcement backend (D14).
#[async_trait]
pub trait SandboxBackend: Send + Sync {
    /// `"bwrap"` or `"none"`. Written to `State.sandbox_backend` and `session_created` (D14).
    fn name(&self) -> &'static str;
    /// One sandbox per call. Enforces `policy.timeout`; on `cancel`, SIGTERM then SIGKILL (D15).
    async fn launch_stateless(
        &self,
        policy: &SandboxPolicy,
        cmd: Command,
        cancel: CancellationToken,
    ) -> Result<ProcessOutput, SandboxError>;
    /// One sandbox for the session; calls are RPC (D5). The backend owns the process until `terminate`.
    async fn launch_session(
        &self,
        policy: &SandboxPolicy,
        cmd: Command,
    ) -> Result<Box<dyn SessionProcess>, SandboxError>;
}

/// Sandbox errors.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// The sandbox could not be started.
    #[error("launch failed: {0}")]
    Launch(String),
    /// An RPC call failed at the protocol level.
    #[error("rpc failed: {0}")]
    Rpc(String),
    /// The session process is gone.
    #[error("session process exited")]
    Exited,
    /// The policy timeout fired.
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    /// Cancelled.
    #[error("cancelled")]
    Cancelled,
}
