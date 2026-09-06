# ADR-0003: Provider client shape — one OpenAI-compatible client with quirk flags

- **Status:** proposed
- **Date:** 2026-09-06
- **Milestone:** P0.2 / P0.4 (see `docs/IMPLEMENTATION_PLAN.md`); constrains P1.5

## Context

The kernel calls models through a `Provider` trait (dev plan §4.1). v0 must serve fine-tuned and local models on vLLM, every hosted provider through a LiteLLM proxy, and the CI stand-in (llama.cpp behind a real LiteLLM proxy, D18). All three speak OpenAI-compatible `/v1/chat/completions` with SSE, but differ in where reasoning text lives, how tool calls are surfaced, whether structured output is honoured, auth, and details of the usage chunk. Dev plan §5 proposed "one client with quirk flags"; P0.2 had to confirm that the differences are flag-sized rather than client-sized before P1.0 freezes the `Provider` signature.

Spike write-up: `docs/spikes/providers.md`. It built the client, a fake upstream emulating four streaming shapes (vLLM, LiteLLM, llama.cpp, parser-less Hermes), ran it against all four and against a real LiteLLM 1.100.0 proxy, and produced the quirk table.

## Options considered

1. **One OpenAI-compatible client parameterised by an explicit `Quirks` struct** (reasoning field, tool format, structured-output capability, stream-usage, auth, strict schema, finish-reason and fragmenting behaviour), with non-native tool syntaxes normalised by a parser middleware.
2. **One client per endpoint type** (`VllmProvider`, `LiteLlmProvider`, `LlamaCppProvider`), each hard-coding its shape.
3. **A third-party SDK** as the client (e.g. the `async-openai` crate, or making LiteLLM the only door and speaking only to it).

## Decision

Option 1. The spike showed every observed difference is expressible as a flag or a post-processing step on one wire format:

- Reasoning arrives as `reasoning_content` on vLLM, llama.cpp and through LiteLLM (verified for a vLLM-style upstream through the real proxy), as `reasoning` on a few endpoints, or inline as `<think>` when no parser is configured. A `ReasoningField` enum covers all of them and the same accumulator produces one `Thinking` block.
- Tool calls are either native index-keyed fragments (vLLM, LiteLLM), one complete chunk (older llama.cpp), or text in `<tool_call>` tags (no parser). One index-based accumulator handles the first two without a flag; the third is a `ToolFormat::Parsed(syntax)` normaliser, which is exactly the parser middleware D7 already reserves a slot for.
- Structured output, stream usage, strict schema and auth are booleans/tri-states that change the request body or headers, not the parsing.
- The probe can *derive* the row from observations, so adding an endpoint is a config change plus one probe run, not new code.

Option 2 would triplicate the SSE/accumulation code, which is where the bugs are (the spike's one real bug, mis-detecting LiteLLM's usage chunk, would have been fixed in one place instead of three). Option 3 was rejected because strongly typed SDKs drop the non-standard fields we depend on (`reasoning_content`, `provider_specific_fields`, `timings`) and their streaming types cannot represent the parser-less case; and making LiteLLM mandatory adds a Python service between the kernel and every local vLLM, which D18 only requires for the hosted path.

## Consequences

- P1.5 implements a single `OpenAiCompatProvider { endpoint, quirks }` behind the `Provider` trait; `Quirks` lives in the model profile (D7) and is content-hashed with it. The recommended trait, request/response and error shapes are in `docs/spikes/providers.md` §5.
- The `Thinking` content block is first-class from P1.0 (`ContentBlock::Thinking { text, signature: Option<String> }`) so it survives serialization and replay across providers.
- Non-native tool syntaxes are handled by a middleware in the D7 fixed early slot, not inside the provider; the provider exposes raw text plus native tool calls.
- Secrets: `Auth::Bearer(SecretHandle)` resolved through the Host at request time (D10); the provider never reads the environment.
- Always request `stream_options.include_usage`; usage is taken from any chunk that carries it (LiteLLM's chunk is not `choices: []`). Usage may differ between streaming and non-streaming through LiteLLM (it adds `reasoning_tokens` only when streaming); tests must not assert equality across modes.
- Structured output is a per-endpoint capability flag filled by a probe, with middleware fallback to prompt-and-parse when `No`/`Unknown` (dev plan §5).
- Harder: provider-specific features (Anthropic prompt caching, OpenAI Responses API, vLLM `guided_*` extras) do not fit the common body. They stay out of v0; native Anthropic/OpenAI clients remain in the Backlog ("after P2") and will be additional `Provider` implementations, not changes to this one.
- Open until the 🧑 runs in `docs/spikes/providers.md` §2.1 land: the exact `parsed(<syntax>)` list per vLLM model, and reasoning behaviour for hosted upstreams through LiteLLM. Neither changes the decision; both fill rows.
