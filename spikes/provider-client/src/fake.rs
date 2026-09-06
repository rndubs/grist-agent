//! Fake OpenAI-compatible upstream emulating four streaming shapes.
//! Mount point selects the shape: `http://host/{vllm|litellm|llamacpp|hermes}/v1`.
//!
//! The wire shapes are modelled on: vLLM's OpenAI server (`reasoning_content`
//! deltas, index-keyed `tool_calls` deltas, usage chunk with empty `choices`),
//! LiteLLM proxy 1.100.0 (shape copied from a real `litellm --config` run in
//! front of this fake's vLLM shape; see docs/spikes/providers.md), llama.cpp `--jinja`
//! (one complete tool-call chunk, `timings` in the usage chunk) and a
//! parser-less "Hermes" endpoint (`<think>` and `<tool_call>` inline in text).

use std::convert::Infallible;
use std::net::SocketAddr;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Json, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Vllm,
    Litellm,
    Llamacpp,
    Hermes,
}

impl Shape {
    pub const ALL: [Shape; 4] = [Shape::Vllm, Shape::Litellm, Shape::Llamacpp, Shape::Hermes];
    pub fn parse(s: &str) -> Option<Shape> {
        Some(match s {
            "vllm" => Shape::Vllm,
            "litellm" => Shape::Litellm,
            "llamacpp" => Shape::Llamacpp,
            "hermes" => Shape::Hermes,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Shape::Vllm => "vllm",
            Shape::Litellm => "litellm",
            Shape::Llamacpp => "llamacpp",
            Shape::Hermes => "hermes",
        }
    }
}

/// Canned content shared by every shape so the normalized responses are identical.
pub mod canned {
    pub const PLAIN_THINK: &str = "The user greets me; a short friendly reply will do.";
    pub const PLAIN_TEXT: &str = "Hello from the fake upstream.";
    pub const TOOL_THINK: &str =
        "The user wants the weather in Oslo, so I should call get_weather.";
    pub const TOOL_NAME: &str = "get_weather";
    pub const TOOL_ARGS: &str = "{\"city\": \"Oslo\", \"unit\": \"celsius\"}";
    pub const TOOL_ID: &str = "call_0";
    pub const FINAL_THINK: &str = "The tool reported 12 degrees and clouds.";
    pub const FINAL_TEXT: &str = "It is 12 °C and cloudy in Oslo.";
    pub const STRUCTURED_TEXT: &str = "{\"answer\": \"Oslo\", \"confidence\": 0.9}";
    pub const PROMPT_TOKENS: u64 = 42;
    pub const COMPLETION_TOKENS: u64 = 17;
    /// LiteLLM counts reasoning tokens itself and reports them only when streaming.
    pub const LITELLM_STREAM_REASONING_TOKENS: u64 = 13;
}

#[derive(Debug)]
enum Scenario {
    Plain,
    ToolCall,
    FinalAfterTool,
    Structured,
}

fn scenario(body: &Value) -> Scenario {
    if body
        .get("response_format")
        .and_then(|r| r.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|t| t == "json_schema" || t == "json_object")
    {
        return Scenario::Structured;
    }
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    if msgs.last().is_some_and(|m| m["role"] == "tool") {
        return Scenario::FinalAfterTool;
    }
    let has_native_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty());
    let has_prompt_tools = msgs.iter().any(|m| {
        m["role"] == "system"
            && m["content"]
                .as_str()
                .is_some_and(|c| c.contains("<tool_call>"))
    });
    if has_native_tools || has_prompt_tools {
        return Scenario::ToolCall;
    }
    Scenario::Plain
}

fn split(s: &str, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        cur.push(ch);
        if cur.chars().count() >= n {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn usage_obj() -> Value {
    json!({
        "prompt_tokens": canned::PROMPT_TOKENS,
        "completion_tokens": canned::COMPLETION_TOKENS,
        "total_tokens": canned::PROMPT_TOKENS + canned::COMPLETION_TOKENS
    })
}

fn chunk(shape: Shape, model: &str, delta: Value, finish: Option<&str>) -> Value {
    let mut choice = json!({"index": 0, "delta": delta, "finish_reason": finish, "logprobs": null});
    if shape == Shape::Vllm {
        choice["stop_reason"] = Value::Null;
    }
    json!({
        "id": "chatcmpl-fake",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000,
        "model": model,
        "choices": [choice]
    })
}

fn reasoning_delta(shape: Shape, piece: &str) -> Value {
    match shape {
        Shape::Vllm => json!({"reasoning_content": piece, "content": null}),
        Shape::Llamacpp => json!({"reasoning_content": piece}),
        // Observed: LiteLLM passes vLLM's `reasoning_content` delta through
        // unchanged and adds nothing else to the delta.
        Shape::Litellm => json!({"reasoning_content": piece}),
        Shape::Hermes => unreachable!("hermes reasoning is inline"),
    }
}

/// Build the SSE events for a streaming reply.
fn stream_events(shape: Shape, model: &str, sc: &Scenario, include_usage: bool) -> Vec<String> {
    let mut ev: Vec<Value> = Vec::new();
    let c = |d, f| chunk(shape, model, d, f);
    let (think, text): (Option<&str>, String) = match sc {
        Scenario::Plain => (Some(canned::PLAIN_THINK), canned::PLAIN_TEXT.into()),
        Scenario::FinalAfterTool => (Some(canned::FINAL_THINK), canned::FINAL_TEXT.into()),
        Scenario::ToolCall => (Some(canned::TOOL_THINK), String::new()),
        Scenario::Structured => (None, canned::STRUCTURED_TEXT.into()),
    };

    ev.push(c(json!({"role": "assistant", "content": ""}), None));

    if shape == Shape::Hermes {
        // Everything inline in content, including <think> and <tool_call>.
        let mut full = String::new();
        if let Some(t) = think {
            full.push_str(&format!("<think>\n{t}\n</think>\n"));
        }
        full.push_str(&text);
        if matches!(sc, Scenario::ToolCall) {
            let args: Value = serde_json::from_str(canned::TOOL_ARGS).unwrap();
            full.push_str(&format!(
                "<tool_call>\n{}\n</tool_call>",
                json!({"name": canned::TOOL_NAME, "arguments": args})
            ));
        }
        for p in split(&full, 7) {
            ev.push(c(json!({"content": p}), None));
        }
        ev.push(c(json!({}), Some("stop")));
    } else {
        if let Some(t) = think {
            for p in split(t, 9) {
                ev.push(c(reasoning_delta(shape, &p), None));
            }
        }
        for p in split(&text, 6) {
            ev.push(c(json!({"content": p}), None));
        }
        match (sc, shape) {
            (Scenario::ToolCall, Shape::Llamacpp) => {
                // One complete tool call in a single chunk, finish_reason in the same chunk.
                ev.push(c(
                    json!({"tool_calls": [{
                        "index": 0, "id": canned::TOOL_ID, "type": "function",
                        "function": {"name": canned::TOOL_NAME, "arguments": canned::TOOL_ARGS}
                    }]}),
                    Some("tool_calls"),
                ));
            }
            (Scenario::ToolCall, _) => {
                ev.push(c(
                    json!({"tool_calls": [{
                        "index": 0, "id": canned::TOOL_ID, "type": "function",
                        "function": {"name": canned::TOOL_NAME, "arguments": ""}
                    }]}),
                    None,
                ));
                for p in split(canned::TOOL_ARGS, 5) {
                    // Observed: LiteLLM re-adds `"type": "function"` to every fragment.
                    let frag = if shape == Shape::Litellm {
                        json!({"tool_calls": [{"index": 0, "type": "function", "function": {"arguments": p}}]})
                    } else {
                        json!({"tool_calls": [{"index": 0, "function": {"arguments": p}}]})
                    };
                    ev.push(c(frag, None));
                }
                ev.push(c(json!({}), Some("tool_calls")));
            }
            _ => ev.push(c(json!({}), Some("stop"))),
        }
    }

    // Usage: trailing chunk with empty choices. llama.cpp always sends it (with
    // timings); the others only when stream_options.include_usage was requested.
    if include_usage || shape == Shape::Llamacpp {
        let mut u = json!({
            "id": "chatcmpl-fake",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000,
            "model": model,
            "choices": [],
            "usage": usage_obj()
        });
        if shape == Shape::Llamacpp {
            u["timings"] = json!({"prompt_n": 42, "predicted_n": 17, "predicted_per_second": 31.4});
        }
        if shape == Shape::Litellm {
            // Observed: LiteLLM's usage chunk keeps one choice with an empty delta
            // and adds its own reasoning-token count (streaming only).
            u["choices"] = json!([{"index": 0, "delta": {}}]);
            u["usage"]["completion_tokens_details"] =
                json!({"reasoning_tokens": canned::LITELLM_STREAM_REASONING_TOKENS});
        }
        ev.push(u);
    }

    let mut out: Vec<String> = ev.into_iter().map(|v| format!("data: {v}\n\n")).collect();
    out.push("data: [DONE]\n\n".to_string());
    out
}

fn full_response(shape: Shape, model: &str, sc: &Scenario) -> Value {
    let (think, text): (Option<&str>, String) = match sc {
        Scenario::Plain => (Some(canned::PLAIN_THINK), canned::PLAIN_TEXT.into()),
        Scenario::FinalAfterTool => (Some(canned::FINAL_THINK), canned::FINAL_TEXT.into()),
        Scenario::ToolCall => (Some(canned::TOOL_THINK), String::new()),
        Scenario::Structured => (None, canned::STRUCTURED_TEXT.into()),
    };
    let is_tool = matches!(sc, Scenario::ToolCall);
    let mut message = json!({"role": "assistant"});
    let mut finish = if is_tool { "tool_calls" } else { "stop" };
    match shape {
        Shape::Hermes => {
            let mut full = String::new();
            if let Some(t) = think {
                full.push_str(&format!("<think>\n{t}\n</think>\n"));
            }
            full.push_str(&text);
            if is_tool {
                let args: Value = serde_json::from_str(canned::TOOL_ARGS).unwrap();
                full.push_str(&format!(
                    "<tool_call>\n{}\n</tool_call>",
                    json!({"name": canned::TOOL_NAME, "arguments": args})
                ));
            }
            message["content"] = Value::String(full);
            finish = "stop";
        }
        _ => {
            message["content"] = if is_tool {
                Value::Null
            } else {
                Value::String(text)
            };
            if let Some(t) = think {
                message["reasoning_content"] = json!(t);
            }
            if shape == Shape::Litellm {
                // Observed: provider_specific_fields carries provider extras
                // (refusal, stop_reason), not the reasoning.
                message["provider_specific_fields"] = json!({"refusal": null});
            }
            if is_tool {
                message["tool_calls"] = json!([{
                    "id": canned::TOOL_ID, "type": "function",
                    "function": {"name": canned::TOOL_NAME, "arguments": canned::TOOL_ARGS}
                }]);
            }
        }
    }
    let mut choice =
        json!({"index": 0, "message": message, "finish_reason": finish, "logprobs": null});
    if shape == Shape::Vllm {
        choice["stop_reason"] = Value::Null;
    }
    if shape == Shape::Litellm {
        choice["provider_specific_fields"] = json!({"stop_reason": null});
    }
    let mut r = json!({
        "id": "chatcmpl-fake",
        "object": "chat.completion",
        "created": 1_700_000_000,
        "model": model,
        "choices": [choice],
        "usage": usage_obj()
    });
    if shape == Shape::Llamacpp {
        r["timings"] = json!({"prompt_n": 42, "predicted_n": 17, "predicted_per_second": 31.4});
    }
    r
}

fn openai_error(status: StatusCode, msg: &str) -> Response {
    (
        status,
        Json(json!({"error": {"message": msg, "type": "invalid_request_error", "param": null, "code": status.as_u16()}})),
    )
        .into_response()
}

#[derive(Clone, Default)]
pub struct FakeState {
    /// Byte size of each streamed body chunk; small values exercise partial-line parsing.
    pub chunk_bytes: usize,
}

async fn chat(
    State(st): State<FakeState>,
    Path(shape): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some(shape) = Shape::parse(&shape) else {
        return openai_error(StatusCode::NOT_FOUND, "unknown fake shape");
    };
    // LiteLLM proxy requires a key; the others (local vLLM / llama.cpp) do not.
    if shape == Shape::Litellm {
        let ok = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("Bearer ") && v.len() > "Bearer ".len());
        if !ok {
            return openai_error(
                StatusCode::UNAUTHORIZED,
                "Authentication Error, No api key passed in.",
            );
        }
    }
    let model = body["model"].as_str().unwrap_or("fake-model").to_string();
    let sc = scenario(&body);

    // Quirk emulation on the request side.
    if shape == Shape::Hermes && matches!(sc, Scenario::Structured) {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "response_format json_schema is not supported by this server",
        );
    }
    if shape == Shape::Hermes && body.get("tool_choice").is_some() {
        // What vLLM says when started without --enable-auto-tool-choice.
        return openai_error(
            StatusCode::BAD_REQUEST,
            "\"auto\" tool choice requires --enable-auto-tool-choice and --tool-call-parser to be set",
        );
    }

    let stream = body["stream"].as_bool().unwrap_or(false);
    if !stream {
        return Json(full_response(shape, &model, &sc)).into_response();
    }
    let include_usage = body["stream_options"]["include_usage"]
        .as_bool()
        .unwrap_or(false);
    let events = stream_events(shape, &model, &sc, include_usage);
    let all: Vec<u8> = events.concat().into_bytes();
    let n = st.chunk_bytes.max(1);
    let chunks: Vec<Result<Bytes, Infallible>> = all
        .chunks(n)
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    let body = Body::from_stream(futures_util::stream::iter(chunks));
    let mut resp = Response::new(body);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}

async fn models(Path(_shape): Path<String>) -> Json<Value> {
    Json(
        json!({"object": "list", "data": [{"id": "fake-model", "object": "model", "owned_by": "fake"}]}),
    )
}

pub fn router(chunk_bytes: usize) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/{shape}/v1/chat/completions", post(chat))
        .route("/{shape}/v1/models", get(models))
        .with_state(FakeState { chunk_bytes })
}

/// Bind and serve in the background. Port 0 picks an ephemeral port.
pub async fn serve(
    port: u16,
    chunk_bytes: usize,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        axum::serve(listener, router(chunk_bytes)).await.unwrap();
    });
    Ok((addr, handle))
}
