# `providers`

**Responsibility:** Model clients. v0: one OpenAI-compatible client with per-endpoint quirk flags (covers vLLM, LiteLLM, and the llama.cpp CI stand-in). Decided in ADR-0003; milestone P1.5.

**Mutable by the evolve loop?** No.

See the crate table in `docs/agent-harness-dev-plan.md` §3.1 and the dependency DAG in `crates/README.md`. Depends on `kernel` only.

## Shape

```
OpenAiCompatProvider::new(name, EndpointConfig { base_url, timeout, extra_headers }, Quirks,
                          Arc<dyn NetHandle>, Arc<dyn SecretResolver>, Arc<dyn ArtifactStore>)
    impl kernel::Provider
        complete_stream(req) -> stream of ModelDelta ending in exactly one Complete
        complete(req)        -> drives the same stream, returns the Complete payload
```

- The launcher resolves the profile's `endpoint` name to a base URL (`GRIST_ENDPOINT_<NAME>_URL`, `profile-schema.md` §2.1) and hands it over as `EndpointConfig.base_url`; the client appends `/chat/completions`. The base URL is never in a profile, so moving an endpoint changes no hash.
- Every request is `stream: true` with `stream_options.include_usage: true`. The SSE body is parsed incrementally (`data:` lines, blank-line separation, `[DONE]`, chunk boundaries anywhere including mid-UTF-8). Usage is taken from any chunk that carries `usage` (LiteLLM's usage chunk has a non-empty `choices`). EOF without `[DONE]` is tolerated when a `finish_reason` was seen; otherwise it is `InvalidResponse`.
- `EndpointConfig.timeout` is passed to the host as `HttpRequest.timeout` and also enforced here on the connect/headers phase and as an idle timeout between body chunks (`ProviderError::Timeout`). The kernel applies its own on top.

Modules: `quirks` (flags and their profile-string parser), `request` (body rendering), `sse` (parser), `accumulate` (deltas and the final response), `error` (status → `ProviderError`), `openai_compat` (the client).

## Quirk flags

`Quirks` mirrors `[model.quirks]` plus `[model].tool_format` (`profile-schema.md` §2.1). `QuirksProfile` is the plain-string form the `profiles` crate hands over; `Quirks::from_profile` / `TryFrom<QuirksProfile>` parse it (unknown keys and malformed values are errors).

| Field | Profile string | Effect in this crate |
|---|---|---|
| `reasoning_field: ReasoningField` | `"none"`, `"reasoning_content"`, `"reasoning"`, `"provider_specific_fields"`, `"inline_think"` | Where reasoning text is read from (`delta.reasoning_content`, `delta.reasoning`, `delta.provider_specific_fields.reasoning_content`, or split from a leading `<think>…</think>` in the text at the end). One `Thinking` block per response. The same field name is used to send `Thinking` blocks back on assistant messages (`provider_specific_fields` writes back as `reasoning_content`); `none`/`inline_think` drop them on replay. |
| `tool_format: ToolFormat` | `"native"`, `"parsed:<syntax>"` | `Native`: `tools[]` is sent and `tool_calls` are accumulated. `Parsed`: `tools`/`tool_choice` are not sent; the definitions are rendered into the system prompt as a `# Tools … <tools>…</tools>` block (Hermes wording for `hermes`); the response text is left untouched for the parser middleware in the D7 fixed slot. |
| `supports_structured_output: Tri` | bool → `Yes`/`No` (`Unknown` only via `Quirks::default()`) | Not read by this client; for structured-output middleware. |
| `supports_stream_usage: bool` | bool | Informational: the client always requests usage; when the endpoint does not send it, `Usage` is all zeros / `None`. |
| `auth: Auth` | `"none"`, `"bearer:<SECRET_NAME>"` | `Bearer(SecretHandle::new(NAME))`: resolved through `SecretResolver::resolve_secret` on every request, at request time, never cached, never from the environment (D10). A resolution failure is `ProviderError::Auth`. |
| `strict_tool_schema: bool` | bool | Adds `strict: true` and `additionalProperties: false` to every object node of each tool schema. |
| `sends_finish_reason_tool_calls: bool` | (not in the profile; default `true`) | Informational. `finish_reason` is never used to decide whether tool calls exist; `stop_reason` is `ToolUse` whenever native tool calls were accumulated. |
| `streams_tool_call_fragments: bool` | (not in the profile; default `true`) | Informational. The accumulator keys `tool_calls` by `index`, tolerates a missing `index` and `arguments` arriving as fragments or as an object. |

## Request mapping

| `ModelRequest` | Wire |
|---|---|
| `system` blocks | one `system` message, blocks joined with `"\n\n"`; omitted when empty; parsed tool block appended |
| user `Text` | text part (single text → plain string content) |
| user `Image{artifact_handle, mime}` | `image_url` part with a `data:<mime>;base64,…` URL from `ArtifactStore::get` at request time (D15); a missing artifact is `Client{status: 0}` |
| user `TaskResult` | a user-role text part holding the JSON of the block (`kernel-interface.md` §3.3) |
| assistant `Text` / `ToolUse` / `Thinking` | `content`, `tool_calls[{id, type: function, function{name, arguments: JSON string}}]`, reasoning field (+ `thinking_blocks` when a signature is present) |
| tool `ToolResult` | one `role: tool` message per block: `tool_call_id`, `content` = JSON string of `Json` or the joined text of `Blocks` |
| `tools` | `tools[{type: function, function{name, description, parameters[, strict]}}]` (native only); `tool_choice` is not sent (OpenAI default `auto`) |
| `params` | `temperature`, `top_p`, `max_tokens`, `stop`; `thinking.enabled` → `thinking: {type: "enabled", budget_tokens}` (LiteLLM's shape; others drop it); `extra` merged last when it is an object, so it can override anything |
| `trace.request_id` | `X-Request-Id` header (nothing else from `trace` is sent) |

## Response mapping

`stop_reason`: `stop` → `EndTurn`, `tool_calls` → `ToolUse`, `length` → `MaxTokens`, `content_filter` → `ContentFilter`, other → `Other(s)`; `stop`/absent with native tool calls present → `ToolUse`. `usage`: `prompt_tokens`, `completion_tokens`, `prompt_tokens_details.cached_tokens`, `completion_tokens_details.reasoning_tokens`. `model_id` from the first chunk's `model` (falls back to the request's). `response_id` from `id`. `raw_response_hash = Hash::of_bytes` of the `data:` payloads (without the prefix, `[DONE]` excluded) joined with `\n`. `Thinking.signature` from `thinking_blocks[].signature` (LiteLLM/Anthropic) and round-trips through serde.

Deltas: `ThinkingDelta`, `TextDelta`, `ToolUseStart` (once the name is known), `ToolUseInputDelta`, `BlockStop`, then `Complete`. Block indices follow arrival order; the final `content` is in the same order. With `inline_think` the deltas show the raw text and only the final content carries the split-out `Thinking` block.

## Errors (`ProviderError`)

429 → `RateLimited{retry_after}` (integer `Retry-After` seconds); 5xx → `Server`; 401/403 → `Auth`; 400 mentioning context length / max tokens → `ContextTooLong`; other 4xx → `Client`; transport and body failures → `Transport`; timeouts → `Timeout`; malformed SSE/JSON or a truncated stream → `InvalidResponse`; a mid-stream `{"error": …}` chunk → `Server{status: 200}`; secret resolution failure → `Auth`. Messages carry the endpoint's error text, never the resolved key. The kernel decides what to retry (`kernel-interface.md` §7.2).

## Tests

Unit tests (`src/tests.rs`) drive the client against a fake `NetHandle` serving canned SSE for the vLLM, LiteLLM, llama.cpp and parser-less Hermes shapes, error statuses, malformed and truncated streams, and assert the captured request bodies.

Integration tests (`tests/standin.rs`, feature `standin-integration`) use a test-only reqwest `NetHandle` and an env-var `SecretResolver`:

```sh
standin/up.sh && eval "$(standin/up.sh --print-env)"
cargo test -p providers --features standin-integration --test standin -- --nocapture
```

The stand-in tests skip with a notice when `STANDIN_*` are unset; in CI the hook step sets `GRIST_REQUIRE_STANDIN=1` so an unset variable fails instead. The `vllm_…` and `litellm_real_…` tests follow the environment-gate convention in `docs/standin.md` (`GRIST_VLLM_BASE_URL`, `GRIST_LITELLM_BASE_URL` + `GRIST_LITELLM_API_KEY`) and always skip when unset.
