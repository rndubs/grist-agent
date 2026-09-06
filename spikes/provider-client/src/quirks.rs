//! Per-endpoint quirk flags. This struct is the spike's real output: the
//! table in `docs/spikes/providers.md` is one row of it per endpoint.

use serde::{Deserialize, Serialize};

/// Where the model's reasoning/thinking text shows up in a chat completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningField {
    /// Endpoint never sends reasoning (or the model has none).
    None,
    /// `message.reasoning_content` / `delta.reasoning_content` (vLLM, LiteLLM, llama.cpp).
    ReasoningContent,
    /// `message.reasoning` / `delta.reasoning` (OpenRouter, some vLLM parsers).
    Reasoning,
    /// LiteLLM sometimes only carries it under `provider_specific_fields.reasoning_content`.
    ProviderSpecificFields,
    /// No reasoning parser on the server: `<think>...</think>` arrives inline in `content`.
    InlineThink,
    /// Probe mode: accept any of the above and record which one was seen.
    #[default]
    Auto,
}

/// How tool calls come back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolFormat {
    /// Server parses tool calls and returns the OpenAI `tool_calls` array.
    #[default]
    Native,
    /// Server has no tool-call parser; the model emits a text syntax that we parse.
    Parsed(ParsedSyntax),
}

/// Text syntaxes our parser middleware understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParsedSyntax {
    /// `<tool_call>{"name": ..., "arguments": {...}}</tool_call>` (Hermes / Qwen).
    Hermes,
}

/// Three-valued capability flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Tri {
    Yes,
    No,
    #[default]
    Unknown,
}

/// How to authenticate. The API key itself is never stored here: only the
/// name of the environment variable, resolved at request time (D10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Auth {
    #[default]
    None,
    Bearer {
        env: String,
    },
}

/// The quirk flags. Every field has a conservative default so a row can be
/// filled in incrementally by the probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Quirks {
    pub reasoning_field: ReasoningField,
    pub tool_format: ToolFormat,
    pub supports_structured_output: Tri,
    /// Send `stream_options: {include_usage: true}` and expect a final usage chunk.
    pub supports_stream_usage: bool,
    pub auth: Auth,
    /// Add `"strict": true` to each function definition (OpenAI structured tools).
    pub strict_tool_schema: bool,
    /// Whether `finish_reason: "tool_calls"` is sent when tool calls are present.
    /// When false the client must detect tool calls from the payload alone.
    pub sends_finish_reason_tool_calls: bool,
    /// Streaming tool calls arrive as many index-keyed fragments (true) or as one
    /// complete chunk per call (false, llama.cpp). The accumulator handles both;
    /// the flag exists so the row documents what the endpoint actually does.
    pub streams_tool_call_fragments: bool,
}

impl Default for Quirks {
    fn default() -> Self {
        Self {
            reasoning_field: ReasoningField::Auto,
            tool_format: ToolFormat::Native,
            supports_structured_output: Tri::Unknown,
            supports_stream_usage: true,
            auth: Auth::None,
            strict_tool_schema: false,
            sends_finish_reason_tool_calls: true,
            streams_tool_call_fragments: true,
        }
    }
}

impl Quirks {
    /// Presets. These are the *expected* rows (from upstream docs); the probe
    /// verifies them and prints the observed row next to them.
    pub fn preset(name: &str) -> Option<Self> {
        Some(match name {
            "vllm" => Self {
                reasoning_field: ReasoningField::ReasoningContent,
                tool_format: ToolFormat::Native,
                supports_structured_output: Tri::Yes,
                supports_stream_usage: true,
                auth: Auth::None,
                strict_tool_schema: false,
                sends_finish_reason_tool_calls: true,
                streams_tool_call_fragments: true,
            },
            "litellm" => Self {
                reasoning_field: ReasoningField::ReasoningContent,
                tool_format: ToolFormat::Native,
                supports_structured_output: Tri::Unknown, // upstream-dependent
                supports_stream_usage: true,
                auth: Auth::Bearer {
                    env: "GRIST_LITELLM_API_KEY".into(),
                },
                strict_tool_schema: false,
                sends_finish_reason_tool_calls: true,
                streams_tool_call_fragments: true,
            },
            "llamacpp" => Self {
                reasoning_field: ReasoningField::ReasoningContent,
                tool_format: ToolFormat::Native,
                supports_structured_output: Tri::Yes,
                supports_stream_usage: true,
                auth: Auth::None,
                strict_tool_schema: false,
                sends_finish_reason_tool_calls: true,
                streams_tool_call_fragments: false,
            },
            "hermes" => Self {
                reasoning_field: ReasoningField::InlineThink,
                tool_format: ToolFormat::Parsed(ParsedSyntax::Hermes),
                supports_structured_output: Tri::No,
                supports_stream_usage: true,
                auth: Auth::None,
                strict_tool_schema: false,
                sends_finish_reason_tool_calls: false,
                streams_tool_call_fragments: false,
            },
            "probe" | "generic" => Self::default(),
            _ => return None,
        })
    }

    /// One markdown table row (no leading header). Used by `probe`.
    pub fn markdown_cells(&self) -> Vec<String> {
        let snake = |v: &dyn erased::Ser| v.snake();
        vec![
            snake(&self.reasoning_field),
            match &self.tool_format {
                ToolFormat::Native => "native".to_string(),
                ToolFormat::Parsed(s) => format!("parsed({})", erased::Ser::snake(s)),
            },
            snake(&self.supports_structured_output),
            self.supports_stream_usage.to_string(),
            match &self.auth {
                Auth::None => "none".to_string(),
                Auth::Bearer { env } => format!("bearer(${env})"),
            },
            self.strict_tool_schema.to_string(),
            self.sends_finish_reason_tool_calls.to_string(),
            self.streams_tool_call_fragments.to_string(),
        ]
    }

    pub const MARKDOWN_HEADER: [&'static str; 8] = [
        "reasoning_field",
        "tool_format",
        "supports_structured_output",
        "supports_stream_usage",
        "auth",
        "strict_tool_schema",
        "sends_finish_reason_tool_calls",
        "streams_tool_call_fragments",
    ];
}

/// Tiny helper so table cells use the same snake_case names as the TOML config.
mod erased {
    pub trait Ser {
        fn snake(&self) -> String;
    }
    impl<T: serde::Serialize> Ser for T {
        fn snake(&self) -> String {
            match serde_json::to_value(self) {
                Ok(serde_json::Value::String(s)) => s,
                Ok(v) => v.to_string(),
                Err(_) => "?".into(),
            }
        }
    }
}
