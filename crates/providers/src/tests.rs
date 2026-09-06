//! Unit tests for the OpenAI-compatible client against a fake `NetHandle` serving canned SSE.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use kernel::{
    ArtifactError, ArtifactHandle, ArtifactMeta, ArtifactStore, ContentBlock, Hash, HostError,
    HttpRequest, HttpResponse, Message, ModelDelta, ModelParams, ModelRequest, ModelResponse,
    NetHandle, PromptBlock, PromptBlockKind, Provider, ProviderError, RequestTrace, Role,
    SecretHandle, SecretResolver, SecretString, SessionId, StopReason, TaskId, TaskStatus,
    ThinkingConfig, ToolDefinition, ToolResultContent,
};
use serde_json::{Value, json};

use crate::{Auth, EndpointConfig, OpenAiCompatProvider, Quirks, ReasoningField, ToolFormat};

// ---------------------------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------------------------

/// What the fake received, minus the body stream.
#[derive(Debug, Clone)]
struct Captured {
    url: String,
    headers: Vec<(String, String)>,
    body: Value,
    timeout: Option<Duration>,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

enum Canned {
    /// Status, headers, body chunks delivered as separate reads.
    Response {
        status: u16,
        headers: Vec<(String, String)>,
        chunks: Vec<Vec<u8>>,
        /// After the chunks: end the stream, stall forever, or fail.
        tail: Tail,
    },
    /// `send` never returns.
    Hang,
    /// `send` fails at the transport level.
    SendError(HostError),
}

#[derive(Clone, Copy)]
enum Tail {
    End,
    Stall,
    Error,
}

#[derive(Default)]
struct FakeNet {
    canned: Mutex<VecDeque<Canned>>,
    captured: Mutex<Vec<Captured>>,
}

impl FakeNet {
    fn with(canned: Canned) -> Arc<FakeNet> {
        let net = FakeNet::default();
        net.canned.lock().unwrap().push_back(canned);
        Arc::new(net)
    }

    fn sse(body: &str) -> Arc<FakeNet> {
        FakeNet::with(ok(vec![body.as_bytes().to_vec()]))
    }

    fn last(&self) -> Captured {
        self.captured.lock().unwrap().last().cloned().unwrap()
    }
}

fn ok(chunks: Vec<Vec<u8>>) -> Canned {
    Canned::Response {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        chunks,
        tail: Tail::End,
    }
}

fn status(status: u16, headers: Vec<(&str, &str)>, body: &str) -> Canned {
    Canned::Response {
        status,
        headers: headers
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        chunks: vec![body.as_bytes().to_vec()],
        tail: Tail::End,
    }
}

#[async_trait]
impl NetHandle for FakeNet {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, HostError> {
        let body: Value = serde_json::from_slice(&req.body).expect("request body is JSON");
        self.captured.lock().unwrap().push(Captured {
            url: req.url,
            headers: req.headers,
            body,
            timeout: req.timeout,
        });
        let canned = self
            .canned
            .lock()
            .unwrap()
            .pop_front()
            .expect("a canned response");
        match canned {
            Canned::Hang => std::future::pending().await,
            Canned::SendError(e) => Err(e),
            Canned::Response {
                status,
                headers,
                chunks,
                tail,
            } => {
                let items: Vec<Result<Vec<u8>, HostError>> = chunks.into_iter().map(Ok).collect();
                let head = futures_util::stream::iter(items);
                let body: std::pin::Pin<
                    Box<dyn futures_core::Stream<Item = Result<Vec<u8>, HostError>> + Send>,
                > = match tail {
                    Tail::End => Box::pin(head),
                    Tail::Stall => Box::pin(head.chain(futures_util::stream::pending())),
                    Tail::Error => Box::pin(head.chain(futures_util::stream::once(async {
                        Err(HostError::Net("connection reset by peer".into()))
                    }))),
                };
                Ok(HttpResponse {
                    status,
                    headers,
                    body,
                })
            }
        }
    }
}

struct CountingResolver {
    value: Option<String>,
    calls: AtomicUsize,
}

impl CountingResolver {
    fn with(value: &str) -> Arc<CountingResolver> {
        Arc::new(CountingResolver {
            value: Some(value.into()),
            calls: AtomicUsize::new(0),
        })
    }

    fn failing() -> Arc<CountingResolver> {
        Arc::new(CountingResolver {
            value: None,
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl SecretResolver for CountingResolver {
    fn resolve_secret(&self, handle: &SecretHandle) -> Result<SecretString, HostError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.value {
            Some(v) => Ok(SecretString::new(v.clone())),
            None => Err(HostError::UnknownSecret(handle.name().to_string())),
        }
    }
}

#[derive(Default)]
struct MemArtifacts(Mutex<HashMap<ArtifactHandle, Vec<u8>>>);

#[async_trait]
impl ArtifactStore for MemArtifacts {
    fn name(&self) -> &str {
        "mem"
    }
    async fn put(&self, bytes: &[u8], _mime: &str) -> Result<ArtifactHandle, ArtifactError> {
        let h = ArtifactHandle(Hash::of_bytes(bytes));
        self.0.lock().unwrap().insert(h.clone(), bytes.to_vec());
        Ok(h)
    }
    async fn get(&self, handle: &ArtifactHandle) -> Result<Vec<u8>, ArtifactError> {
        self.0
            .lock()
            .unwrap()
            .get(handle)
            .cloned()
            .ok_or_else(|| ArtifactError::NotFound(handle.clone()))
    }
    async fn get_range(
        &self,
        handle: &ArtifactHandle,
        range: std::ops::Range<u64>,
    ) -> Result<Vec<u8>, ArtifactError> {
        let b = self.get(handle).await?;
        Ok(b[range.start as usize..range.end as usize].to_vec())
    }
    async fn head(&self, handle: &ArtifactHandle, bytes: u64) -> Result<Vec<u8>, ArtifactError> {
        let b = self.get(handle).await?;
        Ok(b[..(bytes as usize).min(b.len())].to_vec())
    }
    async fn tail(&self, handle: &ArtifactHandle, bytes: u64) -> Result<Vec<u8>, ArtifactError> {
        let b = self.get(handle).await?;
        Ok(b[b.len().saturating_sub(bytes as usize)..].to_vec())
    }
    async fn stat(&self, handle: &ArtifactHandle) -> Result<ArtifactMeta, ArtifactError> {
        let b = self.get(handle).await?;
        Ok(ArtifactMeta {
            size: b.len() as u64,
            mime: "application/octet-stream".into(),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn trace() -> RequestTrace {
    RequestTrace {
        session_id: SessionId("s1".into()),
        turn: 1,
        attempt: 1,
        checkpoint_hash: Hash::of_bytes(b"ckpt"),
        request_id: "req-0001".into(),
    }
}

fn request(messages: Vec<Message>) -> ModelRequest {
    ModelRequest {
        model_id: "test-model".into(),
        system: vec![],
        messages,
        tools: vec![],
        params: ModelParams::default(),
        trace: trace(),
    }
}

fn weather_tool() -> ToolDefinition {
    ToolDefinition {
        name: "get_weather".into(),
        description: "Current weather".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "city": {"type": "string"},
                "opts": {"type": "object", "properties": {"units": {"type": "string"}}}
            },
            "required": ["city"]
        }),
    }
}

struct Built {
    provider: OpenAiCompatProvider,
    net: Arc<FakeNet>,
    resolver: Arc<CountingResolver>,
}

fn build(net: Arc<FakeNet>, quirks: Quirks) -> Built {
    build_with(net, quirks, CountingResolver::with("sk-secret-value"), None)
}

fn build_with(
    net: Arc<FakeNet>,
    quirks: Quirks,
    resolver: Arc<CountingResolver>,
    timeout: Option<Duration>,
) -> Built {
    let endpoint = EndpointConfig {
        base_url: "http://fake.local/v1/".into(),
        timeout,
        extra_headers: vec![],
    };
    let provider = OpenAiCompatProvider::new(
        "openai_compat",
        endpoint,
        quirks,
        net.clone(),
        resolver.clone(),
        Arc::new(MemArtifacts::default()),
    );
    Built {
        provider,
        net,
        resolver,
    }
}

fn rc() -> Quirks {
    Quirks {
        reasoning_field: ReasoningField::ReasoningContent,
        ..Quirks::default()
    }
}

/// `data:` lines joined into an SSE body.
fn sse(chunks: &[Value]) -> String {
    let mut s = String::new();
    for c in chunks {
        s.push_str(&format!("data: {c}\n\n"));
    }
    s.push_str("data: [DONE]\n\n");
    s
}

fn chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "served-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
    })
}

async fn collect(p: &OpenAiCompatProvider, req: ModelRequest) -> Vec<ModelDelta> {
    let mut s = p.complete_stream(req).await.expect("stream opens");
    let mut out = Vec::new();
    while let Some(d) = s.next().await {
        out.push(d.expect("delta ok"));
    }
    out
}

fn complete_of(deltas: &[ModelDelta]) -> ModelResponse {
    let completes: Vec<&ModelResponse> = deltas
        .iter()
        .filter_map(|d| match d {
            ModelDelta::Complete(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(completes.len(), 1, "exactly one Complete: {deltas:?}");
    assert!(
        matches!(deltas.last(), Some(ModelDelta::Complete(_))),
        "Complete is last"
    );
    completes[0].clone()
}

// ---------------------------------------------------------------------------------------------
// Streaming shapes
// ---------------------------------------------------------------------------------------------

fn vllm_tool_stream() -> String {
    let mut ev = vec![chunk(json!({"role": "assistant", "content": ""}), None)];
    for p in ["Let me ", "check the ", "weather."] {
        ev.push(chunk(
            json!({"reasoning_content": p, "content": null}),
            None,
        ));
    }
    ev.push(chunk(
        json!({"tool_calls": [{"index": 0, "id": "call_abc", "type": "function",
                "function": {"name": "get_weather", "arguments": ""}}]}),
        None,
    ));
    for p in ["{\"ci", "ty\": \"Par", "is\"}"] {
        ev.push(chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": p}}]}),
            None,
        ));
    }
    ev.push(chunk(json!({}), Some("tool_calls")));
    ev.push(json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "served-model",
        "choices": [],
        "usage": {"prompt_tokens": 40, "completion_tokens": 12, "total_tokens": 52,
                  "prompt_tokens_details": {"cached_tokens": 8}}
    }));
    sse(&ev)
}

#[tokio::test]
async fn vllm_index_keyed_fragments_reasoning_and_trailing_usage() {
    let b = build(FakeNet::sse(&vllm_tool_stream()), rc());
    let deltas = collect(&b.provider, request(vec![Message::user_text("hi")])).await;
    let resp = complete_of(&deltas);

    assert_eq!(
        resp.content,
        vec![
            ContentBlock::Thinking {
                text: "Let me check the weather.".into(),
                signature: None
            },
            ContentBlock::ToolUse {
                id: "call_abc".into(),
                name: "get_weather".into(),
                input: json!({"city": "Paris"})
            },
        ]
    );
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    assert_eq!(resp.usage.input_tokens, 40);
    assert_eq!(resp.usage.output_tokens, 12);
    assert_eq!(resp.usage.cache_read_tokens, Some(8));
    assert_eq!(resp.usage.reasoning_tokens, None);
    assert_eq!(resp.model_id, "served-model");
    assert_eq!(resp.response_id.as_deref(), Some("chatcmpl-1"));

    // Delta shape: thinking deltas on block 0, tool start + input deltas on block 1, stops.
    assert!(matches!(
        &deltas[0],
        ModelDelta::ThinkingDelta { index: 0, text } if text == "Let me "
    ));
    assert!(deltas.iter().any(|d| matches!(
        d,
        ModelDelta::ToolUseStart { index: 1, id, name } if id == "call_abc" && name == "get_weather"
    )));
    let partial: String = deltas
        .iter()
        .filter_map(|d| match d {
            ModelDelta::ToolUseInputDelta {
                index: 1,
                partial_json,
            } => Some(partial_json.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(partial, "{\"city\": \"Paris\"}");
    assert!(deltas.contains(&ModelDelta::BlockStop { index: 0 }));
    assert!(deltas.contains(&ModelDelta::BlockStop { index: 1 }));
}

#[tokio::test]
async fn complete_returns_the_same_response_as_the_stream() {
    let b = build(FakeNet::sse(&vllm_tool_stream()), rc());
    let resp = b
        .provider
        .complete(request(vec![Message::user_text("hi")]))
        .await
        .unwrap();
    assert_eq!(resp.tool_calls().len(), 1);
    assert_eq!(resp.tool_calls()[0].name, "get_weather");
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn litellm_usage_chunk_with_choices_and_thinking_signature() {
    let ev = vec![
        chunk(json!({"role": "assistant", "content": ""}), None),
        chunk(
            json!({"reasoning_content": "Thinking hard.",
                   "thinking_blocks": [{"type": "thinking", "thinking": "Thinking hard.", "signature": null}]}),
            None,
        ),
        chunk(
            json!({"thinking_blocks": [{"type": "thinking", "thinking": "", "signature": "sig-XYZ=="}]}),
            None,
        ),
        chunk(json!({"content": "The answer is 42."}), None),
        chunk(json!({}), Some("stop")),
        json!({
            "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "served-model",
            "choices": [{"index": 0, "delta": {}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30,
                      "completion_tokens_details": {"reasoning_tokens": 5}}
        }),
    ];
    let b = build(FakeNet::sse(&sse(&ev)), rc());
    let resp = b
        .provider
        .complete(request(vec![Message::user_text("q")]))
        .await
        .unwrap();
    assert_eq!(
        resp.content,
        vec![
            ContentBlock::Thinking {
                text: "Thinking hard.".into(),
                signature: Some("sig-XYZ==".into())
            },
            ContentBlock::Text {
                text: "The answer is 42.".into()
            },
        ]
    );
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert_eq!(resp.usage.reasoning_tokens, Some(5));
    assert_eq!(resp.usage.input_tokens, 10);

    // The Thinking block (with its signature) survives a serde round trip.
    let json = serde_json::to_string(&resp.content[0]).unwrap();
    let back: ContentBlock = serde_json::from_str(&json).unwrap();
    assert_eq!(back, resp.content[0]);
    let json = serde_json::to_string(&resp).unwrap();
    let back: ModelResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(back, resp);
}

#[tokio::test]
async fn llama_cpp_single_chunk_tool_call_with_timings() {
    let ev = vec![
        chunk(json!({"role": "assistant", "content": ""}), None),
        chunk(
            json!({"tool_calls": [{"index": 0, "id": "rnd-7f2a", "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Oslo\"}"}}]}),
            Some("tool_calls"),
        ),
        json!({
            "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "served-model",
            "choices": [],
            "usage": {"prompt_tokens": 42, "completion_tokens": 17, "total_tokens": 59},
            "timings": {"prompt_n": 42, "predicted_n": 17, "predicted_per_second": 31.4}
        }),
    ];
    let b = build(FakeNet::sse(&sse(&ev)), Quirks::default());
    let deltas = collect(&b.provider, request(vec![Message::user_text("q")])).await;
    let resp = complete_of(&deltas);
    assert_eq!(
        resp.content,
        vec![ContentBlock::ToolUse {
            id: "rnd-7f2a".into(),
            name: "get_weather".into(),
            input: json!({"city": "Oslo"})
        }]
    );
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    assert_eq!(resp.usage.output_tokens, 17);
    assert!(deltas.iter().any(|d| matches!(
        d,
        ModelDelta::ToolUseInputDelta { partial_json, .. } if partial_json == "{\"city\":\"Oslo\"}"
    )));
}

#[tokio::test]
async fn hermes_without_parser_keeps_tool_call_text_and_splits_inline_think() {
    let full = "<think>\nPlan it.\n</think>\nSure.\n<tool_call>\n{\"name\": \"get_weather\", \"arguments\": {\"city\": \"Rome\"}}\n</tool_call>";
    let mut ev = vec![chunk(json!({"role": "assistant", "content": ""}), None)];
    let chars: Vec<char> = full.chars().collect();
    for piece in chars.chunks(7) {
        let s: String = piece.iter().collect();
        ev.push(chunk(json!({"content": s}), None));
    }
    ev.push(chunk(json!({}), Some("stop")));
    let q = Quirks {
        reasoning_field: ReasoningField::InlineThink,
        tool_format: ToolFormat::Parsed("hermes".into()),
        ..Quirks::default()
    };
    let b = build(FakeNet::sse(&sse(&ev)), q);
    let deltas = collect(&b.provider, request(vec![Message::user_text("q")])).await;
    let resp = complete_of(&deltas);
    assert_eq!(
        resp.content,
        vec![
            ContentBlock::Thinking {
                text: "Plan it.".into(),
                signature: None
            },
            ContentBlock::Text {
                text: "Sure.\n<tool_call>\n{\"name\": \"get_weather\", \"arguments\": {\"city\": \"Rome\"}}\n</tool_call>".into()
            },
        ]
    );
    // The provider does not parse the text syntax; that is the middleware's job.
    assert!(resp.tool_calls().is_empty());
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    // Raw text deltas were streamed as one text block.
    let streamed: String = deltas
        .iter()
        .filter_map(|d| match d {
            ModelDelta::TextDelta { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(streamed, full);
}

#[tokio::test]
async fn tool_calls_without_index_and_object_arguments_are_accumulated() {
    let ev = vec![
        chunk(
            json!({"tool_calls": [{"id": "a1", "type": "function",
                    "function": {"name": "get_weather", "arguments": {"city": "Lima"}}}]}),
            None,
        ),
        chunk(
            json!({"tool_calls": [{"id": "b2", "type": "function",
                    "function": {"name": "get_time", "arguments": "{\"tz\":"}}]}),
            None,
        ),
        chunk(
            json!({"tool_calls": [{"function": {"arguments": "\"UTC\"}"}}]}),
            None,
        ),
        // `finish_reason: stop` even though tool calls exist (vLLM without the parser flag).
        chunk(json!({}), Some("stop")),
    ];
    let b = build(FakeNet::sse(&sse(&ev)), Quirks::default());
    let resp = b
        .provider
        .complete(request(vec![Message::user_text("q")]))
        .await
        .unwrap();
    assert_eq!(
        resp.content,
        vec![
            ContentBlock::ToolUse {
                id: "a1".into(),
                name: "get_weather".into(),
                input: json!({"city": "Lima"})
            },
            ContentBlock::ToolUse {
                id: "b2".into(),
                name: "get_time".into(),
                input: json!({"tz": "UTC"})
            },
        ]
    );
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn stop_reason_mapping() {
    for (finish, expected) in [
        ("stop", StopReason::EndTurn),
        ("length", StopReason::MaxTokens),
        ("content_filter", StopReason::ContentFilter),
        ("weird", StopReason::Other("weird".into())),
    ] {
        let ev = vec![
            chunk(json!({"content": "x"}), None),
            chunk(json!({}), Some(finish)),
        ];
        let b = build(FakeNet::sse(&sse(&ev)), Quirks::default());
        let resp = b
            .provider
            .complete(request(vec![Message::user_text("q")]))
            .await
            .unwrap();
        assert_eq!(resp.stop_reason, expected, "finish_reason {finish}");
    }
}

#[tokio::test]
async fn reasoning_field_variants_each_produce_one_thinking_block() {
    let cases = [
        (
            ReasoningField::Reasoning,
            json!({"reasoning": "r1"}),
            json!({"reasoning": "r2", "reasoning_content": "IGNORED"}),
        ),
        (
            ReasoningField::ProviderSpecificFields,
            json!({"provider_specific_fields": {"reasoning_content": "p1"}}),
            json!({"provider_specific_fields": {"reasoning_content": "p2"}, "reasoning_content": "IGNORED"}),
        ),
        (
            ReasoningField::None,
            json!({"reasoning_content": "IGNORED"}),
            json!({"reasoning": "IGNORED"}),
        ),
    ];
    for (field, d1, d2) in cases {
        let ev = vec![
            chunk(d1, None),
            chunk(d2, None),
            chunk(json!({"content": "t"}), Some("stop")),
        ];
        let q = Quirks {
            reasoning_field: field.clone(),
            ..Quirks::default()
        };
        let b = build(FakeNet::sse(&sse(&ev)), q);
        let resp = b
            .provider
            .complete(request(vec![Message::user_text("q")]))
            .await
            .unwrap();
        let thinking: Vec<&ContentBlock> = resp
            .content
            .iter()
            .filter(|b| matches!(b, ContentBlock::Thinking { .. }))
            .collect();
        match field {
            ReasoningField::None => assert!(thinking.is_empty()),
            ReasoningField::Reasoning => {
                assert_eq!(thinking.len(), 1);
                assert!(
                    matches!(thinking[0], ContentBlock::Thinking { text, .. } if text == "r1r2")
                );
            }
            ReasoningField::ProviderSpecificFields => {
                assert_eq!(thinking.len(), 1);
                assert!(
                    matches!(thinking[0], ContentBlock::Thinking { text, .. } if text == "p1p2")
                );
            }
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn chunk_boundaries_split_mid_line_and_mid_utf8() {
    let ev = vec![
        chunk(json!({"content": "Grüße ✓ "}), None),
        chunk(json!({"content": "done"}), Some("stop")),
    ];
    let body = sse(&ev).into_bytes();
    // Cut into 5-byte reads: guaranteed to split lines, the JSON, and multi-byte chars.
    let chunks: Vec<Vec<u8>> = body.chunks(5).map(<[u8]>::to_vec).collect();
    let b = build(FakeNet::with(ok(chunks)), Quirks::default());
    let resp = b
        .provider
        .complete(request(vec![Message::user_text("q")]))
        .await
        .unwrap();
    assert_eq!(
        resp.content,
        vec![ContentBlock::Text {
            text: "Grüße ✓ done".into()
        }]
    );
    // Same bytes, same raw hash regardless of how they were chunked.
    let b2 = build(FakeNet::with(ok(vec![body])), Quirks::default());
    let resp2 = b2
        .provider
        .complete(request(vec![Message::user_text("q")]))
        .await
        .unwrap();
    assert_eq!(resp.raw_response_hash, resp2.raw_response_hash);
}

#[tokio::test]
async fn raw_response_hash_is_stable_and_covers_the_data_lines() {
    let ev = vec![chunk(json!({"content": "a"}), Some("stop"))];
    let body = sse(&ev);
    let b = build(FakeNet::sse(&body), Quirks::default());
    let resp = b
        .provider
        .complete(request(vec![Message::user_text("q")]))
        .await
        .unwrap();
    // Payload lines (without the `data: ` prefix, without `[DONE]`) joined with '\n'.
    let expected = Hash::of_bytes(ev[0].to_string().as_bytes());
    assert_eq!(resp.raw_response_hash, expected);

    let ev2 = vec![chunk(json!({"content": "b"}), Some("stop"))];
    let b2 = build(FakeNet::sse(&sse(&ev2)), Quirks::default());
    let resp2 = b2
        .provider
        .complete(request(vec![Message::user_text("q")]))
        .await
        .unwrap();
    assert_ne!(resp.raw_response_hash, resp2.raw_response_hash);
}

#[tokio::test]
async fn model_id_falls_back_to_the_request_when_chunks_carry_none() {
    let ev = vec![
        json!({"choices": [{"index": 0, "delta": {"content": "x"}, "finish_reason": "stop"}]}),
    ];
    let b = build(FakeNet::sse(&sse(&ev)), Quirks::default());
    let resp = b
        .provider
        .complete(request(vec![Message::user_text("q")]))
        .await
        .unwrap();
    assert_eq!(resp.model_id, "test-model");
    assert_eq!(resp.response_id, None);
    assert_eq!(resp.usage, kernel::Usage::default());
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

async fn expect_err(canned: Canned, quirks: Quirks) -> ProviderError {
    let b = build(FakeNet::with(canned), quirks);
    match b
        .provider
        .complete(request(vec![Message::user_text("q")]))
        .await
    {
        Ok(r) => panic!("expected an error, got {r:?}"),
        Err(e) => e,
    }
}

#[tokio::test]
async fn http_429_maps_to_rate_limited_with_retry_after() {
    let e = expect_err(
        status(
            429,
            vec![("Retry-After", "12")],
            r#"{"error":{"message":"slow down"}}"#,
        ),
        Quirks::default(),
    )
    .await;
    assert!(matches!(
        e,
        ProviderError::RateLimited { retry_after: Some(d) } if d == Duration::from_secs(12)
    ));
    assert!(e.retryable());
}

#[tokio::test]
async fn http_500_maps_to_server() {
    let e = expect_err(status(500, vec![], "boom"), Quirks::default()).await;
    assert!(matches!(&e, ProviderError::Server { status: 500, message } if message == "boom"));
    assert!(e.retryable());
}

#[tokio::test]
async fn http_401_maps_to_auth_without_leaking_the_key() {
    let e = expect_err(
        status(
            401,
            vec![],
            r#"{"error":{"message":"Authentication Error, Invalid proxy server token passed"}}"#,
        ),
        Quirks {
            auth: Auth::Bearer(SecretHandle::new("KEY")),
            ..Quirks::default()
        },
    )
    .await;
    assert!(matches!(&e, ProviderError::Auth(m) if m.contains("Invalid proxy server token")));
    assert!(!e.to_string().contains("sk-secret-value"));
    assert!(!e.retryable());
}

#[tokio::test]
async fn http_400_context_length_maps_to_context_too_long() {
    let e = expect_err(
        status(
            400,
            vec![],
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens. However, you requested 9000 tokens.","type":"invalid_request_error"}}"#,
        ),
        Quirks::default(),
    )
    .await;
    assert!(matches!(&e, ProviderError::ContextTooLong(m) if m.contains("8192")));
    let e = expect_err(
        status(
            400,
            vec![],
            r#"{"error":{"message":"invalid tool schema"}}"#,
        ),
        Quirks::default(),
    )
    .await;
    assert!(matches!(e, ProviderError::Client { status: 400, .. }));
}

#[tokio::test]
async fn malformed_json_chunk_is_invalid_response() {
    let e = expect_err(
        ok(vec![b"data: {\"choices\": [}\n\n".to_vec()]),
        Quirks::default(),
    )
    .await;
    assert!(matches!(e, ProviderError::InvalidResponse(_)));
    assert!(!e.retryable());
}

#[tokio::test]
async fn mid_stream_error_object_is_a_server_error() {
    let body = "data: {\"error\": {\"message\": \"upstream exploded\", \"code\": 502}}\n\n";
    let e = expect_err(ok(vec![body.as_bytes().to_vec()]), Quirks::default()).await;
    assert!(
        matches!(&e, ProviderError::Server { message, .. } if message.contains("upstream exploded"))
    );
}

#[tokio::test]
async fn stream_without_done_completes_when_finished_but_errors_when_truncated() {
    let finished = format!(
        "data: {}\n\n",
        chunk(json!({"content": "ok"}), Some("stop"))
    );
    let b = build(
        FakeNet::with(ok(vec![finished.into_bytes()])),
        Quirks::default(),
    );
    let resp = b
        .provider
        .complete(request(vec![Message::user_text("q")]))
        .await
        .unwrap();
    assert_eq!(resp.content, vec![ContentBlock::Text { text: "ok".into() }]);

    let truncated = format!("data: {}\n\n", chunk(json!({"content": "partial"}), None));
    let e = expect_err(ok(vec![truncated.into_bytes()]), Quirks::default()).await;
    assert!(matches!(&e, ProviderError::InvalidResponse(m) if m.contains("without [DONE]")));
}

#[tokio::test]
async fn transport_errors_map_to_transport() {
    let e = expect_err(
        Canned::SendError(HostError::Net("connection refused".into())),
        Quirks::default(),
    )
    .await;
    assert!(matches!(e, ProviderError::Transport(_)));
    assert!(e.retryable());

    // Body stream failing mid-way.
    let head = format!("data: {}\n\n", chunk(json!({"content": "a"}), None));
    let e = expect_err(
        Canned::Response {
            status: 200,
            headers: vec![],
            chunks: vec![head.into_bytes()],
            tail: Tail::Error,
        },
        Quirks::default(),
    )
    .await;
    assert!(matches!(e, ProviderError::Transport(_)));
}

#[tokio::test]
async fn timeouts_map_to_timeout() {
    let t = Some(Duration::from_millis(50));
    // Never answering `send`.
    let b = build_with(
        FakeNet::with(Canned::Hang),
        Quirks::default(),
        CountingResolver::with("k"),
        t,
    );
    let e = b
        .provider
        .complete(request(vec![Message::user_text("q")]))
        .await
        .unwrap_err();
    assert!(matches!(e, ProviderError::Timeout(d) if d == Duration::from_millis(50)));
    assert_eq!(
        b.net.last().timeout,
        t,
        "HttpRequest.timeout is passed to the host"
    );

    // Body that stalls after the first chunk.
    let head = format!("data: {}\n\n", chunk(json!({"content": "a"}), None));
    let b = build_with(
        FakeNet::with(Canned::Response {
            status: 200,
            headers: vec![],
            chunks: vec![head.into_bytes()],
            tail: Tail::Stall,
        }),
        Quirks::default(),
        CountingResolver::with("k"),
        t,
    );
    let e = b
        .provider
        .complete(request(vec![Message::user_text("q")]))
        .await
        .unwrap_err();
    assert!(matches!(e, ProviderError::Timeout(_)));
    assert!(e.retryable());

    // The host reporting its own timeout.
    let e = expect_err(
        Canned::SendError(HostError::Net("operation timed out".into())),
        Quirks::default(),
    )
    .await;
    assert!(matches!(e, ProviderError::Timeout(_)));
}

#[tokio::test]
async fn secret_resolution_failure_is_auth() {
    let b = build_with(
        FakeNet::sse(&sse(&[chunk(json!({"content": "x"}), Some("stop"))])),
        Quirks {
            auth: Auth::Bearer(SecretHandle::new("MISSING_KEY")),
            ..Quirks::default()
        },
        CountingResolver::failing(),
        None,
    );
    let e = b
        .provider
        .complete(request(vec![Message::user_text("q")]))
        .await
        .unwrap_err();
    assert!(matches!(&e, ProviderError::Auth(m) if m.contains("MISSING_KEY")));
    // Nothing was sent.
    assert!(b.net.captured.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Request rendering (asserted on the HttpRequest the fake received)
// ---------------------------------------------------------------------------------------------

fn plain_ok() -> String {
    sse(&[chunk(json!({"content": "ok"}), Some("stop"))])
}

#[tokio::test]
async fn request_body_shape_system_tools_params_and_headers() {
    let b = build(
        FakeNet::sse(&plain_ok()),
        Quirks {
            auth: Auth::Bearer(SecretHandle::new("KEY")),
            reasoning_field: ReasoningField::ReasoningContent,
            ..Quirks::default()
        },
    );
    let mut req = request(vec![
        Message::user_text("What's the weather?"),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    text: "think".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::Text {
                    text: "Checking.".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "get_weather".into(),
                    input: json!({"city": "Paris"}),
                },
            ],
        },
        Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: ToolResultContent::Json(json!({"temp_c": 21})),
                is_error: false,
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_2".into(),
                content: ToolResultContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "line1".into(),
                    },
                    ContentBlock::Text {
                        text: "line2".into(),
                    },
                ]),
                is_error: true,
            }],
        },
    ]);
    req.system = vec![
        PromptBlock::new(PromptBlockKind::Model, "m", "You are terse."),
        PromptBlock::new(PromptBlockKind::Role, "r", "You are a weather bot."),
    ];
    req.tools = vec![weather_tool()];
    req.params = ModelParams {
        temperature: Some(0.2),
        top_p: Some(0.9),
        max_tokens: Some(256),
        stop: vec!["END".into()],
        thinking: Some(ThinkingConfig {
            enabled: true,
            budget_tokens: Some(1024),
        }),
        extra: json!({"seed": 7, "tool_choice": "auto"}),
    };
    b.provider.complete(req).await.unwrap();

    let cap = b.net.last();
    assert_eq!(cap.url, "http://fake.local/v1/chat/completions");
    assert_eq!(cap.header("Authorization"), Some("Bearer sk-secret-value"));
    assert_eq!(cap.header("X-Request-Id"), Some("req-0001"));
    assert_eq!(cap.header("Content-Type"), Some("application/json"));

    let body = &cap.body;
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["top_p"], 0.9);
    assert_eq!(body["max_tokens"], 256);
    assert_eq!(body["stop"], json!(["END"]));
    assert_eq!(
        body["thinking"],
        json!({"type": "enabled", "budget_tokens": 1024})
    );
    assert_eq!(body["seed"], 7, "extra merged");
    assert_eq!(body["tool_choice"], "auto", "extra can add tool_choice");

    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(
        msgs[0]["content"],
        "You are terse.\n\nYou are a weather bot."
    );
    assert_eq!(
        msgs[1],
        json!({"role": "user", "content": "What's the weather?"})
    );
    assert_eq!(msgs[2]["role"], "assistant");
    assert_eq!(msgs[2]["content"], "Checking.");
    assert_eq!(msgs[2]["reasoning_content"], "think");
    assert_eq!(
        msgs[2]["thinking_blocks"],
        json!([{"type": "thinking", "thinking": "think", "signature": "sig"}])
    );
    assert_eq!(
        msgs[2]["tool_calls"],
        json!([{"id": "call_1", "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}])
    );
    assert_eq!(
        msgs[3],
        json!({"role": "tool", "tool_call_id": "call_1", "content": "{\"temp_c\":21}"})
    );
    assert_eq!(
        msgs[4],
        json!({"role": "tool", "tool_call_id": "call_2", "content": "line1\nline2"})
    );

    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "get_weather");
    assert_eq!(tools[0]["function"]["description"], "Current weather");
    assert_eq!(
        tools[0]["function"]["parameters"],
        weather_tool().input_schema
    );
    assert!(tools[0]["function"].get("strict").is_none());
}

#[tokio::test]
async fn strict_tool_schema_adds_strict_and_closes_every_object() {
    let b = build(
        FakeNet::sse(&plain_ok()),
        Quirks {
            strict_tool_schema: true,
            ..Quirks::default()
        },
    );
    let mut req = request(vec![Message::user_text("q")]);
    req.tools = vec![weather_tool()];
    b.provider.complete(req).await.unwrap();
    let f = &b.net.last().body["tools"][0]["function"];
    assert_eq!(f["strict"], true);
    assert_eq!(f["parameters"]["additionalProperties"], false);
    assert_eq!(
        f["parameters"]["properties"]["opts"]["additionalProperties"],
        false
    );
    assert!(
        f["parameters"]["properties"]["city"]
            .get("additionalProperties")
            .is_none()
    );
}

#[tokio::test]
async fn parsed_tool_format_renders_tools_into_the_system_prompt() {
    let b = build(
        FakeNet::sse(&plain_ok()),
        Quirks {
            tool_format: ToolFormat::Parsed("hermes".into()),
            ..Quirks::default()
        },
    );
    let mut req = request(vec![Message::user_text("q")]);
    req.system = vec![PromptBlock::new(
        PromptBlockKind::Model,
        "m",
        "Base prompt.",
    )];
    req.tools = vec![weather_tool()];
    b.provider.complete(req).await.unwrap();
    let body = b.net.last().body;
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(system.starts_with("Base prompt.\n\n# Tools"));
    assert!(system.contains("<tools>"));
    assert!(system.contains("\"name\":\"get_weather\""));
    assert!(system.contains("<tool_call>"));
}

#[tokio::test]
async fn no_system_message_when_system_is_empty_and_no_auth_header_without_bearer() {
    let b = build(FakeNet::sse(&plain_ok()), Quirks::default());
    b.provider
        .complete(request(vec![Message::user_text("q")]))
        .await
        .unwrap();
    let cap = b.net.last();
    assert!(cap.header("Authorization").is_none());
    assert_eq!(cap.body["messages"][0]["role"], "user");
    assert!(cap.body.get("tools").is_none());
    assert!(cap.body.get("temperature").is_none());
    assert!(cap.body.get("thinking").is_none());
    assert_eq!(
        b.resolver.calls(),
        0,
        "no resolver call without Bearer auth"
    );
}

#[tokio::test]
async fn thinking_is_dropped_on_replay_when_no_reasoning_field_is_named() {
    let b = build(FakeNet::sse(&plain_ok()), Quirks::default());
    let req = request(vec![
        Message::user_text("q"),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    text: "secret plan".into(),
                    signature: None,
                },
                ContentBlock::Text { text: "a".into() },
            ],
        },
        Message::user_text("more"),
    ]);
    b.provider.complete(req).await.unwrap();
    let m = &b.net.last().body["messages"][1];
    assert_eq!(m["content"], "a");
    assert!(m.get("reasoning_content").is_none());
    assert!(m.get("reasoning").is_none());
    assert!(!b.net.last().body.to_string().contains("secret plan"));
}

#[tokio::test]
async fn image_blocks_become_data_urls_from_artifact_bytes() {
    let artifacts = Arc::new(MemArtifacts::default());
    let png = b"\x89PNG\r\n\x1a\nfakebytes".to_vec();
    let handle = artifacts.put(&png, "image/png").await.unwrap();
    let net = FakeNet::sse(&plain_ok());
    let provider = OpenAiCompatProvider::new(
        "openai_compat",
        EndpointConfig::new("http://fake.local/v1"),
        Quirks::default(),
        net.clone(),
        CountingResolver::with("k"),
        artifacts.clone(),
    );
    let req = request(vec![Message {
        role: Role::User,
        content: vec![
            ContentBlock::Text {
                text: "What is this?".into(),
            },
            ContentBlock::Image {
                artifact_handle: handle,
                mime: "image/png".into(),
            },
        ],
    }]);
    provider.complete(req).await.unwrap();
    let parts = net.last().body["messages"][0]["content"].clone();
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    assert_eq!(
        parts,
        json!([
            {"type": "text", "text": "What is this?"},
            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{b64}")}}
        ])
    );

    // A missing artifact is a non-retryable client-side failure.
    let missing = ArtifactHandle(Hash::of_bytes(b"nope"));
    let req = request(vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Image {
            artifact_handle: missing,
            mime: "image/png".into(),
        }],
    }]);
    let e = provider.complete(req).await.unwrap_err();
    assert!(matches!(e, ProviderError::Client { status: 0, .. }));
    assert!(!e.retryable());
}

#[tokio::test]
async fn task_result_is_rendered_as_user_text_json() {
    let b = build(FakeNet::sse(&plain_ok()), Quirks::default());
    let block = ContentBlock::TaskResult {
        task_id: TaskId("t1".into()),
        tool_use_id: "call_9".into(),
        status: TaskStatus::Succeeded,
        content: ToolResultContent::Json(json!({"exit": 0})),
        is_error: false,
    };
    let req = request(vec![
        Message::user_text("q"),
        Message {
            role: Role::User,
            content: vec![block.clone()],
        },
    ]);
    b.provider.complete(req).await.unwrap();
    let m = &b.net.last().body["messages"][1];
    assert_eq!(m["role"], "user");
    let text = m["content"].as_str().expect("plain string content");
    let back: ContentBlock = serde_json::from_str(text).unwrap();
    assert_eq!(back, block);
}

#[tokio::test]
async fn secret_is_resolved_exactly_once_per_request_and_only_at_request_time() {
    let net = Arc::new(FakeNet::default());
    net.canned
        .lock()
        .unwrap()
        .push_back(ok(vec![plain_ok().into_bytes()]));
    net.canned
        .lock()
        .unwrap()
        .push_back(ok(vec![plain_ok().into_bytes()]));
    let resolver = CountingResolver::with("sk-live");
    let b = build_with(
        net,
        Quirks {
            auth: Auth::Bearer(SecretHandle::new("KEY")),
            ..Quirks::default()
        },
        resolver,
        None,
    );
    assert_eq!(b.resolver.calls(), 0, "construction does not resolve");
    b.provider
        .complete(request(vec![Message::user_text("1")]))
        .await
        .unwrap();
    assert_eq!(b.resolver.calls(), 1);
    assert_eq!(b.net.last().header("Authorization"), Some("Bearer sk-live"));
    b.provider
        .complete(request(vec![Message::user_text("2")]))
        .await
        .unwrap();
    assert_eq!(b.resolver.calls(), 2, "resolved again, never cached");
}

#[tokio::test]
async fn provider_name_and_debug_do_not_expose_secrets() {
    let b = build(
        FakeNet::sse(&plain_ok()),
        Quirks {
            auth: Auth::Bearer(SecretHandle::with_locator("KEY", "env:KEY")),
            ..Quirks::default()
        },
    );
    assert_eq!(b.provider.name(), "openai_compat");
    let dbg = format!("{:?}", b.provider);
    assert!(dbg.contains("SecretHandle(KEY)"));
    assert!(!dbg.contains("sk-secret-value"));
}
