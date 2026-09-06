//! `sandbox` — inner bwrap policy enforcement, the `SandboxBackend` implementations, the
//! JSON-RPC session launcher, and the six base tools (P1.7).
//!
//! The policy *type* and its pure derivation from capability atoms live in `kernel::sandbox`
//! (§3.12 of `docs/specs/kernel-interface.md`); this crate re-exports them as
//! [`derive_policy`] / [`derive_policy_with`] and turns a [`SandboxPolicy`] into enforcement:
//!
//! - [`BwrapBackend`] — one `bwrap` per stateless call, one `bwrap` per session, built against
//!   the inner shape of `spikes/sandbox-nesting/inner-tool-call.sh` (`--unshare-*`, `--ro-bind / /`,
//!   per-mount binds, `--tmpfs /tmp`, `--clearenv` + explicit `--setenv`, `--die-with-parent
//!   --new-session`). [`policy_to_args`] is the pure mapping.
//! - [`JsonRpcSession`] — newline-delimited JSON-RPC 2.0 over a child's stdio (ADR-0001), with the
//!   per-call policy timeout and SIGTERM → grace → SIGKILL on cancel (D15). Shared by both backends.
//! - [`NoneBackend`] (feature `dev-sandbox-none` only, D14) — runs commands directly with the same
//!   scrubbed environment and program check, without isolation. Never in a release build.
//! - [`tools`] — `read`, `write`, `edit` (in-process, `Host` path checks), `bash` (Stateless),
//!   `run_script` (Stateless, returns a `Task`, D1) and `python` (Session REPL, ADR-0001).
//!
//! Secrets (D10): a sandbox receives exactly the policy's `env_allowlist` names from the kernel
//! process environment, never a name matching [`SECRET_LIKE_ENV`], plus `HOME=/tmp` and the tool's
//! own explicit `Command::env`. See [`scrubbed_env`].

pub mod bwrap;
pub mod env;
pub mod launch;
#[cfg(feature = "dev-sandbox-none")]
pub mod none;
pub mod session;
pub mod tools;

pub use bwrap::{BwrapBackend, DEFAULT_GRACE, policy_to_args, policy_to_args_with_env};
pub use env::{check_program, scrubbed_env, scrubbed_env_with};
pub use kernel::sandbox::{
    PolicyError, RpcRequest, RpcResponse, SECRET_LIKE_ENV, SandboxBackend, SandboxError,
    SandboxLimits, SandboxPolicy, SessionProcess, derive_policy, derive_policy_with,
    env_name_is_secret_like,
};
#[cfg(feature = "dev-sandbox-none")]
pub use none::NoneBackend;
pub use session::JsonRpcSession;
pub use tools::{base_tools, tool_decls};
