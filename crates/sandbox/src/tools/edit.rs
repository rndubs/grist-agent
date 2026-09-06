//! `edit`: in-process exact-string replacement under the Host's `FsPolicy` check (D5).

use std::path::PathBuf;

use async_trait::async_trait;
use kernel::capability::{Capability, FsMode};
use kernel::tool::{Tool, ToolContext, ToolError, ToolKind, ToolResult};
use serde_json::{Value, json};

use super::{fs_cap, host_err, opt_bool, require_str, resolve_path};

/// Replaces an exact string in a file. Declares `fs.rw:<workdir>`.
///
/// Input `{path, old_string, new_string, replace_all?}`. `old_string` must occur **exactly once**
/// unless `replace_all` is true; zero occurrences, or several without `replace_all`, are
/// `ToolError::InvalidInput` and the file is untouched. Reads under `Ro`, writes under `Rw` (both
/// through the Host); output `{"replacements": <n>}`.
pub struct EditTool {
    workdir: PathBuf,
}

impl EditTool {
    /// An `edit` tool for a session rooted at `workdir` (absolute, normalized).
    pub fn new(workdir: impl Into<PathBuf>) -> EditTool {
        EditTool {
            workdir: super::normalize_workdir(workdir),
        }
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace an exact string in a file. `old_string` must match exactly once (include enough \
         surrounding context to make it unique) unless `replace_all` is true."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path; relative paths resolve against the working directory."},
                "old_string": {"type": "string", "description": "Exact text to find. Must be non-empty."},
                "new_string": {"type": "string", "description": "Replacement text."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence instead of requiring exactly one. Default false."}
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Stateless
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![fs_cap(&self.workdir, FsMode::Rw)]
    }

    async fn invoke(&self, ctx: &ToolContext<'_>, input: Value) -> Result<ToolResult, ToolError> {
        let path = resolve_path(&self.workdir, require_str(&input, "path")?);
        let old = require_str(&input, "old_string")?;
        let new = require_str(&input, "new_string")?;
        let replace_all = opt_bool(&input, "replace_all")?.unwrap_or(false);
        if old.is_empty() {
            return Err(ToolError::InvalidInput(
                "`old_string` must not be empty".into(),
            ));
        }

        let fs = ctx.policy.fs();
        let bytes = ctx.host.read_file(&fs, &path).await.map_err(host_err)?;
        let text = String::from_utf8(bytes).map_err(|_| {
            ToolError::Failed(format!("{} is not valid UTF-8 text", path.display()))
        })?;

        let count = text.matches(old).count();
        if count == 0 {
            return Err(ToolError::InvalidInput(format!(
                "`old_string` was not found in {}",
                path.display()
            )));
        }
        if count > 1 && !replace_all {
            return Err(ToolError::InvalidInput(format!(
                "`old_string` occurs {count} times in {}; add more context to make it unique or set `replace_all`",
                path.display()
            )));
        }
        let (replaced, n) = if replace_all {
            (text.replace(old, new), count)
        } else {
            (text.replacen(old, new, 1), 1)
        };
        ctx.host
            .write_file(&fs, &path, replaced.as_bytes())
            .await
            .map_err(host_err)?;
        Ok(ToolResult::Value(json!({ "replacements": n })))
    }
}
