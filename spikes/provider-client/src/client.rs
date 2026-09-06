//! Minimal OpenAI-compatible chat-completions client with SSE streaming.
//! Behaviour is driven entirely by `Quirks`.

use std::collections::BTreeMap;
use std::fmt;

use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::hermes;
use crate::quirks::{Auth, Quirks, ReasoningField, ToolFormat};
use crate::types::{Completion, Message, Observed, Response, ToolCall, ToolDef, Usage};

#[derive(Debug, Clone)]
pub struct Endpoint {
    /// e.g. `http://127.0.0.1:8000/v1` (no trailing slash; `/chat/completions` is appended).
    pub base_url: String,
    pub model: String,
    pub quirks: Quirks,
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub stream: bool,
    pub response_format: Option<Value>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

#[derive(Debug)]
pub enum Error {
    /// Non-2xx from the endpoint. Body is included verbatim (endpoints put the
    /// useful diagnostics there); it never contains our key.
    Http {
        status: u16,
        body: String,
    },
    Transport(String),
    Protocol(String),
    /// The configured env var for the API key is not set. Names the variable, never a value.
    MissingSecret(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http { status, body } => write!(f, "HTTP {status}: {body}"),
            Error::Transport(e) => write!(f, "transport: {e}"),
            Error::Protocol(e) => write!(f, "protocol: {e}"),
            Error::MissingSecret(env) => write!(f, "env var {env} is not set (api key)"),
        }
    }
}
impl std::error::Error for Error {}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        // reqwest errors can embed the URL but never headers; still, strip nothing else.
        // Include the source chain: the top-level message is often just "error sending request".
        let e = e.without_url();
        let mut msg = e.to_string();
        let mut src = std::error::Error::source(&e);
        while let Some(inner) = src {
            msg.push_str(": ");
            msg.push_str(&inner.to_string());
            src = inner.source();
        }
        Error::Transport(msg)
    }
}

pub struct Client {
    http: reqwest::Client,
    pub endpoint: Endpoint,
    /// Debug aid: print the JSON body (never headers) to stderr before sending.
    pub dump_requests: bool,
}

impl Client {
    pub fn new(endpoint: Endpoint) -> Result<Self, Error> {
        // No idle-connection reuse: llama.cpp (cpp-httplib) closes keep-alive connections
        // after streamed responses and on a short idle timeout, and reqwest does not retry a
        // POST that fails on a stale pooled connection ("error sending request"). One fresh
        // connection per request costs a loopback handshake and removes the flake. P1.5 must
        // either do the same or retry once on a connection error before any bytes arrive.
        let mut b = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .pool_max_idle_per_host(0);
        // Loopback endpoints never go through a proxy, whatever HTTPS_PROXY says.
        if endpoint.base_url.contains("127.0.0.1") || endpoint.base_url.contains("localhost") {
            b = b.no_proxy();
        }
        Ok(Self {
            http: b.build()?,
            endpoint,
            dump_requests: false,
        })
    }

    fn url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.endpoint.base_url.trim_end_matches('/')
        )
    }

    /// Build the wire body from the request and the quirk flags.
    pub fn build_body(&self, req: &Request) -> Value {
        let q = &self.endpoint.quirks;
        let mut messages = req.messages.clone();
        let mut body = json!({
            "model": self.endpoint.model,
            "stream": req.stream,
        });
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(m) = req.max_tokens {
            body["max_tokens"] = json!(m);
        }
        if !req.tools.is_empty() {
            match &q.tool_format {
                ToolFormat::Native => {
                    let tools: Vec<Value> = req
                        .tools
                        .iter()
                        .map(|t| {
                            let mut f = json!({
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.parameters,
                            });
                            if q.strict_tool_schema {
                                f["strict"] = json!(true);
                            }
                            json!({"type": "function", "function": f})
                        })
                        .collect();
                    body["tools"] = Value::Array(tools);
                    body["tool_choice"] = json!("auto");
                }
                ToolFormat::Parsed(syntax) => {
                    // The parser-middleware path (D7): render the tools into the
                    // system prompt ourselves and send no `tools` field, so a
                    // server without a tool parser (or without
                    // --enable-auto-tool-choice, which rejects tool_choice=auto)
                    // still works.
                    let block = hermes::system_block(*syntax, &req.tools);
                    match messages.first_mut() {
                        Some(m) if m["role"] == "system" => {
                            let existing = m["content"].as_str().unwrap_or("").to_string();
                            m["content"] = Value::String(format!("{existing}\n\n{block}"));
                        }
                        _ => messages.insert(0, crate::types::system(&block)),
                    }
                }
            }
        }
        if req.stream && q.supports_stream_usage {
            body["stream_options"] = json!({"include_usage": true});
        }
        if let Some(rf) = &req.response_format {
            body["response_format"] = rf.clone();
        }
        body["messages"] = Value::Array(messages);
        body
    }

    pub async fn complete(&self, req: &Request) -> Result<Completion, Error> {
        let body = self.build_body(req);
        if self.dump_requests {
            eprintln!("--> {}", serde_json::to_string_pretty(&body).unwrap());
        }
        let mut rb = self.http.post(self.url()).json(&body);
        // Secret resolution happens here and only here (D10). The value is
        // moved straight into the header and never formatted anywhere.
        if let Auth::Bearer { env } = &self.endpoint.quirks.auth {
            let key = std::env::var(env).map_err(|_| Error::MissingSecret(env.clone()))?;
            rb = rb.bearer_auth(key);
        }
        let resp = rb.send().await?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Http { status, body });
        }
        let mut acc = Accumulator::new(&self.endpoint.quirks);
        acc.observed.http_status = status;
        if req.stream {
            acc.observed.streamed = true;
            let mut stream = resp.bytes_stream();
            let mut sse = SseParser::default();
            'outer: while let Some(chunk) = stream.next().await {
                let chunk: Bytes = chunk?;
                for data in sse.push(&chunk) {
                    if data.trim() == "[DONE]" {
                        break 'outer;
                    }
                    let v: Value = serde_json::from_str(&data)
                        .map_err(|e| Error::Protocol(format!("bad SSE JSON: {e}; data={data}")))?;
                    acc.push_chunk(&v);
                }
            }
        } else {
            let v: Value = resp.json().await?;
            acc.push_full(&v)?;
        }
        Ok(acc.finish())
    }
}

/// Splits a byte stream into SSE `data:` payloads. Handles `\n` and `\r\n`,
/// multi-line data (joined with `\n`), comments (`:`), and ignores `event:`/`id:`.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    data_lines: Vec<String>,
}

impl SseParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let mut line = String::from_utf8_lossy(&line).into_owned();
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            if line.is_empty() {
                if !self.data_lines.is_empty() {
                    out.push(self.data_lines.join("\n"));
                    self.data_lines.clear();
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("data:") {
                self.data_lines
                    .push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            }
            // event:, id:, retry: ignored
        }
        out
    }
}

#[derive(Default, Debug)]
struct ToolAcc {
    id: String,
    name: String,
    arguments: String,
}

struct Accumulator<'q> {
    q: &'q Quirks,
    thinking: String,
    text: String,
    tools: BTreeMap<u64, ToolAcc>,
    last_index: Option<u64>,
    usage: Option<Usage>,
    observed: Observed,
}

impl<'q> Accumulator<'q> {
    fn new(q: &'q Quirks) -> Self {
        Self {
            q,
            thinking: String::new(),
            text: String::new(),
            tools: BTreeMap::new(),
            last_index: None,
            usage: None,
            observed: Observed::default(),
        }
    }

    fn note_reasoning_seen(&mut self, key: &str) {
        if !self.observed.reasoning_fields_seen.iter().any(|k| k == key) {
            self.observed.reasoning_fields_seen.push(key.to_string());
        }
    }

    /// Pull reasoning text out of a `delta` or `message` object per the flag.
    fn take_reasoning(&mut self, node: &Value) {
        let rc = node.get("reasoning_content").and_then(Value::as_str);
        let r = node.get("reasoning").and_then(Value::as_str);
        let psf = node
            .get("provider_specific_fields")
            .and_then(|p| p.get("reasoning_content"))
            .and_then(Value::as_str);
        // Record everything we saw (probe output), regardless of the flag.
        if rc.is_some_and(|s| !s.is_empty()) {
            self.note_reasoning_seen("reasoning_content");
        }
        if r.is_some_and(|s| !s.is_empty()) {
            self.note_reasoning_seen("reasoning");
        }
        if psf.is_some_and(|s| !s.is_empty()) {
            self.note_reasoning_seen("provider_specific_fields.reasoning_content");
        }
        let picked = match &self.q.reasoning_field {
            ReasoningField::None | ReasoningField::InlineThink => None,
            ReasoningField::ReasoningContent => rc,
            ReasoningField::Reasoning => r,
            ReasoningField::ProviderSpecificFields => psf,
            // LiteLLM duplicates the text into provider_specific_fields; take the first hit only.
            ReasoningField::Auto => rc.or(r).or(psf),
        };
        if let Some(s) = picked {
            self.thinking.push_str(s);
        }
    }

    fn take_tool_calls(&mut self, node: &Value, streaming: bool) {
        let Some(calls) = node.get("tool_calls").and_then(Value::as_array) else {
            return;
        };
        if calls.is_empty() {
            return;
        }
        if streaming {
            self.observed.tool_call_delta_chunks += 1;
        }
        for (pos, c) in calls.iter().enumerate() {
            // Index-based accumulation. If `index` is missing (non-streaming, or
            // sloppy servers), a fragment with an id starts a new call and one
            // without continues the last.
            let idx = match c.get("index").and_then(Value::as_u64) {
                Some(i) => i,
                None if !streaming => pos as u64,
                None if c.get("id").is_some() => self.tools.len() as u64,
                None => self.last_index.unwrap_or(0),
            };
            self.last_index = Some(idx);
            let entry = self.tools.entry(idx).or_default();
            if let Some(id) = c
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                entry.id = id.to_string();
            }
            if let Some(f) = c.get("function") {
                if let Some(n) = f
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    entry.name = n.to_string();
                }
                match f.get("arguments") {
                    Some(Value::String(a)) => entry.arguments.push_str(a),
                    // Some proxies hand back already-parsed objects.
                    Some(Value::Object(o)) => {
                        entry.arguments = Value::Object(o.clone()).to_string()
                    }
                    _ => {}
                }
            }
        }
    }

    fn take_usage(&mut self, v: &Value) {
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(Usage {
                prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
                completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
                total_tokens: u["total_tokens"].as_u64().unwrap_or(0),
                reasoning_tokens: u
                    .get("completion_tokens_details")
                    .and_then(|d| d.get("reasoning_tokens"))
                    .and_then(Value::as_u64),
            });
        }
    }

    fn push_chunk(&mut self, v: &Value) {
        self.observed.chunks += 1;
        if self.observed.model.is_none() {
            self.observed.model = v.get("model").and_then(Value::as_str).map(str::to_string);
        }
        let choices = v.get("choices").and_then(Value::as_array);
        let no_choices = choices.is_none_or(|c| c.is_empty());
        if v.get("usage").is_some_and(|u| u.is_object()) {
            self.take_usage(v);
            // vLLM/llama.cpp send `choices: []` in the usage chunk; LiteLLM sends
            // `choices: [{index:0, delta:{}}]`. Either way it is the usage chunk.
            self.observed.usage_in_final_chunk = true;
            self.observed.usage_chunk_choices_empty = no_choices;
        }
        if let Some(choices) = choices {
            for ch in choices {
                if let Some(d) = ch.get("delta") {
                    if let Some(s) = d.get("content").and_then(Value::as_str) {
                        self.text.push_str(s);
                    }
                    self.take_reasoning(d);
                    self.take_tool_calls(d, true);
                }
                if let Some(fr) = ch.get("finish_reason").and_then(Value::as_str) {
                    self.observed.finish_reason = Some(fr.to_string());
                }
            }
        }
    }

    fn push_full(&mut self, v: &Value) -> Result<(), Error> {
        self.observed.model = v.get("model").and_then(Value::as_str).map(str::to_string);
        self.take_usage(v);
        let ch = v
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .ok_or_else(|| Error::Protocol(format!("no choices in response: {v}")))?;
        let m = &ch["message"];
        if let Some(s) = m.get("content").and_then(Value::as_str) {
            self.text.push_str(s);
        }
        self.take_reasoning(m);
        self.take_tool_calls(m, false);
        if let Some(fr) = ch.get("finish_reason").and_then(Value::as_str) {
            self.observed.finish_reason = Some(fr.to_string());
        }
        Ok(())
    }

    fn finish(mut self) -> Completion {
        let mut text = std::mem::take(&mut self.text);
        let mut thinking = std::mem::take(&mut self.thinking);

        // Inline <think> tags: mandated by the flag, or opportunistically in Auto.
        if matches!(
            self.q.reasoning_field,
            ReasoningField::InlineThink | ReasoningField::Auto
        ) {
            let (th, rest) = hermes::extract_think(&text);
            if let Some(th) = th {
                self.note_reasoning_seen("inline_think");
                thinking.push_str(&th);
                text = rest;
            }
        }

        let mut tool_calls: Vec<ToolCall> = self
            .tools
            .into_values()
            .map(|t| ToolCall {
                id: t.id,
                name: t.name,
                arguments: serde_json::from_str(&t.arguments)
                    .unwrap_or_else(|_| Value::String(t.arguments.clone())),
            })
            .collect();

        // Parsed syntax: mandated by the flag, or opportunistically when a
        // "native" endpoint turns out to have no parser (probe detection).
        let parsed_wanted = matches!(self.q.tool_format, ToolFormat::Parsed(_));
        if parsed_wanted || (tool_calls.is_empty() && text.contains("<tool_call>")) {
            let (calls, rest) = hermes::extract_tool_calls(&text, tool_calls.len());
            if !calls.is_empty() {
                self.observed.tool_calls_parsed_from_text = true;
                tool_calls.extend(calls);
                text = rest;
            }
        }

        // Fill in ids for servers that omit them (the kernel needs one for the tool result).
        for (i, c) in tool_calls.iter_mut().enumerate() {
            if c.id.is_empty() {
                c.id = format!("call_{i}");
            }
        }

        let thinking = if thinking.trim().is_empty() {
            None
        } else {
            Some(thinking.trim().to_string())
        };
        Completion {
            response: Response {
                thinking,
                text: text.trim().to_string(),
                tool_calls,
                usage: self.usage,
            },
            observed: self.observed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parser_splits_events_and_handles_crlf_and_partial_chunks() {
        let mut p = SseParser::default();
        let a = p.push(b"data: {\"a\":1}\r\n\r\n: keepalive\n\ndata: {\"b\"");
        assert_eq!(a, vec!["{\"a\":1}".to_string()]);
        let b = p.push(b":2}\n\ndata: [DONE]\n\n");
        assert_eq!(b, vec!["{\"b\":2}".to_string(), "[DONE]".to_string()]);
    }
}
