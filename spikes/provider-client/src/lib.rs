//! P0.2 provider spike: one OpenAI-compatible `/v1/chat/completions` client
//! with per-endpoint quirk flags, plus a fake upstream that emulates the
//! streaming shapes of vLLM, LiteLLM, llama.cpp and a "parsed" (Hermes) model.
//!
//! Throwaway code. The deliverables are `docs/spikes/providers.md` and ADR-0003.

pub mod client;
pub mod fake;
pub mod hermes;
pub mod probe;
pub mod quirks;
pub mod types;
