//! The scrubbed environment (D10, §7.5) and the launcher-level program check (§3.12), shared by
//! every backend. Both are pure given an environment lookup.

use std::collections::BTreeMap;
use std::path::Path;

use kernel::host::Command;
use kernel::sandbox::{SandboxError, SandboxPolicy, env_name_is_secret_like};

/// `HOME` inside every sandbox (§3.12: "`HOME=/tmp` is always set by the launcher").
pub const SANDBOX_HOME: &str = "/tmp";

/// The complete environment a sandboxed process receives, computed from the kernel process
/// environment via [`std::env::var`]. See [`scrubbed_env_with`] for the rules.
pub fn scrubbed_env(
    policy: &SandboxPolicy,
    cmd: &Command,
) -> Result<BTreeMap<String, String>, SandboxError> {
    scrubbed_env_with(policy, cmd, |name| std::env::var(name).ok())
}

/// Pure form of [`scrubbed_env`] with an explicit environment lookup.
///
/// Rules (D10, §7.5):
/// 1. For every name in `policy.env_allowlist` that `lookup` resolves, copy it. A name matching
///    [`kernel::sandbox::SECRET_LIKE_ENV`] is refused outright with `SandboxError::Launch` — the
///    derivation already rejects such names, so reaching this is a bug upstream, and the launcher
///    is the last line of defense.
/// 2. Add the tool's own explicit `cmd.env` entries (the `Command` doc says env is "exactly `env`";
///    these are the tool's deliberate values, not inherited, so they are passed through as given).
/// 3. Force `HOME=/tmp` last; it wins over both the allowlist and `cmd.env`.
///
/// Nothing else reaches the sandbox: no other inherited variable, ever.
pub fn scrubbed_env_with(
    policy: &SandboxPolicy,
    cmd: &Command,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<BTreeMap<String, String>, SandboxError> {
    let mut env = BTreeMap::new();
    for name in &policy.env_allowlist {
        if env_name_is_secret_like(name) {
            return Err(SandboxError::Launch(format!(
                "environment variable `{name}` looks like a secret and cannot be allowlisted"
            )));
        }
        if let Some(value) = lookup(name) {
            env.insert(name.clone(), value);
        }
    }
    for (name, value) in &cmd.env {
        env.insert(name.clone(), value.clone());
    }
    env.insert("HOME".to_owned(), SANDBOX_HOME.to_owned());
    Ok(env)
}

/// The launcher-level argv\[0\] check (§3.12: "enforcement of the program list is a P1.7 launcher
/// concern"). An empty `policy.programs` permits anything on the mounted `PATH`. Otherwise
/// `cmd.program` is permitted iff the set contains it verbatim (name or absolute path) or
/// contains its file name (so `proc:bash` permits `/usr/bin/bash`, and `proc:/usr/bin/bash`
/// permits exactly that path).
pub fn check_program(policy: &SandboxPolicy, cmd: &Command) -> Result<(), SandboxError> {
    if policy.programs.is_empty() || policy.programs.contains(&cmd.program) {
        return Ok(());
    }
    let basename = Path::new(&cmd.program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if !basename.is_empty() && policy.programs.contains(basename) {
        return Ok(());
    }
    Err(SandboxError::Launch(format!(
        "program not permitted: `{}` is not in {:?}",
        cmd.program, policy.programs
    )))
}
