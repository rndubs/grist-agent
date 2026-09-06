# P0.2 — Provider spike: one OpenAI-compatible client with quirk flags

- **Milestone:** P0.2 (feeds ADR-0003 and P1.5). Status of each checkbox is at the end.
- **Code:** `spikes/provider-client/` (throwaway; standalone Cargo package, not in the workspace).
- **Environment used here:** rustc 1.94.1, reqwest 0.13.4, axum 0.8.9, tokio 1.53.1, clap 4.6.6, toml 1.1.5;
  Python 3.11.15 with `litellm[proxy]` **1.100.0** (installed with `uv pip install "litellm[proxy]"`).
  No GPU, no Docker daemon, huggingface.co blocked: **vLLM and llama.cpp could not be run here.**
  A real LiteLLM proxy *was* run here, in front of the fake upstream (details below).

## 1. What was built

| Piece | Where | Notes |
|---|---|---|
| Client | `src/client.rs` | `POST {base_url}/chat/completions`, streaming (`stream: true`, SSE `data:` lines, `[DONE]`) and non-streaming. Index-based accumulation of `tool_calls` fragments (id / name / `arguments` string pieces), reasoning field per flag, `usage` from the body or from the trailing usage chunk (`stream_options: {include_usage: true}`). Bearer key resolved from a *named env var at request time only*, moved straight into the header, never formatted or logged (D10). |
| Quirk flags | `src/quirks.rs` | `Quirks { reasoning_field, tool_format, supports_structured_output, supports_stream_usage, auth, strict_tool_schema, sends_finish_reason_tool_calls, streams_tool_call_fragments }` plus presets `vllm`, `litellm`, `llamacpp`, `hermes`, `probe`. Loadable from TOML (`[quirks]` table overrides a preset). |
| Normalizer | `src/hermes.rs` | `parsed(hermes)`: `<tool_call>{json}</tool_call>` → tool calls (ids synthesized `call_N`), and `<think>…</think>` → thinking. Also renders the tool list into the system prompt in Hermes/Qwen wording so no `tools` field is sent (a server without `--enable-auto-tool-choice` rejects `tool_choice: auto`). |
| Normalized output | `src/types.rs` | `Response { thinking: Option<String>, text, tool_calls: Vec<{id, name, arguments: Value}>, usage }` plus `Observed { finish_reason, reasoning_fields_seen, usage_in_final_chunk, usage_chunk_choices_empty, tool_call_delta_chunks, tool_calls_parsed_from_text, … }` which the probe turns into a quirk row. |
| Fake upstream | `src/fake.rs` | axum server; `http://host/{vllm,litellm,llamacpp,hermes}/v1/chat/completions`. Emits the four streaming shapes below and the matching non-streaming bodies; honours or rejects `response_format`; the LiteLLM shape demands a bearer token; streamed bodies are cut into 41-byte chunks so SSE events straddle reads. |
| Probe | `src/probe.rs` | Scenarios: plain (non-stream), stream (usage chunk), reasoning, two-turn tool loop, `strict: true` acceptance, structured-output probe. Derives the *observed* quirk row and prints it beside the *configured* one. |
| Runner | `run-against.sh` | `fake \| standin \| standin-llamacpp \| standin-litellm \| vllm \| litellm`; env-driven; writes `out/quirks.md` and `out/<label>.json`. |
| Tests | `tests/shapes.rs` + unit tests | 6 integration + 5 unit tests, see §3. |

### Streaming shapes the fake emulates

| Shape | Reasoning | Tool calls | `finish_reason` | Usage (streaming) | Provenance of the shape |
|---|---|---|---|---|---|
| `vllm` | `delta.reasoning_content` (content `null` in those chunks) | first fragment `{index, id, type, function:{name, arguments:""}}`, then `{index, function:{arguments: piece}}` × N | `tool_calls` in its own chunk | trailing chunk, `choices: []`, only if `stream_options.include_usage` | vLLM OpenAI server docs (reasoning outputs, tool calling); expected |
| `litellm` | `delta.reasoning_content` only; non-stream `message.reasoning_content` + `message.provider_specific_fields: {refusal: null}` + `choices[].provider_specific_fields: {stop_reason: null}` | as vLLM but every fragment re-carries `"type": "function"` | `tool_calls` in its own chunk | trailing chunk with **`choices: [{index:0, delta:{}}]`** and `usage.completion_tokens_details.reasoning_tokens` (LiteLLM's own count, streaming only) | **copied from a real LiteLLM 1.100.0 run** in front of the fake `vllm` shape (§2.2) |
| `llamacpp` | `delta.reasoning_content` (`--reasoning-format deepseek`, the default under `--jinja`) | **one chunk with the complete call**, `finish_reason: tool_calls` in the same chunk | `tool_calls` | trailing chunk `choices: []` with `usage` **and `timings`**, sent regardless of `stream_options` | llama.cpp server README; expected. Newer builds also stream fragments (see note in §4) |
| `hermes` | inline `<think>…</think>` in `content` | inline `<tool_call>{"name","arguments"}</tool_call>` in `content`, split across deltas | `stop` | trailing chunk `choices: []` | a vLLM/llama.cpp server started *without* a reasoning/tool parser, or any raw Hermes/Qwen model |

## 2. How to run

```bash
cd spikes/provider-client
cargo test                                     # everything below in §3.1
./run-against.sh fake                          # CLI end to end against the fake, prints quirk rows
./run-against.sh standin                       # CI: STANDIN_OPENAI_BASE_URL STANDIN_MODEL STANDIN_LITELLM_BASE_URL STANDIN_LITELLM_KEY
./run-against.sh vllm                          # human: GRIST_VLLM_BASE_URL GRIST_VLLM_MODEL [GRIST_VLLM_API_KEY]
./run-against.sh litellm                       # human: GRIST_LITELLM_BASE_URL GRIST_LITELLM_API_KEY GRIST_LITELLM_MODEL
cargo run -- --preset vllm --base-url http://gpu:8000/v1 --model Qwen/Qwen3-8B probe --label vllm
cargo run -- --config configs/litellm.example.toml tools --dump-requests
```

Per endpoint the runner executes: `complete` (stream), `complete --no-stream`, `reasoning`, `tools` (two-turn: call → synthetic tool result → final answer), `structured`, then `probe`, which prints a `configured` and an `observed` quirk row. Exit code is non-zero if a required scenario fails; `structured` is always informational and `reasoning` is informational for the stand-in targets (small stand-in models may not be reasoning models; override with `OPTIONAL=structured`).

### 2.1 What a human needs to do (🧑 items)

**vLLM** (one GPU box, ~5 minutes):

```bash
vllm serve Qwen/Qwen3-8B --enable-auto-tool-choice --tool-call-parser hermes --reasoning-parser qwen3 --port 8000
export GRIST_VLLM_BASE_URL=http://<host>:8000/v1 GRIST_VLLM_MODEL=Qwen/Qwen3-8B
spikes/provider-client/run-against.sh vllm
```

Then repeat with the parsers removed (`vllm serve Qwen/Qwen3-8B` only) using `PRESET=hermes` to record the `parsed(hermes)` row, and, if a Llama-3.x model is in use, with `--tool-call-parser llama3_json --chat-template examples/tool_chat_template_llama3.1_json.jinja`. Paste both observed rows into §4 and replace `🧑 to verify` cells.

**LiteLLM** (proxy with at least one real upstream key, ~5 minutes):

```bash
export GRIST_LITELLM_BASE_URL=https://<proxy>/v1 GRIST_LITELLM_API_KEY=sk-... GRIST_LITELLM_MODEL=anthropic/claude-sonnet-4-5
spikes/provider-client/run-against.sh litellm
```

For the reasoning row, the proxy's `model_list` entry (or the request) must enable thinking upstream, e.g. `litellm_params: { model: anthropic/claude-sonnet-4-5, thinking: {type: enabled, budget_tokens: 1024} }` or `reasoning_effort: low` for OpenAI o-series / hosted DeepSeek-R1. Repeat with a second upstream (`openai/…`, `hosted_vllm/…`) if available.

### 2.2 What was verified here with a real LiteLLM proxy

LiteLLM 1.100.0 was started locally with `litellm --config litellm-config.yaml --port 4000` routing `fake/vllm-hosted → hosted_vllm/fake-model`, `fake/vllm-openai → openai/fake-model` (both `api_base: …/vllm/v1`) and `fake/llamacpp → openai/fake-model` (`api_base: …/llamacpp/v1`), with a `master_key`. Then `GRIST_LITELLM_BASE_URL=http://127.0.0.1:4000 GRIST_LITELLM_API_KEY=… GRIST_LITELLM_MODEL=fake/vllm-hosted ./run-against.sh litellm` ran all scenarios green. Findings, all now encoded in the fake's `litellm` shape and in the client:

1. `reasoning_content` deltas from a vLLM-style upstream pass through LiteLLM unchanged, under both `hosted_vllm/` and `openai/` providers. Non-streaming, `message.reasoning_content` is preserved.
2. `provider_specific_fields` does **not** carry the reasoning for these upstreams; it carries `{refusal: null}` on the message and `{stop_reason: null}` on the choice. The `ReasoningField::ProviderSpecificFields` variant stays in the enum for upstreams where LiteLLM documents it, but it was not observed.
3. LiteLLM's streaming usage chunk is `choices: [{index: 0, delta: {}}]`, not `choices: []`. The first version of the client keyed "usage arrived" on empty `choices` and mis-reported `supports_stream_usage=false` through LiteLLM; the detection is now "a chunk carried `usage`".
4. LiteLLM adds `usage.completion_tokens_details.reasoning_tokens` (its own tokenizer count) **only when streaming**; the non-streaming body has plain `usage`. Usage therefore differs between streaming and non-streaming through the proxy; P1.5 must not assert equality across modes.
5. Every tool-call fragment is rewritten to carry `"type": "function"`; the first fragment carries `id` and `name`; `finish_reason: tool_calls` arrives in its own chunk. The llama.cpp-style single complete chunk passes through as a single chunk (plus a separate finish chunk).
6. Without `stream_options.include_usage` LiteLLM **drops** llama.cpp's unsolicited usage/timings chunk. Always request `include_usage` through the proxy.
7. `strict: true` on a function definition is accepted (HTTP 200) and forwarded; `response_format: json_schema` is forwarded verbatim and the upstream's answer comes back intact.
8. A wrong key against a proxy that has a `master_key` but no database returns HTTP 400 `no_db_connection` rather than 401. Do not classify auth failures by status code alone.

## 3. What was verified here

### 3.1 Tests (all passing)

```
$ cd spikes/provider-client && cargo test
running 5 tests   (unit: SSE parser, <think> extraction, hermes block extraction incl. double-encoded args and unterminated blocks, structured classifier)
test result: ok. 5 passed
running 6 tests   (tests/shapes.rs)
  every_shape_normalizes_identically_with_its_preset            4 shapes × {stream, non-stream} × {plain, tool call, final after tool}: identical Response
  auto_mode_detects_reasoning_location_and_parserless_tools      probe mode records which field carried reasoning; parser-less server is detected and handled by parsed(hermes)
  stream_usage_flag_controls_the_request_and_llamacpp_sends_usage_regardless
  structured_output_probe_reports_honored_or_error               vllm/litellm/llamacpp: honored; hermes: HTTP 400 → flag `no`
  probe_derives_the_expected_quirk_row_for_each_shape            observed row == preset row for all four shapes
  api_key_is_resolved_from_env_at_request_time_and_missing_env_is_an_error
test result: ok. 6 passed
```

`cargo clippy --all-targets` and `cargo fmt --check` are clean. `./run-against.sh fake` exits 0 with four green rows; `./run-against.sh litellm` against the local real LiteLLM proxy (§2.2) exits 0.

### 3.2 The identical normalized response

For the tool-call turn every shape yields exactly:

```json
{"thinking": "The user wants the weather in Oslo, so I should call get_weather.",
 "text": "",
 "tool_calls": [{"id": "call_0", "name": "get_weather", "arguments": {"city": "Oslo", "unit": "celsius"}}],
 "usage": {"prompt_tokens": 42, "completion_tokens": 17, "total_tokens": 59}}
```

with one documented exception: the LiteLLM streaming path also has `usage.reasoning_tokens = 13` (finding 4 above).

## 4. Quirk-flag table

Legend: `verified (fake)` = asserted by a test against the emulated shape; `verified (litellm)` = observed against the real LiteLLM 1.100.0 proxy here; `expected (docs)` = from upstream documentation, not yet observed; `🧑 to verify` = needs the real endpoint.

| Flag | vLLM (`--enable-auto-tool-choice --tool-call-parser X --reasoning-parser Y`) | vLLM without parsers | LiteLLM proxy | llama.cpp stand-in (`llama-server --jinja`) |
|---|---|---|---|---|
| `reasoning_field` | `reasoning_content` — verified (fake); expected (docs: reasoning outputs page); 🧑 to verify per model/parser | `inline_think` — verified (fake); 🧑 to verify | `reasoning_content` — verified (litellm, vLLM-style upstream); 🧑 to verify for Anthropic (`thinking`) and OpenAI o-series upstreams (docs: LiteLLM normalizes both into `reasoning_content`, plus `thinking_blocks` for Anthropic) | `reasoning_content` — verified (fake); expected (docs: `--reasoning-format deepseek` default); 🧑 to verify with the stand-in model |
| `tool_format` | `native` — verified (fake); 🧑 record which models need which `--tool-call-parser` (`hermes` for Qwen2.5/Qwen3/Hermes, `llama3_json` for Llama 3.x, `mistral`, `deepseek_v3`, `qwen3_coder`, …) | `parsed(hermes)` — verified (fake): `tool_choice: auto` is rejected with 400 unless `--enable-auto-tool-choice`; client renders tools into the system prompt | `native` (passthrough) — verified (litellm) | `native` — verified (fake); expected (docs: `--jinja` enables native tool parsing for generic/hermes/llama3/functionary templates); 🧑 to verify |
| `supports_structured_output` | `yes` — verified (fake); expected (docs: `response_format: json_schema` via guided decoding; vLLM also accepts `guided_json`) ; 🧑 to verify with a reasoning parser active | `yes` (guided decoding is independent of parsers) — expected (docs) | `unknown` (upstream-dependent) — passthrough verified (litellm) | `yes` — verified (fake); expected (docs: JSON schema → GBNF grammar) ; 🧑 to verify |
| `supports_stream_usage` | `true` — verified (fake); expected (docs) | `true` | `true` — verified (litellm); usage chunk has a non-empty `choices` (finding 3) | `true`, usage chunk also carries `timings`, sent even without `stream_options` — verified (fake); 🧑 to verify |
| `auth` | `none` (or `bearer` if `--api-key`) — verified (fake) | `none` | `bearer($GRIST_LITELLM_API_KEY)` — verified (litellm) | `none` — verified (fake) |
| `strict_tool_schema` | accepted — verified (fake); 🧑 to verify (vLLM ignores `strict`; not an error) | n/a | accepted and forwarded — verified (litellm) | accepted — verified (fake); 🧑 to verify |
| `sends_finish_reason_tool_calls` | `true` — verified (fake); expected (docs) | `false` (`stop`) — verified (fake) | `true` — verified (litellm) | `true` — verified (fake); expected (docs) |
| `streams_tool_call_fragments` | `true` — verified (fake); expected (docs) | n/a (text) | `true` passthrough — verified (litellm) | `false` in the fake; **newer llama.cpp builds stream fragments** — either way the accumulator is agnostic; 🧑 record what the stand-in build does |

Model naming (dev plan §5): vLLM takes the bare served name, LiteLLM `provider/model`. The client treats the string as opaque; verified (litellm) that the proxy rewrites `model` in responses to the alias.

Upstream behaviour relied on:

- **vLLM.** Tool calling requires `--enable-auto-tool-choice --tool-call-parser <name>`; parsers include `hermes`, `llama3_json`, `mistral`, `granite`, `internlm`, `pythonic`, `deepseek_v3`, `qwen3_coder`, and the parser must match the model's chat template (some models need `--chat-template`). Without the flag, `tool_choice: "auto"` is a 400; named tool choice still works through guided decoding. `--reasoning-parser <deepseek_r1|qwen3|granite|…>` splits reasoning into `reasoning_content` on both `message` and `delta`. Structured output via `response_format` (`json_schema`, `json_object`) and vLLM-specific `guided_json`/`guided_regex`/`guided_grammar`. `stream_options.include_usage` supported.
- **LiteLLM.** Proxy `model_list` maps `model_name` (what we send) to `litellm_params.model = provider/upstream-model`; reasoning from every provider is normalized to `reasoning_content` (and `thinking_blocks` for Anthropic), enabled per request/model with `thinking: {type: enabled, budget_tokens}` or `reasoning_effort`; `drop_params: true` silently drops unsupported params. Verified locally as above.
- **llama.cpp.** `llama-server --jinja` turns on chat-template tool calling with native parsers; `--reasoning-format {none,deepseek,auto}` controls whether thinking is split into `reasoning_content`; `response_format` JSON schema is compiled to a GBNF grammar; the final streaming chunk carries `usage` and `timings`.

## 5. Recommended shape for P1.5 (`providers` crate)

```rust
/// Per-endpoint flags, loaded from the model profile (D7). Same fields as the spike,
/// with the secret as a Host handle (D10) instead of an env var name.
pub struct Quirks {
    pub reasoning_field: ReasoningField,        // None | ReasoningContent | Reasoning | ProviderSpecificFields | InlineThink
    pub tool_format: ToolFormat,                // Native | Parsed(ParsedSyntax::Hermes | Llama3Json | …)
    pub supports_structured_output: Tri,        // Yes | No | Unknown  → middleware falls back to prompt+parse
    pub supports_stream_usage: bool,
    pub auth: Auth,                             // None | Bearer(SecretHandle)
    pub strict_tool_schema: bool,
    pub sends_finish_reason_tool_calls: bool,
    pub streams_tool_call_fragments: bool,      // informational; the accumulator handles both
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;                       // "endpoint:model", the model-profile key
    fn quirks(&self) -> &Quirks;
    /// Always streams internally (usage from the trailing chunk); `sink` is for UI deltas.
    async fn complete(&self, req: &ModelRequest, sink: Option<&dyn DeltaSink>) -> Result<ModelResponse, ProviderError>;
}

pub struct ModelRequest { pub messages: Vec<Message>, pub tools: Vec<ToolSpec>, pub response_format: Option<Value>, pub sampling: Sampling }
pub struct ModelResponse {
    pub content: Vec<ContentBlock>,             // Thinking{text} | Text{text} | ToolCall{id,name,arguments} | Image{..}
    pub finish: FinishReason,                   // Stop | ToolCalls | Length | ContentFilter | Other(String)
    pub usage: Usage,                           // prompt, completion, total, reasoning: Option (D13)
    pub model: String,
    pub raw: Value,                             // hashed into the model-call event (D13), redacted by the log writer (D10)
}
pub enum ProviderError {                        // D15 retry classes
    RateLimited { retry_after: Option<Duration> }, Server { status: u16 }, Client { status: u16, body: String },
    Transport(String), Protocol(String), MissingSecret(String),
}
```

Rules that fell out of the spike:

1. Keep `tool_calls` accumulation keyed by `index`, tolerate a missing `index`, tolerate `arguments` arriving as an object, and never rely on `finish_reason` to decide whether tool calls exist.
2. `Thinking` is one content block per response, assembled from whichever location the flag names; in the `Auto`/probe mode take the first hit per delta (LiteLLM can duplicate).
3. `parsed(<syntax>)` is a middleware in the D7 fixed early slot; the provider only exposes raw text. The spike does it inside the client for brevity.
4. Always send `stream_options.include_usage`; treat "a chunk carried `usage`" as the signal, not `choices: []`.
5. Structured output is a capability probe result stored in the model profile, not detected at runtime.
6. The `Provider` never reads env vars; it asks the Host for the secret behind `Auth::Bearer(handle)` at request time.

## 6. Checkbox status for P0.2

| Task | Status |
|---|---|
| Minimal OpenAI-compatible client with SSE streaming | done (this spike, tested against four shapes and the real LiteLLM proxy) |
| 🧑 vLLM endpoint: tool calling with auto-tool-choice + parser; record `parsed(<syntax>)` models | 🧑 — run §2.1 `vllm`, paste rows |
| vLLM: `reasoning_content` captured and mapped to `Thinking` | done against the fake vLLM shape; 🧑 confirm on the real endpoint (same command) |
| 🧑 LiteLLM proxy with keys: `provider/model` routing; key + URL from config | 🧑 for real upstream keys; routing, key-from-env and URL-from-config verified against a local real proxy |
| LiteLLM: reasoning field behaviour for at least one upstream | recorded for a vLLM-style upstream through LiteLLM 1.100.0 (§2.2); 🧑 for a hosted upstream |
| Structured-output probe on both, recorded as a flag | probe built and tested; flag recorded for the fake and the local proxy; 🧑 for real vLLM / hosted upstream |
| Same client against the P0.5 llama.cpp stand-in | ready: `run-against.sh standin`; completes when the P0.5 CI job calls it |
| Table of quirk flags | §4 |
| Write-up | this file |

## 7. Open questions

1. In `parsed(hermes)` mode the spike renders tools into the system prompt and omits `tools`. Alternative: still send `tools` (so the server's chat template renders them) but omit `tool_choice`. Which one a given vLLM build does with `tool_choice` absent should be checked on the real endpoint.
2. Whether the stand-in LiteLLM route is `stand-in/<STANDIN_MODEL>` or `STANDIN_MODEL` already carries the prefix; the runner accepts `STANDIN_LITELLM_MODEL` to settle it.
3. Tool-call `id`s from llama.cpp are random strings and from parsed syntaxes are synthesized; the kernel must echo whatever id it received and never derive meaning from it.
4. Reasoning `signature` (Anthropic via LiteLLM `thinking_blocks`) is needed to replay thinking in multi-turn tool use; the `Thinking` block should carry an opaque `signature: Option<String>` from P1.5 so serialization "survives" as P1.5 requires.
