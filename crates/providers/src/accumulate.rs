//! Assembling streamed chunks into `ModelDelta`s and one final `ModelResponse`.
//!
//! Rules (from `docs/spikes/providers.md` §5):
//!
//! 1. `tool_calls` are keyed by `index`; a missing `index` is tolerated (a fragment with an `id`
//!    starts a new call, one without continues the last); `arguments` may arrive as fragments of
//!    a JSON string or as a whole object. `finish_reason` is never consulted to decide whether
//!    tool calls exist.
//! 2. One `Thinking` block per response, taken from the location `reasoning_field` names; the
//!    `signature` of a `thinking_blocks[]` entry (LiteLLM/Anthropic) is carried through.
//! 4. Usage is taken from any chunk that carries a `usage` object, whatever its `choices`.
//!
//! Block indices in the emitted deltas follow arrival order (thinking, text and tool blocks get
//! an index when their first non-empty piece arrives); the final `content` is in the same order.
//! With `ReasoningField::InlineThink` the `<think>…</think>` split happens at the end, so the
//! deltas show the raw text and the final content carries a `Thinking` block before the text.

use std::collections::BTreeMap;

use kernel::{ContentBlock, Hash, ModelDelta, ModelResponse, StopReason, Usage};
use serde_json::Value;

use crate::quirks::ReasoningField;

#[derive(Debug, Default)]
struct ToolAcc {
    block: Option<u32>,
    id: String,
    name: String,
    arguments: String,
    /// Bytes of `arguments` already emitted as `ToolUseInputDelta`.
    emitted: usize,
}

/// Streaming accumulator. Feed parsed chunks with [`Accumulator::push_chunk`] and raw payloads
/// with [`Accumulator::push_raw`]; take the response with [`Accumulator::finish`].
#[derive(Debug)]
pub struct Accumulator {
    reasoning_field: ReasoningField,
    next_block: u32,
    thinking_block: Option<u32>,
    text_block: Option<u32>,
    /// The thinking/text block currently receiving deltas, closed when another kind starts.
    open_stream_block: Option<u32>,
    thinking: String,
    signature: Option<String>,
    text: String,
    tools: BTreeMap<u64, ToolAcc>,
    last_tool_index: Option<u64>,
    usage: Option<Usage>,
    finish_reason: Option<String>,
    model: Option<String>,
    response_id: Option<String>,
    raw: Vec<u8>,
    chunks: usize,
}

impl Accumulator {
    /// A fresh accumulator for one response.
    pub fn new(reasoning_field: ReasoningField) -> Self {
        Accumulator {
            reasoning_field,
            next_block: 0,
            thinking_block: None,
            text_block: None,
            open_stream_block: None,
            thinking: String::new(),
            signature: None,
            text: String::new(),
            tools: BTreeMap::new(),
            last_tool_index: None,
            usage: None,
            finish_reason: None,
            model: None,
            response_id: None,
            raw: Vec::new(),
            chunks: 0,
        }
    }

    /// Record one raw `data:` payload for `raw_response_hash` (payloads are joined with `\n`).
    pub fn push_raw(&mut self, payload: &str) {
        if !self.raw.is_empty() {
            self.raw.push(b'\n');
        }
        self.raw.extend_from_slice(payload.as_bytes());
    }

    /// Number of JSON chunks pushed so far.
    pub fn chunks(&self) -> usize {
        self.chunks
    }

    /// Whether a `finish_reason` has been seen.
    pub fn saw_finish_reason(&self) -> bool {
        self.finish_reason.is_some()
    }

    /// Consume one parsed chunk (`chat.completion.chunk`, or a whole `chat.completion` object:
    /// `message` is read like `delta`). Returns the deltas it produced.
    pub fn push_chunk(&mut self, v: &Value) -> Vec<ModelDelta> {
        self.chunks += 1;
        let mut out = Vec::new();
        if self.response_id.is_none()
            && let Some(id) = v.get("id").and_then(Value::as_str)
            && !id.is_empty()
        {
            self.response_id = Some(id.to_string());
        }
        if self.model.is_none()
            && let Some(m) = v.get("model").and_then(Value::as_str)
            && !m.is_empty()
        {
            self.model = Some(m.to_string());
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(usage_from(u));
        }
        if let Some(choices) = v.get("choices").and_then(Value::as_array) {
            for ch in choices {
                let node = ch.get("delta").or_else(|| ch.get("message"));
                if let Some(node) = node {
                    self.take_reasoning(node, &mut out);
                    if let Some(s) = node.get("content").and_then(Value::as_str)
                        && !s.is_empty()
                    {
                        let idx = self.start_text(&mut out);
                        self.text.push_str(s);
                        out.push(ModelDelta::TextDelta {
                            index: idx,
                            text: s.to_string(),
                        });
                    }
                    self.take_tool_calls(node, &mut out);
                }
                if let Some(fr) = ch.get("finish_reason").and_then(Value::as_str)
                    && !fr.is_empty()
                {
                    self.finish_reason = Some(fr.to_string());
                }
            }
        }
        out
    }

    fn alloc_block(&mut self) -> u32 {
        let i = self.next_block;
        self.next_block += 1;
        i
    }

    fn close_open_stream_block(&mut self, out: &mut Vec<ModelDelta>) {
        if let Some(i) = self.open_stream_block.take() {
            out.push(ModelDelta::BlockStop { index: i });
        }
    }

    fn start_text(&mut self, out: &mut Vec<ModelDelta>) -> u32 {
        match self.text_block {
            Some(i) => {
                if self.open_stream_block != Some(i) {
                    self.close_open_stream_block(out);
                    self.open_stream_block = Some(i);
                }
                i
            }
            None => {
                self.close_open_stream_block(out);
                let i = self.alloc_block();
                self.text_block = Some(i);
                self.open_stream_block = Some(i);
                i
            }
        }
    }

    fn start_thinking(&mut self, out: &mut Vec<ModelDelta>) -> u32 {
        match self.thinking_block {
            Some(i) => {
                if self.open_stream_block != Some(i) {
                    self.close_open_stream_block(out);
                    self.open_stream_block = Some(i);
                }
                i
            }
            None => {
                self.close_open_stream_block(out);
                let i = self.alloc_block();
                self.thinking_block = Some(i);
                self.open_stream_block = Some(i);
                i
            }
        }
    }

    fn take_reasoning(&mut self, node: &Value, out: &mut Vec<ModelDelta>) {
        let picked = match self.reasoning_field {
            ReasoningField::None | ReasoningField::InlineThink => None,
            ReasoningField::ReasoningContent => node.get("reasoning_content"),
            ReasoningField::Reasoning => node.get("reasoning"),
            ReasoningField::ProviderSpecificFields => node
                .get("provider_specific_fields")
                .and_then(|p| p.get("reasoning_content")),
        }
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
        if let Some(s) = picked {
            let idx = self.start_thinking(out);
            self.thinking.push_str(s);
            out.push(ModelDelta::ThinkingDelta {
                index: idx,
                text: s.to_string(),
            });
        }
        // LiteLLM (Anthropic upstream) adds `thinking_blocks[{type, thinking, signature}]`; the
        // text is duplicated in `reasoning_content`, so only the signature is taken from here.
        if let Some(blocks) = node.get("thinking_blocks").and_then(Value::as_array) {
            for b in blocks {
                if let Some(sig) = b.get("signature").and_then(Value::as_str)
                    && !sig.is_empty()
                {
                    self.signature = Some(sig.to_string());
                }
            }
        }
    }

    fn take_tool_calls(&mut self, node: &Value, out: &mut Vec<ModelDelta>) {
        let Some(calls) = node.get("tool_calls").and_then(Value::as_array) else {
            return;
        };
        for (pos, c) in calls.iter().enumerate() {
            let has_id = c
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty());
            let idx = match c.get("index").and_then(Value::as_u64) {
                Some(i) => i,
                // No index: a fragment with an id starts a new call (non-streaming bodies list
                // complete calls, so position works too); one without continues the last.
                None if has_id => self.tools.len().max(pos) as u64,
                None => self.last_tool_index.unwrap_or(0),
            };
            self.last_tool_index = Some(idx);
            let entry = self.tools.entry(idx).or_default();
            if has_id {
                entry.id = c["id"].as_str().unwrap_or_default().to_string();
            }
            if let Some(f) = c.get("function") {
                if let Some(n) = f.get("name").and_then(Value::as_str)
                    && !n.is_empty()
                {
                    entry.name = n.to_string();
                }
                match f.get("arguments") {
                    Some(Value::String(a)) => entry.arguments.push_str(a),
                    // Some proxies hand back already-parsed objects.
                    Some(Value::Object(o)) => {
                        entry.arguments = Value::Object(o.clone()).to_string();
                        entry.emitted = 0;
                    }
                    _ => {}
                }
            }
            // Emit `ToolUseStart` once the name is known, then any arguments not yet emitted.
            let needs_start = entry.block.is_none() && !entry.name.is_empty();
            if needs_start {
                let i = self.alloc_block();
                let entry = self.tools.get_mut(&idx).expect("just inserted");
                entry.block = Some(i);
                out.push(ModelDelta::ToolUseStart {
                    index: i,
                    id: entry.id.clone(),
                    name: entry.name.clone(),
                });
            }
            let entry = self.tools.get_mut(&idx).expect("just inserted");
            if let Some(i) = entry.block
                && entry.arguments.len() > entry.emitted
            {
                let partial = entry.arguments[entry.emitted..].to_string();
                entry.emitted = entry.arguments.len();
                out.push(ModelDelta::ToolUseInputDelta {
                    index: i,
                    partial_json: partial,
                });
            }
        }
    }

    /// Close every open block and build the response. `fallback_model` is used when no chunk
    /// carried a `model`.
    pub fn finish(mut self, fallback_model: &str) -> (Vec<ModelDelta>, ModelResponse) {
        let mut out = Vec::new();
        self.close_open_stream_block(&mut out);
        for t in self.tools.values() {
            if let Some(i) = t.block {
                out.push(ModelDelta::BlockStop { index: i });
            }
        }

        let mut text = std::mem::take(&mut self.text);
        let mut thinking = std::mem::take(&mut self.thinking);
        if self.reasoning_field == ReasoningField::InlineThink
            && let (Some(th), rest) = split_inline_think(&text)
        {
            thinking.push_str(&th);
            text = rest;
        }

        // Blocks in index order: thinking and text keep their arrival slots; tool calls follow
        // their block indices; an inline-think block goes right before the text.
        let mut ordered: Vec<(u32, ContentBlock)> = Vec::new();
        if !thinking.is_empty() || self.signature.is_some() {
            let slot = self.thinking_block.or(self.text_block).unwrap_or(0);
            ordered.push((
                slot,
                ContentBlock::Thinking {
                    text: thinking,
                    signature: self.signature.take(),
                },
            ));
        }
        if !text.is_empty() {
            let slot = self.text_block.unwrap_or(u32::MAX);
            ordered.push((slot, ContentBlock::Text { text }));
        }
        let mut had_tool_calls = false;
        for (k, t) in std::mem::take(&mut self.tools) {
            if t.name.is_empty() && t.arguments.is_empty() {
                continue;
            }
            had_tool_calls = true;
            let input = if t.arguments.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_str(&t.arguments)
                    .unwrap_or_else(|_| Value::String(t.arguments.clone()))
            };
            let id = if t.id.is_empty() {
                format!("call_{k}")
            } else {
                t.id
            };
            ordered.push((
                t.block.unwrap_or(u32::MAX),
                ContentBlock::ToolUse {
                    id,
                    name: t.name,
                    input,
                },
            ));
        }
        // Stable sort: the inline-think block shares the text slot and was pushed first.
        ordered.sort_by_key(|(slot, _)| *slot);
        let content: Vec<ContentBlock> = ordered.into_iter().map(|(_, b)| b).collect();

        let stop_reason = match self.finish_reason.as_deref() {
            Some("tool_calls") => StopReason::ToolUse,
            Some("length") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::ContentFilter,
            // `stop` (or nothing) with native tool calls present: the model stopped to call tools
            // even though the endpoint did not say so (vLLM without a parser flag, rule 1).
            Some("stop") | None if had_tool_calls => StopReason::ToolUse,
            Some("stop") | None => StopReason::EndTurn,
            Some(other) => StopReason::Other(other.to_string()),
        };

        let resp = ModelResponse {
            content,
            stop_reason,
            usage: self.usage.take().unwrap_or_default(),
            model_id: self
                .model
                .take()
                .unwrap_or_else(|| fallback_model.to_string()),
            raw_response_hash: Hash::of_bytes(&self.raw),
            response_id: self.response_id.take(),
        };
        out.push(ModelDelta::Complete(resp.clone()));
        (out, resp)
    }
}

fn usage_from(u: &Value) -> Usage {
    let n = |k: &str| u.get(k).and_then(Value::as_u64);
    Usage {
        input_tokens: n("prompt_tokens").unwrap_or(0),
        output_tokens: n("completion_tokens").unwrap_or(0),
        cache_read_tokens: u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64),
        reasoning_tokens: u
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64),
    }
}

/// Split a leading `<think>…</think>` out of `text`. Tolerates a missing opening tag (Qwen3
/// templates sometimes pre-fill `<think>` so the model only emits the close tag). Returns
/// `(None, text)` when there is no close tag.
pub fn split_inline_think(text: &str) -> (Option<String>, String) {
    let Some(close) = text.find("</think>") else {
        return (None, text.to_string());
    };
    let head = &text[..close];
    let think = match head.find("<think>") {
        Some(open) => &head[open + "<think>".len()..],
        None => head,
    };
    let rest = &text[close + "</think>".len()..];
    (
        Some(think.trim().to_string()),
        rest.trim_start_matches(['\n', '\r']).to_string(),
    )
}
