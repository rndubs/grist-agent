//! Integration tests against real endpoints (feature `standin-integration`).
//!
//! Stand-in tier (`docs/standin.md`): reads `STANDIN_OPENAI_BASE_URL`, `STANDIN_LITELLM_BASE_URL`,
//! `STANDIN_LITELLM_KEY`, `STANDIN_MODEL`. Skips with a notice when they are unset, unless
//! `GRIST_REQUIRE_STANDIN=1` (what the CI hook step sets), in which case an unset variable fails.
//!
//! Environment-gated tiers (`vllm_…`, `litellm_real_…`) read `GRIST_VLLM_BASE_URL`
//! (+ optional `GRIST_VLLM_API_KEY`, `GRIST_VLLM_MODEL`) and `GRIST_LITELLM_BASE_URL` +
//! `GRIST_LITELLM_API_KEY` (+ optional `GRIST_LITELLM_MODEL`); they always skip when unset.
//!
//! The `NetHandle` and `SecretResolver` here are test-only (reqwest, env vars); the real ones
//! live in the `host` crate.
#![cfg(feature = "standin-integration")]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt};
use kernel::{
    ContentBlock, Hash, HostError, HttpMethod, HttpRequest, HttpResponse, Message, ModelDelta,
    ModelParams, ModelRequest, NetHandle, NoopArtifactStore, PromptBlock, PromptBlockKind,
    Provider, RequestTrace, Role, SecretHandle, SecretResolver, SecretString, SessionId,
    StopReason, ToolDefinition, ToolResultContent,
};
use providers::{Auth, EndpointConfig, OpenAiCompatProvider, Quirks, ReasoningField};
use serde_json::json;

// ---------------------------------------------------------------------------------------------
// Gates
// ---------------------------------------------------------------------------------------------

/// Returns the gate value or prints a skip notice and returns None.
fn gate(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => {
            eprintln!("SKIPPED: {var} unset (environment-gated tier)");
            None
        }
    }
}

/// Stand-in variables: skip when unset unless `GRIST_REQUIRE_STANDIN=1`, then fail.
fn standin(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Some(v),
        _ if std::env::var("GRIST_REQUIRE_STANDIN").as_deref() == Ok("1") => {
            panic!("{var} unset but GRIST_REQUIRE_STANDIN=1 (stand-in tests must run in CI)")
        }
        _ => {
            eprintln!("SKIPPED: {var} unset (stand-in stack not available)");
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Test-only host pieces
// ---------------------------------------------------------------------------------------------

struct ReqwestNet(reqwest::Client);

impl ReqwestNet {
    fn new() -> Arc<ReqwestNet> {
        Arc::new(ReqwestNet(reqwest::Client::new()))
    }
}

#[async_trait]
impl NetHandle for ReqwestNet {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, HostError> {
        let method = match req.method {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Put => reqwest::Method::PUT,
            HttpMethod::Delete => reqwest::Method::DELETE,
            HttpMethod::Patch => reqwest::Method::PATCH,
            HttpMethod::Head => reqwest::Method::HEAD,
        };
        let mut r = self.0.request(method, &req.url).body(req.body);
        for (k, v) in &req.headers {
            r = r.header(k.as_str(), v.as_str());
        }
        if let Some(t) = req.timeout {
            r = r.timeout(t);
        }
        let resp = r.send().await.map_err(|e| HostError::Net(e.to_string()))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    v.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let body = resp
            .bytes_stream()
            .map_ok(|b| b.to_vec())
            .map_err(|e| HostError::Net(e.to_string()));
        Ok(HttpResponse {
            status,
            headers,
            body: Box::pin(body),
        })
    }
}

/// Resolves a secret handle by reading the environment variable named by the handle.
/// Test-only: the real resolver is the `host` crate's.
struct EnvResolver;

impl SecretResolver for EnvResolver {
    fn resolve_secret(&self, handle: &SecretHandle) -> Result<SecretString, HostError> {
        std::env::var(handle.name())
            .map(SecretString::new)
            .map_err(|_| HostError::UnknownSecret(handle.name().to_string()))
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Resolves every handle to one fixed value (for the bad-key test).
struct FixedResolver(&'static str);

impl SecretResolver for FixedResolver {
    fn resolve_secret(&self, _handle: &SecretHandle) -> Result<SecretString, HostError> {
        Ok(SecretString::new(self.0))
    }
}

fn provider(base_url: &str, quirks: Quirks) -> OpenAiCompatProvider {
    provider_with_resolver(base_url, quirks, Arc::new(EnvResolver))
}

fn provider_with_resolver(
    base_url: &str,
    quirks: Quirks,
    secrets: Arc<dyn SecretResolver>,
) -> OpenAiCompatProvider {
    let endpoint = EndpointConfig {
        base_url: base_url.to_string(),
        timeout: Some(Duration::from_secs(300)),
        extra_headers: vec![],
    };
    OpenAiCompatProvider::new(
        "openai_compat",
        endpoint,
        quirks,
        ReqwestNet::new(),
        secrets,
        Arc::new(NoopArtifactStore),
    )
}

fn req(
    model_id: &str,
    system: &str,
    messages: Vec<Message>,
    tools: Vec<ToolDefinition>,
) -> ModelRequest {
    ModelRequest {
        model_id: model_id.to_string(),
        system: vec![PromptBlock::new(PromptBlockKind::Model, "test", system)],
        messages,
        tools,
        params: ModelParams {
            temperature: Some(0.0),
            max_tokens: Some(200),
            extra: json!({"seed": 7}),
            ..ModelParams::default()
        },
        trace: RequestTrace {
            session_id: SessionId("standin".into()),
            turn: 1,
            attempt: 1,
            checkpoint_hash: Hash::of_bytes(b"standin"),
            request_id: format!("standin-{}", std::process::id()),
        },
    }
}

fn weather_tool() -> ToolDefinition {
    ToolDefinition {
        name: "get_weather".into(),
        description: "Get the current weather for a city.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {"city": {"type": "string", "description": "City name"}},
            "required": ["city"]
        }),
    }
}

/// Two-turn tool call: ask, expect a `get_weather` call, answer it, expect a grounded reply.
async fn tool_roundtrip(p: &OpenAiCompatProvider, model: &str) {
    let system =
        "You are a weather assistant. Always call get_weather to answer weather questions.";
    let first = req(
        model,
        system,
        vec![Message::user_text(
            "What is the weather in Paris right now?",
        )],
        vec![weather_tool()],
    );
    // Small models occasionally answer in prose; allow a few attempts like smoke.sh does.
    let mut resp = None;
    for attempt in 0..3 {
        let r = p.complete(first.clone()).await.expect("first completion");
        eprintln!("attempt {attempt}: {:?}", r.content);
        if !r.tool_calls().is_empty() {
            resp = Some(r);
            break;
        }
    }
    let resp = resp.expect("a tool call within three attempts");
    let call = &resp.tool_calls()[0];
    assert_eq!(call.name, "get_weather");
    assert!(
        call.input["city"]
            .as_str()
            .is_some_and(|c| c.to_ascii_lowercase().contains("paris")),
        "arguments: {}",
        call.input
    );
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    assert!(resp.usage.input_tokens > 0 && resp.usage.output_tokens > 0);

    let second = req(
        model,
        system,
        vec![
            Message::user_text("What is the weather in Paris right now?"),
            Message {
                role: Role::Assistant,
                content: resp.content.clone(),
            },
            Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: call.tool_use_id.clone(),
                    content: ToolResultContent::Json(
                        json!({"city": "Paris", "temp_c": 21, "sky": "sunny"}),
                    ),
                    is_error: false,
                }],
            },
        ],
        vec![weather_tool()],
    );
    let r2 = p.complete(second).await.expect("second completion");
    let text: String = r2
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    eprintln!("final: {text}");
    assert!(
        text.contains("21") || text.to_ascii_lowercase().contains("sunny"),
        "final answer should be grounded in the tool result: {text}"
    );
    assert!(r2.usage.output_tokens > 0);
}

// ---------------------------------------------------------------------------------------------
// Stand-in tier (CI)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn standin_llama_plain_completion_streams_and_reports_usage() {
    let (Some(base), Some(model)) = (standin("STANDIN_OPENAI_BASE_URL"), standin("STANDIN_MODEL"))
    else {
        return;
    };
    let p = provider(&base, Quirks::default());
    let r = req(
        &model,
        "Answer in one short sentence.",
        vec![Message::user_text("Say hello.")],
        vec![],
    );
    let mut stream = p.complete_stream(r).await.expect("stream opens");
    let mut text_deltas = 0;
    let mut complete = None;
    while let Some(d) = stream.next().await {
        match d.expect("delta") {
            ModelDelta::TextDelta { .. } => text_deltas += 1,
            ModelDelta::Complete(r) => {
                assert!(complete.is_none(), "exactly one Complete");
                complete = Some(r);
            }
            _ => {}
        }
    }
    let resp = complete.expect("Complete delta");
    assert!(text_deltas >= 1, "text streamed as deltas");
    assert!(matches!(resp.content.first(), Some(ContentBlock::Text { text }) if !text.is_empty()));
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert!(resp.usage.input_tokens > 0, "usage from the final chunk");
    assert!(resp.usage.output_tokens > 0);
    assert!(!resp.model_id.is_empty());
    assert_ne!(resp.raw_response_hash, Hash::of_bytes(b""));
}

#[tokio::test]
async fn standin_litellm_tool_call_roundtrip_with_bearer_auth() {
    let (Some(base), Some(model), Some(_key)) = (
        standin("STANDIN_LITELLM_BASE_URL"),
        standin("STANDIN_MODEL"),
        standin("STANDIN_LITELLM_KEY"),
    ) else {
        return;
    };
    let quirks = Quirks {
        auth: Auth::Bearer(SecretHandle::new("STANDIN_LITELLM_KEY")),
        reasoning_field: ReasoningField::ReasoningContent,
        ..Quirks::default()
    };
    let p = provider(&base, quirks);
    tool_roundtrip(&p, &format!("stand-in/{model}")).await;
}

#[tokio::test]
async fn standin_litellm_rejects_a_bad_key_as_auth() {
    let (Some(base), Some(model)) = (
        standin("STANDIN_LITELLM_BASE_URL"),
        standin("STANDIN_MODEL"),
    ) else {
        return;
    };
    let quirks = Quirks {
        auth: Auth::Bearer(SecretHandle::new("BAD_KEY")),
        ..Quirks::default()
    };
    let p = provider_with_resolver(
        &base,
        quirks,
        Arc::new(FixedResolver("sk-not-the-master-key")),
    );
    let r = req(
        &format!("stand-in/{model}"),
        "",
        vec![Message::user_text("hi")],
        vec![],
    );
    let e = p.complete(r).await.expect_err("bad key is rejected");
    // A LiteLLM proxy backed by a database answers an unknown virtual key with 401 (`Auth`); the
    // CI stand-in runs LiteLLM without a database, which answers `400 No connected db` (`Client`).
    // Either way the request is refused before reaching the model and the key never leaks.
    assert!(
        matches!(
            e,
            kernel::ProviderError::Auth(_) | kernel::ProviderError::Client { status: 400, .. }
        ),
        "{e}"
    );
    assert!(!e.to_string().contains("sk-not-the-master-key"));
}

// ---------------------------------------------------------------------------------------------
// Environment-gated tiers (never set in CI)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn vllm_tool_calls_roundtrip() {
    let Some(base) = gate("GRIST_VLLM_BASE_URL") else {
        return;
    };
    let model = std::env::var("GRIST_VLLM_MODEL").unwrap_or_else(|_| "default".into());
    let auth = match std::env::var("GRIST_VLLM_API_KEY") {
        Ok(k) if !k.is_empty() => Auth::Bearer(SecretHandle::new("GRIST_VLLM_API_KEY")),
        _ => Auth::None,
    };
    let quirks = Quirks {
        auth,
        reasoning_field: ReasoningField::ReasoningContent,
        ..Quirks::default()
    };
    let p = provider(&base, quirks);
    tool_roundtrip(&p, &model).await;
}

#[tokio::test]
async fn litellm_real_tool_calls_roundtrip() {
    let (Some(base), Some(_key)) = (
        gate("GRIST_LITELLM_BASE_URL"),
        gate("GRIST_LITELLM_API_KEY"),
    ) else {
        return;
    };
    let model = std::env::var("GRIST_LITELLM_MODEL").unwrap_or_else(|_| "default".into());
    let quirks = Quirks {
        auth: Auth::Bearer(SecretHandle::new("GRIST_LITELLM_API_KEY")),
        reasoning_field: ReasoningField::ReasoningContent,
        ..Quirks::default()
    };
    let p = provider(&base, quirks);
    tool_roundtrip(&p, &model).await;
}
