//! `read`: in-process file read under the Host's `FsPolicy` check (D5).

use std::path::PathBuf;

use async_trait::async_trait;
use kernel::capability::{Capability, FsMode};
use kernel::tool::{Tool, ToolContext, ToolError, ToolKind, ToolResult};
use serde_json::{Value, json};

use super::{fs_cap, host_err, opt_u64, require_str, resolve_path};

/// Lines returned when `limit` is absent.
pub const DEFAULT_LIMIT: u64 = 2000;

/// Reads a text file and returns it with line numbers. Declares `fs.ro:<workdir>`.
///
/// Input `{path, offset?, limit?}`: `offset` is a 1-based first line (default 1), `limit` the
/// maximum number of lines (default [`DEFAULT_LIMIT`]). Output `{"content": "<n>\t<line>…",
/// "lines": <total lines in file>, "truncated": <whether lines were left out>}`.
/// Non-UTF-8 content is `ToolError::Failed`; a policy refusal is `ToolError::Denied`.
pub struct ReadTool {
    workdir: PathBuf,
}

impl ReadTool {
    /// A `read` tool for a session rooted at `workdir` (absolute, normalized).
    pub fn new(workdir: impl Into<PathBuf>) -> ReadTool {
        ReadTool {
            workdir: super::normalize_workdir(workdir),
        }
    }
}

/// Number lines `cat -n` style: right-aligned width 6, a tab, the line.
pub fn number_lines(lines: &[&str], first: u64) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        out.push_str(&format!("{:>6}\t{}\n", first + i as u64, line));
    }
    out
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a text file from the working directory and return it with line numbers. \
         Use `offset` (1-based line) and `limit` to page through long files."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path; relative paths resolve against the working directory."},
                "offset": {"type": "integer", "minimum": 1, "description": "First line to return (1-based). Default 1."},
                "limit": {"type": "integer", "minimum": 1, "description": "Maximum number of lines to return. Default 2000."}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Stateless
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![fs_cap(&self.workdir, FsMode::Ro)]
    }

    async fn invoke(&self, ctx: &ToolContext<'_>, input: Value) -> Result<ToolResult, ToolError> {
        let path = resolve_path(&self.workdir, require_str(&input, "path")?);
        let offset = opt_u64(&input, "offset")?.unwrap_or(1);
        if offset == 0 {
            return Err(ToolError::InvalidInput(
                "`offset` is 1-based; 0 is not a line".into(),
            ));
        }
        let limit = opt_u64(&input, "limit")?.unwrap_or(DEFAULT_LIMIT);
        if limit == 0 {
            return Err(ToolError::InvalidInput("`limit` must be at least 1".into()));
        }

        let bytes = ctx
            .host
            .read_file(&ctx.policy.fs(), &path)
            .await
            .map_err(host_err)?;
        let text = String::from_utf8(bytes).map_err(|_| {
            ToolError::Failed(format!("{} is not valid UTF-8 text", path.display()))
        })?;
        let all: Vec<&str> = text.lines().collect();
        let total = all.len() as u64;
        let start = (offset - 1).min(total) as usize;
        let end = (start as u64).saturating_add(limit).min(total) as usize;
        let page = &all[start..end];
        Ok(ToolResult::Value(json!({
            "content": number_lines(page, offset),
            "lines": total,
            "truncated": (page.len() as u64) < total,
        })))
    }
}
