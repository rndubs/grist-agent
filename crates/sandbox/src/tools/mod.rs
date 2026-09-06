//! The six base tools (`profile-schema.md` §3.11), each a `kernel::Tool` constructed with the
//! session `workdir` so its declared capabilities are concrete:
//!
//! | Tool | Kind (D5) | Declares | Returns |
//! |---|---|---|---|
//! | [`read`] | Stateless, in-process (Host policy) | `fs.ro:<workdir>` | `Value` |
//! | [`write`] | Stateless, in-process | `fs.rw:<workdir>` | `Value` |
//! | [`edit`] | Stateless, in-process | `fs.rw:<workdir>` | `Value` |
//! | [`bash`] | Stateless, sandboxed | `fs.rw:<workdir>`, `proc:bash` | `Value` |
//! | [`run_script`] | Stateless, sandboxed | `fs.rw:<workdir>`, `proc:bash` | `Task` (D1) |
//! | [`python`] | Session, sandboxed | `fs.rw:<workdir>`, `proc:python3` | `Value` |
//!
//! Descriptions are defaults; a model profile may override the phrasing (`profile-schema.md`
//! §2.4). Schemas are JSON Schema (draft 2020-12) objects and are never overridden.
//!
//! Relative `path`/`cwd` inputs resolve against the workdir; absolute ones are used as given. The
//! `Host` (in-process tools) or the sandbox mounts (out-of-process tools) enforce the policy —
//! tools never check paths themselves.

pub mod bash;
pub mod edit;
pub mod python;
pub mod read;
pub mod run_script;
pub mod write;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use kernel::capability::{Capability, FsMode};
use kernel::host::{HostError, ProcessOutput};
use kernel::sandbox::{SandboxBackend, SandboxError};
use kernel::tool::{Tool, ToolError, ToolKind};
use serde_json::{Value, json};

pub use bash::BashTool;
pub use edit::EditTool;
pub use python::{PythonTool, REPL_SERVER_SOURCE};
pub use read::ReadTool;
pub use run_script::RunScriptTool;
pub use write::WriteTool;

/// All six base tools for a session at `workdir`. `sandbox` is the backend the kernel will also
/// place in `ToolContext`; `run_script` needs an owned handle because its task future must be
/// `'static` (see [`RunScriptTool`]).
pub fn base_tools(
    workdir: impl Into<PathBuf>,
    sandbox: Arc<dyn SandboxBackend>,
) -> Vec<Arc<dyn Tool>> {
    let workdir = normalize_workdir(workdir);
    vec![
        Arc::new(ReadTool::new(workdir.clone())),
        Arc::new(WriteTool::new(workdir.clone())),
        Arc::new(EditTool::new(workdir.clone())),
        Arc::new(BashTool::new(workdir.clone())),
        Arc::new(RunScriptTool::new(workdir.clone(), sandbox)),
        Arc::new(PythonTool::new(workdir)),
    ]
}

/// `(name, kind, capabilities)` for every base tool, for the profiles validator
/// (`profile-schema.md` §3.3) — no backend needed.
pub fn tool_decls(workdir: impl Into<PathBuf>) -> Vec<(String, ToolKind, Vec<Capability>)> {
    let workdir = normalize_workdir(workdir);
    let decl = |t: &dyn Tool| (t.name().to_owned(), t.kind(), t.capabilities());
    vec![
        decl(&ReadTool::new(workdir.clone())),
        decl(&WriteTool::new(workdir.clone())),
        decl(&EditTool::new(workdir.clone())),
        decl(&BashTool::new(workdir.clone())),
        decl(&run_script::decl(workdir.clone())),
        decl(&PythonTool::new(workdir)),
    ]
}

/// Absolute + lexically normalized (`.` and `..` collapsed). A relative workdir is resolved against
/// the process cwd so the declared `fs.*` atoms are always absolute, as `Capability::Fs` requires.
pub(crate) fn normalize_workdir(workdir: impl Into<PathBuf>) -> PathBuf {
    let workdir = workdir.into();
    let abs = if workdir.is_absolute() {
        workdir
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&workdir))
            .unwrap_or(workdir)
    };
    lexical_normalize(&abs)
}

/// Resolve a tool `path` input: absolute as given, relative against `workdir`; `.`/`..` collapsed.
/// Symlinks are the Host's business (§3.9: it resolves them before the policy check).
pub(crate) fn resolve_path(workdir: &Path, input: &str) -> PathBuf {
    let p = Path::new(input);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        workdir.join(p)
    };
    lexical_normalize(&joined)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::Normal(seg) => out.push(seg),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

pub(crate) fn fs_cap(workdir: &Path, mode: FsMode) -> Capability {
    Capability::Fs {
        path: workdir.to_path_buf(),
        mode,
    }
}

pub(crate) fn proc_cap(program: &str) -> Capability {
    Capability::Proc {
        program: program.to_owned(),
    }
}

pub(crate) fn require_str<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    match input.get(key) {
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(ToolError::InvalidInput(format!("`{key}` must be a string"))),
        None => Err(ToolError::InvalidInput(format!("`{key}` is required"))),
    }
}

pub(crate) fn opt_str<'a>(input: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(ToolError::InvalidInput(format!("`{key}` must be a string"))),
    }
}

pub(crate) fn opt_u64(input: &Value, key: &str) -> Result<Option<u64>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| {
            ToolError::InvalidInput(format!("`{key}` must be a non-negative integer"))
        }),
    }
}

pub(crate) fn opt_bool(input: &Value, key: &str) -> Result<Option<bool>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(ToolError::InvalidInput(format!(
            "`{key}` must be a boolean"
        ))),
    }
}

pub(crate) fn opt_str_list(input: &Value, key: &str) -> Result<Vec<String>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str().map(str::to_owned).ok_or_else(|| {
                    ToolError::InvalidInput(format!("`{key}` must be an array of strings"))
                })
            })
            .collect(),
        Some(_) => Err(ToolError::InvalidInput(format!(
            "`{key}` must be an array of strings"
        ))),
    }
}

/// `HostError` → `ToolError` for in-process tools: `Denied` stays a policy denial.
pub(crate) fn host_err(e: HostError) -> ToolError {
    match e {
        HostError::Denied(p) => ToolError::Denied(p.to_string()),
        HostError::Cancelled => ToolError::Cancelled,
        HostError::NotFound(p) => ToolError::Failed(format!("not found: {p}")),
        other => ToolError::Failed(other.to_string()),
    }
}

/// `SandboxError` → `ToolError` for sandboxed tools.
pub(crate) fn sandbox_err(e: SandboxError) -> ToolError {
    match e {
        SandboxError::Timeout(d) => ToolError::Timeout(d),
        SandboxError::Cancelled => ToolError::Cancelled,
        SandboxError::Launch(m) if m.contains("program not permitted") => ToolError::Denied(m),
        other => ToolError::Failed(other.to_string()),
    }
}

/// The JSON the model sees for a finished process. stdout/stderr are lossy UTF-8; the kernel
/// spills oversized results (§7.4).
pub(crate) fn output_json(out: &ProcessOutput) -> Value {
    json!({
        "exit_code": out.exit_code,
        "signal": out.signal,
        "stdout": String::from_utf8_lossy(&out.stdout),
        "stderr": String::from_utf8_lossy(&out.stderr),
        "timed_out": out.timed_out,
        "duration_ms": out.duration.as_millis() as u64,
    })
}
