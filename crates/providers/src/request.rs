//! Rendering a `kernel::ModelRequest` into an OpenAI-compatible `/v1/chat/completions` body.
//!
//! Mapping (see `crates/providers/README.md` for the table):
//!
//! - `req.system` blocks are joined with `"\n\n"` into one `system` message (omitted when empty).
//!   With `ToolFormat::Parsed(_)` the tool definitions are appended to that message as a
//!   documented block and `tools`/`tool_choice` are not sent.
//! - `Role::User`: `Text` → text part, `Image` → `image_url` data URL built from the artifact
//!   bytes at request time (D15), `TaskResult` → a text part holding the JSON of the block
//!   (`kernel-interface.md` §3.3). A message with exactly one text-like part is sent as a
//!   plain string for maximum compatibility; anything else as a parts array.
//! - `Role::Assistant`: `Text` → `content`, `ToolUse` → `tool_calls[]`, `Thinking` → the field
//!   named by `reasoning_field` (`reasoning_content` / `reasoning`; `provider_specific_fields`
//!   is written back as `reasoning_content` since that is what LiteLLM accepts on input) plus
//!   `thinking_blocks` when a signature is present; dropped for `None` / `InlineThink`.
//! - `Role::Tool`: one `role: tool` message per `ToolResult`, `content` = the JSON string of
//!   `ToolResultContent::Json` or the concatenated text of `Blocks` (images in tool results
//!   cannot be expressed on this wire and are replaced by a placeholder line).
//! - `params`: `temperature`, `top_p`, `max_tokens`, `stop`; `thinking.enabled` →
//!   `thinking: {type: "enabled", budget_tokens}` (LiteLLM's shape; other endpoints ignore or
//!   drop it); `extra` merged into the body last when it is an object, so it can override
//!   anything above (e.g. `tool_choice`, `response_format`).
//! - `tool_choice` is not sent: the OpenAI default is `auto` when `tools` is present.

use std::sync::Arc;

use kernel::{
    ArtifactError, ArtifactStore, ContentBlock, Message, ModelRequest, ProviderError, Role,
    ToolDefinition, ToolResultContent,
};
use serde_json::{Map, Value, json};

use crate::quirks::{Quirks, ReasoningField, ToolFormat};

/// Render the JSON body for `req`. Async because image bytes are fetched from the artifact store.
pub async fn render_body(
    req: &ModelRequest,
    quirks: &Quirks,
    artifacts: &Arc<dyn ArtifactStore>,
) -> Result<Value, ProviderError> {
    let mut messages: Vec<Value> = Vec::new();

    let mut system = req.system_text();
    if let ToolFormat::Parsed(syntax) = &quirks.tool_format
        && !req.tools.is_empty()
    {
        if !system.is_empty() {
            system.push_str("\n\n");
        }
        system.push_str(&render_tools_block(syntax, &req.tools));
    }
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system}));
    }

    for m in &req.messages {
        render_message(m, quirks, artifacts, &mut messages).await?;
    }

    let mut body = Map::new();
    body.insert("model".into(), Value::String(req.model_id.clone()));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), Value::Bool(true));
    body.insert("stream_options".into(), json!({"include_usage": true}));

    if matches!(quirks.tool_format, ToolFormat::Native) && !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| render_tool(t, quirks.strict_tool_schema))
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }

    let p = &req.params;
    if let Some(t) = p.temperature {
        body.insert("temperature".into(), json!(t));
    }
    if let Some(t) = p.top_p {
        body.insert("top_p".into(), json!(t));
    }
    if let Some(n) = p.max_tokens {
        body.insert("max_tokens".into(), json!(n));
    }
    if !p.stop.is_empty() {
        body.insert("stop".into(), json!(p.stop));
    }
    if let Some(th) = &p.thinking
        && th.enabled
    {
        let mut t = Map::new();
        t.insert("type".into(), Value::String("enabled".into()));
        if let Some(b) = th.budget_tokens {
            t.insert("budget_tokens".into(), json!(b));
        }
        body.insert("thinking".into(), Value::Object(t));
    }
    if let Value::Object(extra) = &p.extra {
        for (k, v) in extra {
            body.insert(k.clone(), v.clone());
        }
    }
    Ok(Value::Object(body))
}

/// One OpenAI `tools[]` entry. With `strict`, `strict: true` is set and every object schema node
/// gets `additionalProperties: false`.
pub fn render_tool(t: &ToolDefinition, strict: bool) -> Value {
    let mut parameters = t.input_schema.clone();
    if parameters.is_null() {
        parameters = json!({"type": "object", "properties": {}});
    }
    if strict {
        close_object_schemas(&mut parameters);
    }
    let mut function = Map::new();
    function.insert("name".into(), Value::String(t.name.clone()));
    function.insert("description".into(), Value::String(t.description.clone()));
    function.insert("parameters".into(), parameters);
    if strict {
        function.insert("strict".into(), Value::Bool(true));
    }
    json!({"type": "function", "function": Value::Object(function)})
}

fn close_object_schemas(schema: &mut Value) {
    let Value::Object(o) = schema else {
        return;
    };
    let is_object =
        o.get("type").and_then(Value::as_str) == Some("object") || o.contains_key("properties");
    if is_object && !o.contains_key("additionalProperties") {
        o.insert("additionalProperties".into(), Value::Bool(false));
    }
    for (k, v) in o.iter_mut() {
        // Descend into schema-bearing keywords only; never into `enum`/`const`/`default`.
        match k.as_str() {
            "properties" | "$defs" | "definitions" | "patternProperties" => {
                if let Value::Object(children) = v {
                    for child in children.values_mut() {
                        close_object_schemas(child);
                    }
                }
            }
            "items" | "additionalProperties" | "not" | "if" | "then" | "else" => {
                close_object_schemas(v)
            }
            "anyOf" | "oneOf" | "allOf" | "prefixItems" => {
                if let Value::Array(children) = v {
                    for child in children {
                        close_object_schemas(child);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The system-prompt block advertising tools for a `parsed:<syntax>` endpoint. The wording for
/// `hermes` mirrors the Hermes-2-Pro / Qwen chat template; any other syntax gets the same JSON
/// listing with a neutral instruction naming the syntax (the parser middleware owns the details).
pub fn render_tools_block(syntax: &str, tools: &[ToolDefinition]) -> String {
    let mut s = String::from(
        "# Tools\n\nYou may call one or more functions to assist with the user query.\n\n\
         You are provided with function signatures within <tools></tools> XML tags:\n<tools>\n",
    );
    for t in tools {
        s.push_str(&render_tool(t, false).to_string());
        s.push('\n');
    }
    s.push_str("</tools>\n\n");
    if syntax == "hermes" {
        s.push_str(
            "For each function call, return a json object with function name and arguments \
             within <tool_call></tool_call> XML tags:\n<tool_call>\n{\"name\": <function-name>, \
             \"arguments\": <args-json-object>}\n</tool_call>",
        );
    } else {
        s.push_str(&format!(
            "Emit function calls in the `{syntax}` tool-call syntax expected by this model."
        ));
    }
    s
}

async fn render_message(
    m: &Message,
    quirks: &Quirks,
    artifacts: &Arc<dyn ArtifactStore>,
    out: &mut Vec<Value>,
) -> Result<(), ProviderError> {
    match m.role {
        Role::System => {
            // Never in `State.messages` (§3.3); rendered as a system message if it ever shows up.
            let text = text_of(&m.content);
            out.push(json!({"role": "system", "content": text}));
        }
        Role::User => {
            let mut parts: Vec<Value> = Vec::new();
            for b in &m.content {
                match b {
                    ContentBlock::Text { text } => {
                        parts.push(json!({"type": "text", "text": text}));
                    }
                    ContentBlock::Image {
                        artifact_handle,
                        mime,
                    } => {
                        let bytes = artifacts
                            .get(artifact_handle)
                            .await
                            .map_err(|e| artifact_error(&artifact_handle.0.to_string(), e))?;
                        let url = data_url(mime, &bytes);
                        parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                    }
                    ContentBlock::TaskResult { .. } => {
                        let text = serde_json::to_string(b)
                            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;
                        parts.push(json!({"type": "text", "text": text}));
                    }
                    other => {
                        // ToolUse/ToolResult/Thinking never appear in user messages (§3.3);
                        // degrade to text rather than fail the turn.
                        parts.push(json!({"type": "text", "text": block_text(other)}));
                    }
                }
            }
            let content = match parts.as_slice() {
                [single] if single.get("type").and_then(Value::as_str) == Some("text") => {
                    single["text"].clone()
                }
                _ => Value::Array(parts),
            };
            out.push(json!({"role": "user", "content": content}));
        }
        Role::Assistant => {
            let mut text = String::new();
            let mut thinking = String::new();
            let mut signature: Option<String> = None;
            let mut tool_calls: Vec<Value> = Vec::new();
            for b in &m.content {
                match b {
                    ContentBlock::Text { text: t } => text.push_str(t),
                    ContentBlock::Thinking {
                        text: t,
                        signature: sig,
                    } => {
                        thinking.push_str(t);
                        if sig.is_some() {
                            signature = sig.clone();
                        }
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        let arguments = serde_json::to_string(input)
                            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": arguments}
                        }));
                    }
                    other => text.push_str(&block_text(other)),
                }
            }
            let mut msg = Map::new();
            msg.insert("role".into(), Value::String("assistant".into()));
            msg.insert(
                "content".into(),
                if text.is_empty() && !tool_calls.is_empty() {
                    Value::Null
                } else {
                    Value::String(text)
                },
            );
            if !thinking.is_empty() || signature.is_some() {
                let field = match quirks.reasoning_field {
                    ReasoningField::ReasoningContent | ReasoningField::ProviderSpecificFields => {
                        Some("reasoning_content")
                    }
                    ReasoningField::Reasoning => Some("reasoning"),
                    ReasoningField::None | ReasoningField::InlineThink => None,
                };
                if let Some(field) = field {
                    msg.insert(field.into(), Value::String(thinking.clone()));
                    if let Some(sig) = &signature {
                        msg.insert(
                            "thinking_blocks".into(),
                            json!([{"type": "thinking", "thinking": thinking, "signature": sig}]),
                        );
                    }
                }
            }
            if !tool_calls.is_empty() {
                msg.insert("tool_calls".into(), Value::Array(tool_calls));
            }
            out.push(Value::Object(msg));
        }
        Role::Tool => {
            for b in &m.content {
                match b {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        let text = tool_result_text(content)?;
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": text
                        }));
                    }
                    other => {
                        // Not allowed by §3.3; keep the conversation well-formed anyway.
                        out.push(json!({"role": "user", "content": block_text(other)}));
                    }
                }
            }
        }
    }
    Ok(())
}

/// `content` of a `role: tool` message: the JSON string for `Json`, the concatenated text of
/// `Blocks` otherwise.
pub fn tool_result_text(content: &ToolResultContent) -> Result<String, ProviderError> {
    match content {
        ToolResultContent::Json(v) => match v {
            Value::String(s) => Ok(s.clone()),
            other => serde_json::to_string(other)
                .map_err(|e| ProviderError::InvalidResponse(e.to_string())),
        },
        ToolResultContent::Blocks(blocks) => Ok(text_of(blocks)),
    }
}

fn text_of(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .map(block_text)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn block_text(b: &ContentBlock) -> String {
    match b {
        ContentBlock::Text { text } => text.clone(),
        ContentBlock::Thinking { text, .. } => text.clone(),
        ContentBlock::Image { mime, .. } => format!("[image: {mime}]"),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("data:{mime};base64,{b64}")
}

fn artifact_error(handle: &str, e: ArtifactError) -> ProviderError {
    match e {
        // Transient store I/O is worth a retry; a missing artifact is not.
        ArtifactError::Io(msg) => {
            ProviderError::Transport(format!("image artifact {handle}: io error: {msg}"))
        }
        other => ProviderError::Client {
            status: 0,
            message: format!("image artifact {handle} unavailable: {other}"),
        },
    }
}
