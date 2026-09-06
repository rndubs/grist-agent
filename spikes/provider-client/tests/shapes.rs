//! Runs the client against every fake shape, streaming and not, and asserts
//! the normalized `Response` is identical.

use provider_spike::client::{Client, Endpoint, Request};
use provider_spike::fake::{self, Shape, canned};
use provider_spike::probe::{self, StructuredOutcome};
use provider_spike::quirks::{ParsedSyntax, Quirks, ReasoningField, ToolFormat, Tri};
use provider_spike::types::{Response, ToolCall, Usage};
use serde_json::json;

/// The litellm preset authenticates with `$GRIST_LITELLM_API_KEY`; the fake's
/// LiteLLM shape rejects requests without a bearer token, so provide one.
fn ensure_test_key() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: called once, before any request task is spawned, from the test thread.
        unsafe { std::env::set_var("GRIST_LITELLM_API_KEY", "sk-fake-test-key") };
    });
}

async fn client_for(shape: Shape, base: &str, quirks: Quirks) -> Client {
    ensure_test_key();
    Client::new(Endpoint {
        base_url: format!("{base}/{}/v1", shape.name()),
        model: "fake-model".into(),
        quirks,
    })
    .unwrap()
}

fn usage() -> Option<Usage> {
    Some(Usage {
        prompt_tokens: canned::PROMPT_TOKENS,
        completion_tokens: canned::COMPLETION_TOKENS,
        total_tokens: canned::PROMPT_TOKENS + canned::COMPLETION_TOKENS,
        reasoning_tokens: None,
    })
}

/// The one place the normalized responses legitimately differ: LiteLLM adds its
/// own `reasoning_tokens` count to the streaming usage chunk only.
fn with_shape_usage(mut r: Response, shape: Shape, stream: bool) -> Response {
    if shape == Shape::Litellm && stream {
        r.usage.as_mut().unwrap().reasoning_tokens = Some(canned::LITELLM_STREAM_REASONING_TOKENS);
    }
    r
}

fn expected_plain() -> Response {
    Response {
        thinking: Some(canned::PLAIN_THINK.into()),
        text: canned::PLAIN_TEXT.into(),
        tool_calls: vec![],
        usage: usage(),
    }
}

fn expected_tool_call() -> Response {
    Response {
        thinking: Some(canned::TOOL_THINK.into()),
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: canned::TOOL_ID.into(),
            name: canned::TOOL_NAME.into(),
            arguments: json!({"city": "Oslo", "unit": "celsius"}),
        }],
        usage: usage(),
    }
}

fn expected_final() -> Response {
    Response {
        thinking: Some(canned::FINAL_THINK.into()),
        text: canned::FINAL_TEXT.into(),
        tool_calls: vec![],
        usage: usage(),
    }
}

/// Each shape with the quirks its preset says it needs.
#[tokio::test]
async fn every_shape_normalizes_identically_with_its_preset() {
    let (addr, _h) = fake::serve(0, 41).await.unwrap();
    let base = format!("http://{addr}");
    for shape in Shape::ALL {
        let q = Quirks::preset(shape.name()).unwrap();
        let c = client_for(shape, &base, q).await;
        for stream in [false, true] {
            let ctx = format!("shape={} stream={stream}", shape.name());
            let plain = c.complete(&probe::plain_request(stream)).await.expect(&ctx);
            assert_eq!(
                plain.response,
                with_shape_usage(expected_plain(), shape, stream),
                "{ctx} plain"
            );
            assert_eq!(plain.observed.streamed, stream, "{ctx}");

            let (first, second) = probe::tool_loop(&c, stream).await.expect(&ctx);
            assert_eq!(
                first.response,
                with_shape_usage(expected_tool_call(), shape, stream),
                "{ctx} tool call"
            );
            assert_eq!(
                second.expect("second turn").response,
                with_shape_usage(expected_final(), shape, stream),
                "{ctx} final"
            );

            // Shape-specific observations that back the quirk table.
            match shape {
                Shape::Vllm | Shape::Litellm => {
                    assert_eq!(
                        first.observed.finish_reason.as_deref(),
                        Some("tool_calls"),
                        "{ctx}"
                    );
                    if stream {
                        assert!(first.observed.tool_call_delta_chunks > 1, "{ctx} fragments");
                        assert!(plain.observed.usage_in_final_chunk, "{ctx} usage chunk");
                        assert_eq!(
                            plain.observed.usage_chunk_choices_empty,
                            shape == Shape::Vllm,
                            "{ctx} usage chunk choices"
                        );
                    }
                    assert!(!first.observed.tool_calls_parsed_from_text);
                }
                Shape::Llamacpp => {
                    assert_eq!(
                        first.observed.finish_reason.as_deref(),
                        Some("tool_calls"),
                        "{ctx}"
                    );
                    if stream {
                        assert_eq!(
                            first.observed.tool_call_delta_chunks, 1,
                            "{ctx} single chunk"
                        );
                        assert!(plain.observed.usage_in_final_chunk, "{ctx} usage chunk");
                        assert!(plain.observed.usage_chunk_choices_empty, "{ctx}");
                    }
                }
                Shape::Hermes => {
                    assert_eq!(
                        first.observed.finish_reason.as_deref(),
                        Some("stop"),
                        "{ctx}"
                    );
                    assert!(first.observed.tool_calls_parsed_from_text, "{ctx}");
                    assert_eq!(
                        first.observed.reasoning_fields_seen,
                        vec!["inline_think"],
                        "{ctx}"
                    );
                }
            }
        }
    }
}

/// Probe mode (`Auto` reasoning, native tools) must still normalize the
/// native shapes and must *detect* the parser-less shape rather than fail.
#[tokio::test]
async fn auto_mode_detects_reasoning_location_and_parserless_tools() {
    let (addr, _h) = fake::serve(0, 7).await.unwrap();
    let base = format!("http://{addr}");
    for shape in [Shape::Vllm, Shape::Litellm, Shape::Llamacpp] {
        let mut q = Quirks::default();
        if shape == Shape::Litellm {
            q.auth = provider_spike::quirks::Auth::Bearer {
                env: "GRIST_LITELLM_API_KEY".into(),
            };
        }
        let c = client_for(shape, &base, q).await;
        let r = c.complete(&probe::plain_request(true)).await.unwrap();
        assert_eq!(
            r.response,
            with_shape_usage(expected_plain(), shape, true),
            "{}",
            shape.name()
        );
        assert_eq!(
            r.observed.reasoning_fields_seen,
            vec!["reasoning_content"],
            "{}",
            shape.name()
        );
    }
    // Hermes shape with tool_choice=auto is rejected the way vLLM does it
    // without --enable-auto-tool-choice; with the parsed preset it works.
    let c = client_for(Shape::Hermes, &base, Quirks::default()).await;
    let err = c.complete(&probe::tools_request(true)).await.unwrap_err();
    assert!(err.to_string().contains("enable-auto-tool-choice"), "{err}");
    let q = Quirks {
        tool_format: ToolFormat::Parsed(ParsedSyntax::Hermes),
        ..Quirks::default()
    };
    let c = client_for(Shape::Hermes, &base, q).await;
    let r = c.complete(&probe::tools_request(true)).await.unwrap();
    assert_eq!(r.response, expected_tool_call());
    assert!(r.observed.tool_calls_parsed_from_text);
}

#[tokio::test]
async fn stream_usage_flag_controls_the_request_and_llamacpp_sends_usage_regardless() {
    let (addr, _h) = fake::serve(0, 64).await.unwrap();
    let base = format!("http://{addr}");
    let mut q = Quirks::preset("vllm").unwrap();
    q.supports_stream_usage = false;
    let c = client_for(Shape::Vllm, &base, q.clone()).await;
    let body = c.build_body(&probe::plain_request(true));
    assert!(body.get("stream_options").is_none());
    let r = c.complete(&probe::plain_request(true)).await.unwrap();
    assert_eq!(
        r.response.usage, None,
        "no usage without include_usage on vLLM shape"
    );

    let c = client_for(Shape::Llamacpp, &base, q).await;
    let r = c.complete(&probe::plain_request(true)).await.unwrap();
    assert_eq!(r.response.usage, usage(), "llama.cpp sends usage anyway");
}

#[tokio::test]
async fn structured_output_probe_reports_honored_or_error() {
    let (addr, _h) = fake::serve(0, 41).await.unwrap();
    let base = format!("http://{addr}");
    for shape in [Shape::Vllm, Shape::Litellm, Shape::Llamacpp] {
        let c = client_for(shape, &base, Quirks::preset(shape.name()).unwrap()).await;
        let o = probe::structured_probe(&c).await.unwrap();
        assert!(
            matches!(o, StructuredOutcome::Honored { .. }),
            "{}: {o:?}",
            shape.name()
        );
    }
    let c = client_for(Shape::Hermes, &base, Quirks::preset("hermes").unwrap()).await;
    let o = probe::structured_probe(&c).await.unwrap();
    assert!(
        matches!(o, StructuredOutcome::Error { status: 400, .. }),
        "{o:?}"
    );
}

#[tokio::test]
async fn probe_derives_the_expected_quirk_row_for_each_shape() {
    let (addr, _h) = fake::serve(0, 41).await.unwrap();
    let base = format!("http://{addr}");
    for shape in Shape::ALL {
        // Start from the preset (Auto would also work for the native shapes) and
        // check the probe reproduces the preset from observations alone.
        let preset = Quirks::preset(shape.name()).unwrap();
        let c = client_for(shape, &base, preset.clone()).await;
        let report = probe::run_all(&c).await;
        assert!(
            report.all_ok(),
            "{}: {}",
            shape.name(),
            report.markdown(shape.name())
        );
        let mut expected = preset.clone();
        // The probe records whether strict:true was *accepted*; the fake accepts it.
        expected.strict_tool_schema = preset.tool_format == ToolFormat::Native;
        // The litellm preset says `unknown` (upstream-dependent); the fake honours it.
        if shape == Shape::Litellm {
            expected.supports_structured_output = Tri::Yes;
        }
        assert_eq!(
            report.observed,
            expected,
            "{}\n{}",
            shape.name(),
            report.markdown(shape.name())
        );
        if shape == Shape::Hermes {
            assert_eq!(report.observed.supports_structured_output, Tri::No);
            assert_eq!(report.observed.reasoning_field, ReasoningField::InlineThink);
        }
    }
}

#[tokio::test]
async fn api_key_is_resolved_from_env_at_request_time_and_missing_env_is_an_error() {
    let (addr, _h) = fake::serve(0, 41).await.unwrap();
    let base = format!("http://{addr}");
    let mut q = Quirks::preset("vllm").unwrap();
    q.auth = provider_spike::quirks::Auth::Bearer {
        env: "PROVIDER_SPIKE_TEST_KEY_UNSET".into(),
    };
    let c = client_for(Shape::Vllm, &base, q).await;
    let err = c.complete(&probe::plain_request(false)).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("PROVIDER_SPIKE_TEST_KEY_UNSET"), "{msg}");
    // The request body never carries auth material.
    let body = c.build_body(&Request::default());
    assert!(body.get("api_key").is_none() && body.get("authorization").is_none());
}
