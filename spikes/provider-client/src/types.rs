//! Normalized response shape (what P1.5's `Provider::complete` would return)
//! plus the observations the probe uses to fill in a quirk row.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

/// The normalized response. Tests assert this is identical across every
/// upstream shape for the same logical answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Response {
    pub thinking: Option<String>,
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
}

/// Raw facts observed while parsing, used to *derive* quirk flags. Not part
/// of the normalized response; not compared in tests except where noted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Observed {
    pub streamed: bool,
    pub finish_reason: Option<String>,
    /// Which reasoning locations carried text (`reasoning_content`, `reasoning`,
    /// `provider_specific_fields.reasoning_content`, `inline_think`).
    pub reasoning_fields_seen: Vec<String>,
    /// Streaming only: a chunk carrying `usage` arrived (requested via stream_options).
    pub usage_in_final_chunk: bool,
    /// Streaming only: that usage chunk had `choices: []` (vLLM, llama.cpp) rather
    /// than `choices: [{index:0, delta:{}}]` (LiteLLM).
    pub usage_chunk_choices_empty: bool,
    /// Streaming only: number of chunks that carried a `tool_calls` delta.
    pub tool_call_delta_chunks: usize,
    /// Streaming only: number of SSE data events (excluding `[DONE]`).
    pub chunks: usize,
    /// Tool calls were found only by parsing text (parsed syntax), not natively.
    pub tool_calls_parsed_from_text: bool,
    pub http_status: u16,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Completion {
    pub response: Response,
    pub observed: Observed,
}

/// Messages in OpenAI wire format; we build them as raw JSON to keep the spike small.
pub type Message = Value;

pub fn system(text: &str) -> Message {
    serde_json::json!({"role": "system", "content": text})
}
pub fn user(text: &str) -> Message {
    serde_json::json!({"role": "user", "content": text})
}
pub fn assistant_with_tool_calls(text: &str, calls: &[ToolCall]) -> Message {
    let tool_calls: Vec<Value> = calls
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "type": "function",
                "function": {"name": c.name, "arguments": c.arguments.to_string()}
            })
        })
        .collect();
    let mut m = serde_json::json!({"role": "assistant", "tool_calls": tool_calls});
    if !text.is_empty() {
        m["content"] = Value::String(text.to_string());
    }
    m
}
pub fn tool_result(call_id: &str, content: &str) -> Message {
    serde_json::json!({"role": "tool", "tool_call_id": call_id, "content": content})
}

/// A tool definition in OpenAI `tools` format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

pub fn weather_tool() -> ToolDef {
    ToolDef {
        name: "get_weather".into(),
        description: "Get the current weather for a city.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "city": {"type": "string", "description": "City name"},
                "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]}
            },
            "required": ["city"],
            "additionalProperties": false
        }),
    }
}
