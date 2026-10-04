//! [`ProcessTool`]: one tool of an out-of-process extension, as a `kernel::Tool` (ADR-0001).
//!
//! The wire is the stdio subset of MCP (`docs/specs/extension-manifest.md` §3): newline-delimited
//! JSON-RPC 2.0, `tools/call {name, arguments}` → `{content: [{type: "text", text}],
//! structuredContent?, isError?}`. Both kinds use the existing sandbox launchers, so policy,
//! scrubbed environment, timeout and SIGTERM → SIGKILL cancellation (D10, D15) are theirs:
//!
//! - `Stateless`: `SandboxBackend::launch_stateless` with the request line on stdin; the process
//!   answers and exits when stdin closes. One process per call.
//! - `Session`: the kernel launches `session_command()` lazily under this tool's derived policy
//!   (`kernel-interface.md` §7.9); `invoke` sends `tools/call` over `ToolContext::session_process`.

use std::path::PathBuf;

use async_trait::async_trait;
use kernel::capability::Capability;
use kernel::content::ContentBlock;
use kernel::host::Command;
use kernel::sandbox::{RpcRequest, RpcResponse, SandboxError};
use kernel::tool::{Tool, ToolContext, ToolError, ToolKind, ToolResult};
use serde_json::{Value, json};

use crate::manifest::{Manifest, ToolSpec};

/// The JSON-RPC method every out-of-process tool call uses.
pub const CALL_METHOD: &str = "tools/call";

/// One tool served by an out-of-process extension. Built by [`crate::loader::admit`] only, so
/// every instance has passed the grant check.
#[derive(Clone, Debug)]
pub struct ProcessTool {
    name: String,
    local_name: String,
    extension: String,
    description: String,
    schema: Value,
    kind: ToolKind,
    capabilities: Vec<Capability>,
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
}

impl ProcessTool {
    pub(crate) fn new(manifest: &Manifest, spec: &ToolSpec) -> ProcessTool {
        ProcessTool {
            name: manifest.tool_name(&spec.name),
            local_name: spec.name.clone(),
            extension: manifest.name.clone(),
            description: spec.description.clone(),
            schema: spec.input_schema.clone(),
            kind: manifest.kind,
            capabilities: manifest.capabilities.clone(),
            program: manifest.command[0].clone(),
            args: manifest.command[1..].to_vec(),
            cwd: manifest.cwd.clone(),
        }
    }

    /// The extension this tool belongs to.
    pub fn extension(&self) -> &str {
        &self.extension
    }

    /// The name the extension process knows the tool by (`tools/call` `name`).
    pub fn local_name(&self) -> &str {
        &self.local_name
    }

    fn command(&self) -> Command {
        Command {
            program: self.program.clone(),
            args: self.args.clone(),
            cwd: Some(self.cwd.clone()),
            env: Default::default(),
            stdin: None,
        }
    }

    fn params(&self, input: Value) -> Value {
        json!({ "name": self.local_name, "arguments": input })
    }

    async fn call_stateless(
        &self,
        ctx: &ToolContext<'_>,
        input: Value,
    ) -> Result<ToolResult, ToolError> {
        let mut line = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": CALL_METHOD,
            "params": self.params(input),
        }))
        .map_err(|e| ToolError::Internal(Box::new(e)))?;
        line.push(b'\n');
        let cmd = Command {
            stdin: Some(line),
            ..self.command()
        };
        let out = ctx
            .sandbox
            .launch_stateless(ctx.policy, cmd, ctx.cancel.clone())
            .await
            .map_err(|e| self.sandbox_err(e))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let response = stdout
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
            .find(|v| v.get("id") == Some(&json!(1)) || v.get("error").is_some());
        match response {
            Some(v) => self.result(rpc_response(&v)),
            None => Err(ToolError::Failed(format!(
                "extension `{}` gave no response (exit code {:?}, signal {:?}); stderr: {}",
                self.extension,
                out.exit_code,
                out.signal,
                String::from_utf8_lossy(&out.stderr).trim_end()
            ))),
        }
    }

    fn sandbox_err(&self, e: SandboxError) -> ToolError {
        match e {
            SandboxError::Timeout(d) => ToolError::Timeout(d),
            SandboxError::Cancelled => ToolError::Cancelled,
            SandboxError::Launch(m) if m.contains("program not permitted") => ToolError::Denied(m),
            other => ToolError::Failed(format!("extension `{}`: {other}", self.extension)),
        }
    }

    /// Map a `tools/call` response onto the kernel's result space.
    fn result(&self, resp: RpcResponse) -> Result<ToolResult, ToolError> {
        let value = match resp {
            RpcResponse::Result(v) => v,
            RpcResponse::Error {
                code,
                message,
                data,
            } => {
                return Err(ToolError::Failed(format!(
                    "extension `{}` protocol error {code}: {message}{}",
                    self.extension,
                    data.map(|d| format!(" ({d})")).unwrap_or_default()
                )));
            }
        };
        call_result(&self.extension, value)
    }
}

/// A raw JSON-RPC response object as an `RpcResponse` (stateless path; the session path gets one
/// from `JsonRpcSession`).
fn rpc_response(v: &Value) -> RpcResponse {
    match v.get("error") {
        Some(err) => RpcResponse::Error {
            code: err.get("code").and_then(Value::as_i64).unwrap_or(-32603),
            message: err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            data: err.get("data").filter(|d| !d.is_null()).cloned(),
        },
        None => RpcResponse::Result(v.get("result").cloned().unwrap_or(Value::Null)),
    }
}

/// `CallToolResult` → `ToolResult` (`docs/specs/extension-manifest.md` §3.3):
/// `isError: true` → `ToolError::Failed` with the text (the model sees an `is_error` result);
/// `structuredContent` → `ToolResult::Value`; otherwise the text blocks → `ToolResult::Blocks`.
pub(crate) fn call_result(extension: &str, value: Value) -> Result<ToolResult, ToolError> {
    let malformed = |why: &str| {
        ToolError::Failed(format!(
            "extension `{extension}` returned a malformed tools/call result: {why}"
        ))
    };
    let obj = value
        .as_object()
        .ok_or_else(|| malformed("not an object"))?;
    let mut texts = Vec::new();
    if let Some(content) = obj.get("content") {
        let items = content
            .as_array()
            .ok_or_else(|| malformed("`content` is not an array"))?;
        for item in items {
            match (item.get("type").and_then(Value::as_str), item.get("text")) {
                (Some("text"), Some(Value::String(t))) => texts.push(t.clone()),
                (Some(other), _) => {
                    return Err(malformed(&format!(
                        "content type `{other}` is not supported (text only)"
                    )));
                }
                _ => return Err(malformed("a content item has no `type`")),
            }
        }
    }
    if obj.get("isError").and_then(Value::as_bool) == Some(true) {
        let message = if texts.is_empty() {
            "the tool reported an error".to_owned()
        } else {
            texts.join("\n")
        };
        return Err(ToolError::Failed(message));
    }
    if let Some(structured) = obj.get("structuredContent") {
        return Ok(ToolResult::Value(structured.clone()));
    }
    if texts.is_empty() {
        return Err(malformed("neither `content` nor `structuredContent`"));
    }
    Ok(ToolResult::Blocks(
        texts
            .into_iter()
            .map(|text| ContentBlock::Text { text })
            .collect(),
    ))
}

#[async_trait]
impl Tool for ProcessTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    fn kind(&self) -> ToolKind {
        self.kind
    }

    fn capabilities(&self) -> Vec<Capability> {
        self.capabilities.clone()
    }

    fn session_command(&self) -> Option<Command> {
        (self.kind == ToolKind::Session).then(|| self.command())
    }

    async fn invoke(&self, ctx: &ToolContext<'_>, input: Value) -> Result<ToolResult, ToolError> {
        match self.kind {
            ToolKind::Stateless => self.call_stateless(ctx, input).await,
            ToolKind::Session => {
                let session = ctx.session_process()?;
                let req = RpcRequest {
                    method: CALL_METHOD.to_owned(),
                    params: self.params(input),
                };
                let resp = session
                    .call(req, ctx.cancel.clone())
                    .await
                    .map_err(|e| self.sandbox_err(e))?;
                self.result(resp)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_result_mapping() {
        let ok = call_result("x", json!({"content": [{"type": "text", "text": "hi"}]})).unwrap();
        assert_eq!(
            ok,
            ToolResult::Blocks(vec![ContentBlock::Text { text: "hi".into() }])
        );
        let structured = call_result(
            "x",
            json!({"content": [{"type": "text", "text": "{}"}], "structuredContent": {"n": 3}}),
        )
        .unwrap();
        assert_eq!(structured, ToolResult::Value(json!({"n": 3})));
        let err = call_result(
            "x",
            json!({"content": [{"type": "text", "text": "no such file"}], "isError": true}),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::Failed(m) if m == "no such file"));
        for bad in [
            json!("text"),
            json!({"content": "x"}),
            json!({"content": [{"type": "image", "data": ""}]}),
            json!({}),
        ] {
            assert!(matches!(call_result("x", bad), Err(ToolError::Failed(_))));
        }
    }

    #[test]
    fn protocol_errors_parse() {
        let r = rpc_response(
            &json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "nope"}}),
        );
        assert_eq!(
            r,
            RpcResponse::Error {
                code: -32601,
                message: "nope".into(),
                data: None
            }
        );
    }
}
