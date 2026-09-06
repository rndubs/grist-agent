//! The `bwrap` backend (D5, D14): the pure `SandboxPolicy` → argv mapping and the launchers.
//!
//! The argv shape is exactly the inner sandbox of `spikes/sandbox-nesting/inner-tool-call.sh`
//! (ADR-0002 is pending a human run on the login node; this is the shape it validates):
//!
//! ```text
//! bwrap --unshare-user --unshare-pid --unshare-ipc --unshare-uts [--unshare-net | --share-net]
//!       --uid 1000 --gid 1000
//!       --ro-bind / /                       # read-only base image
//!       (--bind|--ro-bind) <path> <path> …  # one per policy mount, most specific last (it wins)
//!       --size <bytes> --tmpfs /tmp         # scratch, `scratch_tmpfs_mb` MiB; also $HOME and default cwd
//!       --proc /proc --dev /dev
//!       --clearenv --setenv NAME VALUE …    # the scrubbed env only (D10)
//!       --chdir <cmd.cwd or /tmp>
//!       --die-with-parent --new-session
//!       -- <program> <args…>
//! ```
//!
//! What bwrap cannot enforce in P1: the network **allowlist** (`NetPolicy.allow`). When
//! `net.enabled` the sandbox keeps the whole network namespace; host filtering is a later
//! milestone (a proxy or nftables in the outer sandbox). The program list is enforced by the
//! launcher's argv\[0\] check ([`crate::check_program`]), not by bwrap.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kernel::cancel::CancellationToken;
use kernel::capability::FsMode;
use kernel::host::{Command, Host, ProcPolicy, ProcessOutput};
use kernel::sandbox::{SandboxBackend, SandboxError, SandboxPolicy, SessionProcess};

use crate::env::{SANDBOX_HOME, check_program, scrubbed_env, scrubbed_env_with};
use crate::launch::{host_error, supervise};
use crate::session::{JsonRpcSession, SessionSpec};

/// Default grace between SIGTERM and SIGKILL (§7.1: "default 5 s").
pub const DEFAULT_GRACE: Duration = Duration::from_secs(5);

/// The uid/gid every sandboxed process runs as (the spike's `--uid 1000 --gid 1000`).
const SANDBOX_UID: &str = "1000";
const SANDBOX_GID: &str = "1000";

/// `bwrap` argv for `policy` and `cmd`, with the environment read from the kernel process via
/// [`std::env::var`]. Pure apart from that lookup; see [`policy_to_args_with_env`].
pub fn policy_to_args(policy: &SandboxPolicy, cmd: &Command) -> Result<Vec<String>, SandboxError> {
    policy_to_args_with_env(policy, cmd, |name| std::env::var(name).ok())
}

/// Pure `bwrap` argv mapping. `lookup` resolves allowlisted environment names (tests inject a
/// fake). Fails only when the policy's allowlist names a secret-like variable
/// (`SandboxError::Launch`), which derivation should already have refused.
///
/// The mapping, field by field:
///
/// | policy field | argv |
/// |---|---|
/// | (always) | `--unshare-user --unshare-pid --unshare-ipc --unshare-uts --uid 1000 --gid 1000 --ro-bind / /` |
/// | `mounts[i]` (sorted, shallowest first) | `--bind p p` for `Rw`, `--ro-bind p p` for `Ro` |
/// | `scratch_tmpfs_mb` | `--size <mb·1048576> --tmpfs /tmp` |
/// | `net.enabled == false` | `--unshare-net` |
/// | `net.enabled == true` | `--share-net` (allowlist not enforceable in P1) |
/// | `env_allowlist` ∩ process env, `cmd.env`, `HOME=/tmp` | `--clearenv` then `--setenv NAME VALUE` each |
/// | `cmd.cwd` | `--chdir <cwd>` (default `/tmp`) |
/// | (always) | `--proc /proc --dev /dev --die-with-parent --new-session -- <program> <args…>` |
///
/// `timeout` and `programs` are launcher concerns (`supervise`, `check_program`), not argv.
pub fn policy_to_args_with_env(
    policy: &SandboxPolicy,
    cmd: &Command,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Vec<String>, SandboxError> {
    let env = scrubbed_env_with(policy, cmd, lookup)?;
    Ok(build_args(policy, cmd, &env))
}

fn build_args(
    policy: &SandboxPolicy,
    cmd: &Command,
    env: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let push =
        |args: &mut Vec<String>, items: &[&str]| args.extend(items.iter().map(|s| s.to_string()));

    push(
        &mut args,
        &[
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
        ],
    );
    if policy.net.enabled {
        push(&mut args, &["--share-net"]);
    } else {
        push(&mut args, &["--unshare-net"]);
    }
    push(&mut args, &["--uid", SANDBOX_UID, "--gid", SANDBOX_GID]);
    push(&mut args, &["--ro-bind", "/", "/"]);

    // Shallowest first so the most specific mount is applied last and wins (bwrap semantics).
    let mut mounts: Vec<_> = policy.mounts.iter().collect();
    mounts.sort_by_key(|m| (m.path.components().count(), m.path.clone()));
    for m in mounts {
        let flag = match m.mode {
            FsMode::Rw => "--bind",
            FsMode::Ro => "--ro-bind",
        };
        let p = m.path.to_string_lossy().into_owned();
        push(&mut args, &[flag, &p, &p]);
    }

    let bytes = policy.scratch_tmpfs_mb.max(1).saturating_mul(1024 * 1024);
    push(
        &mut args,
        &["--size", &bytes.to_string(), "--tmpfs", SANDBOX_HOME],
    );
    push(&mut args, &["--proc", "/proc", "--dev", "/dev"]);

    push(&mut args, &["--clearenv"]);
    for (name, value) in env {
        push(&mut args, &["--setenv", name, value]);
    }

    let cwd = cmd
        .cwd
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| SANDBOX_HOME.to_owned());
    push(&mut args, &["--chdir", &cwd]);
    push(&mut args, &["--die-with-parent", "--new-session", "--"]);
    args.push(cmd.program.clone());
    args.extend(cmd.args.iter().cloned());
    args
}

/// `SandboxBackend` backed by bubblewrap. `name() == "bwrap"`.
///
/// Stateless launches exec `bwrap` **through [`Host::spawn`]** (§3.9: the one legitimate
/// non-kernel use of `spawn`) under a `ProcPolicy{programs: {bwrap}, env_allowlist, timeout}`.
/// Session launches spawn `bwrap` with `tokio::process::Command` directly, because the RPC
/// transport needs piped stdio and `ChildProcess` exposes no pipes (documented deviation; the
/// argv and environment are identical to the stateless path).
pub struct BwrapBackend {
    host: Arc<dyn Host>,
    bwrap: String,
    grace: Duration,
}

impl BwrapBackend {
    /// A backend that runs `bwrap` from `PATH` with [`DEFAULT_GRACE`].
    pub fn new(host: Arc<dyn Host>) -> BwrapBackend {
        BwrapBackend {
            host,
            bwrap: "bwrap".to_owned(),
            grace: DEFAULT_GRACE,
        }
    }

    /// Use an explicit `bwrap` binary (name or absolute path).
    pub fn with_bwrap_path(mut self, path: impl Into<String>) -> BwrapBackend {
        self.bwrap = path.into();
        self
    }

    /// Override the SIGTERM → SIGKILL grace period.
    pub fn with_grace(mut self, grace: Duration) -> BwrapBackend {
        self.grace = grace;
        self
    }

    /// The `bwrap` program this backend execs.
    pub fn bwrap_path(&self) -> &str {
        &self.bwrap
    }

    /// The SIGTERM → SIGKILL grace period.
    pub fn grace(&self) -> Duration {
        self.grace
    }

    /// Program check, scrubbed env, argv. Shared by both launchers.
    fn prepare(
        &self,
        policy: &SandboxPolicy,
        cmd: &Command,
    ) -> Result<(Vec<String>, BTreeMap<String, String>), SandboxError> {
        check_program(policy, cmd)?;
        let env = scrubbed_env(policy, cmd)?;
        Ok((build_args(policy, cmd, &env), env))
    }
}

#[async_trait]
impl SandboxBackend for BwrapBackend {
    fn name(&self) -> &'static str {
        "bwrap"
    }

    async fn launch_stateless(
        &self,
        policy: &SandboxPolicy,
        cmd: Command,
        cancel: CancellationToken,
    ) -> Result<ProcessOutput, SandboxError> {
        let (args, env) = self.prepare(policy, &cmd)?;
        let proc_policy = ProcPolicy {
            programs: [self.bwrap.clone()].into_iter().collect(),
            env_allowlist: policy.env_allowlist.clone(),
            timeout: policy.timeout,
        };
        // bwrap itself gets the scrubbed env too (`--clearenv` makes it moot inside, but the
        // wrapper process must not hold secrets either). Its cwd is the host's; `--chdir` sets
        // the sandbox's.
        let wrapper = Command {
            program: self.bwrap.clone(),
            args,
            cwd: None,
            env,
            stdin: cmd.stdin,
        };
        let child = self
            .host
            .spawn(&proc_policy, wrapper)
            .await
            .map_err(|e| host_error("spawn bwrap", e))?;
        supervise(child, policy.timeout, cancel, self.grace).await
    }

    async fn launch_session(
        &self,
        policy: &SandboxPolicy,
        cmd: Command,
    ) -> Result<Box<dyn SessionProcess>, SandboxError> {
        let (args, env) = self.prepare(policy, &cmd)?;
        let session = JsonRpcSession::spawn(SessionSpec {
            program: self.bwrap.clone(),
            args,
            cwd: None,
            env,
            timeout: policy.timeout,
            grace: self.grace,
        })
        .await?;
        Ok(Box::new(session))
    }
}

/// True iff `program` resolves to an executable on `PATH` (or is an existing absolute path).
/// Tests use it to skip when `bwrap` is not installed.
pub fn program_available(program: &str) -> bool {
    let p = Path::new(program);
    if p.is_absolute() {
        return p.is_file();
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}
