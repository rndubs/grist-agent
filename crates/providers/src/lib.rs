//! `providers` — Model clients. v0: one OpenAI-compatible client with per-endpoint quirk flags
//! (covers vLLM, LiteLLM, and the llama.cpp CI stand-in). ADR-0003; plan milestone P1.5.
//!
//! - [`OpenAiCompatProvider`] implements `kernel::Provider` for `/v1/chat/completions`. It always
//!   streams (SSE) and parses the body incrementally, emitting `ModelDelta`s and one `Complete`.
//! - [`Quirks`] carries the per-endpoint flags from the model profile (`[model.quirks]` plus
//!   `tool_format`), parsed from their string form with [`Quirks::from_profile`].
//! - Secrets are `SecretHandle`s resolved through `kernel::SecretResolver` at request time only
//!   (D10); images are read from the `ArtifactStore` and base64-encoded at request time (D15);
//!   every response carries `Usage` (D13).
//!
//! See `crates/providers/README.md` for the request/response mapping tables and how to run the
//! stand-in integration tests.

pub mod accumulate;
pub mod error;
pub mod openai_compat;
pub mod quirks;
pub mod request;
pub mod sse;

pub use openai_compat::{EndpointConfig, OpenAiCompatProvider};
pub use quirks::{Auth, Quirks, QuirksError, QuirksProfile, ReasoningField, ToolFormat, Tri};

#[cfg(test)]
mod tests;
