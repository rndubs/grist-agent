//! `write`: in-process file write under the Host's `FsPolicy` check (D5).

use std::path::PathBuf;

use async_trait::async_trait;
use kernel::capability::{Capability, FsMode};
use kernel::tool::{Tool, ToolContext, ToolError, ToolKind, ToolResult};
use serde_json::{Value, json};

use super::{fs_cap, host_err, require_str, resolve_path};

/// Creates or truncates a file with the given content. Declares `fs.rw:<workdir>`.
///
/// Input `{path, content}`; output `{"bytes": <bytes written>}`. A policy refusal (a path
/// outside the tool's `Rw` mounts) is `ToolError::Denied`.
pub struct WriteTool {
    workdir: PathBuf,
}

impl WriteTool {
    /// A `write` tool for a session rooted at `workdir` (absolute, normalized).
    pub fn new(workdir: impl Into<PathBuf>) -> WriteTool {
        WriteTool {
            workdir: super::normalize_workdir(workdir),
        }
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write a file in the working directory, creating it or replacing its whole content."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path; relative paths resolve against the working directory."},
                "content": {"type": "string", "description": "The complete new content of the file."}
            },
            "required": ["path", "content"],
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
        let content = require_str(&input, "content")?;
        ctx.host
            .write_file(&ctx.policy.fs(), &path, content.as_bytes())
            .await
            .map_err(host_err)?;
        Ok(ToolResult::Value(json!({ "bytes": content.len() })))
    }
}
