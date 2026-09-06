//! Per-endpoint quirk flags (ADR-0003, `docs/spikes/providers.md` §4–§5).
//!
//! `Quirks` is what the model profile's `[model.quirks]` table plus `tool_format`
//! (`docs/specs/profile-schema.md` §2.1) become once the strings are parsed. The
//! `profiles` crate hands the raw strings over as a [`QuirksProfile`]; this module
//! turns them into the typed struct the client reads.

use kernel::SecretHandle;
use serde::{Deserialize, Serialize};

/// Where reasoning text arrives in a response.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningField {
    /// The endpoint emits no reasoning field; no `Thinking` block is produced.
    #[default]
    None,
    /// `delta.reasoning_content` / `message.reasoning_content` (vLLM, llama.cpp, LiteLLM).
    ReasoningContent,
    /// `delta.reasoning` / `message.reasoning` (a few OpenAI-compatible endpoints).
    Reasoning,
    /// `delta.provider_specific_fields.reasoning_content` (LiteLLM for some upstreams).
    ProviderSpecificFields,
    /// Reasoning is inline in `content` as `<think>…</think>` (vLLM without `--reasoning-parser`);
    /// the client splits it out of the final text.
    InlineThink,
}

/// How tool calls are surfaced.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFormat {
    /// Structured `tool_calls` in the response; `tools` is sent in the request.
    #[default]
    Native,
    /// Tool calls arrive as text in the named syntax (`"hermes"`, `"llama3_json"`, …). The client
    /// renders the tool definitions into the system prompt, omits `tools`/`tool_choice`, and leaves
    /// the response text untouched for the parser middleware in the D7 fixed slot.
    Parsed(String),
}

/// A capability that a probe may or may not have established.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tri {
    /// Verified supported.
    Yes,
    /// Verified unsupported.
    No,
    /// Not probed.
    #[default]
    Unknown,
}

/// How the client authenticates.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Auth {
    /// No `Authorization` header.
    #[default]
    None,
    /// `Authorization: Bearer <secret>`, resolved through the `SecretResolver` at request time (D10).
    Bearer(SecretHandle),
}

/// Per-endpoint flags, loaded from the model profile (D7).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quirks {
    /// Where reasoning text lives.
    pub reasoning_field: ReasoningField,
    /// Native tool calls or a parsed text syntax.
    pub tool_format: ToolFormat,
    /// Whether `response_format` JSON schema is honoured. Read by structured-output middleware,
    /// not by this client.
    pub supports_structured_output: Tri,
    /// Whether the endpoint emits a usage object in the stream. The client always requests it
    /// (`stream_options.include_usage`); when this is `false` the usage on the response may be
    /// all zeros.
    pub supports_stream_usage: bool,
    /// Authentication.
    pub auth: Auth,
    /// Send `strict: true` and `additionalProperties: false` tool schemas.
    pub strict_tool_schema: bool,
    /// Informational: the endpoint sets `finish_reason: "tool_calls"`. The client never relies on
    /// it to decide whether tool calls exist.
    pub sends_finish_reason_tool_calls: bool,
    /// Informational: tool calls stream as index-keyed fragments. The accumulator handles both
    /// fragments and single complete chunks regardless.
    pub streams_tool_call_fragments: bool,
}

impl Default for Quirks {
    fn default() -> Self {
        Quirks {
            reasoning_field: ReasoningField::None,
            tool_format: ToolFormat::Native,
            supports_structured_output: Tri::Unknown,
            supports_stream_usage: true,
            auth: Auth::None,
            strict_tool_schema: false,
            sends_finish_reason_tool_calls: true,
            streams_tool_call_fragments: true,
        }
    }
}

/// The plain-string form of the quirk flags as they appear in a model profile:
/// the `[model.quirks]` keys of `profile-schema.md` §2.1 plus `[model].tool_format`.
/// Unknown keys are rejected, like everywhere else in the profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuirksProfile {
    /// `"none"`, `"reasoning_content"`, `"reasoning"`, `"provider_specific_fields"`, `"inline_think"`.
    #[serde(default = "default_none")]
    pub reasoning_field: String,
    /// JSON-schema / guided decoding is available.
    #[serde(default)]
    pub supports_structured_output: bool,
    /// The endpoint emits usage in the final SSE chunk.
    #[serde(default)]
    pub supports_stream_usage: bool,
    /// Send strict tool schemas.
    #[serde(default)]
    pub strict_tool_schema: bool,
    /// `"none"` or `"bearer:<SECRET_NAME>"`.
    #[serde(default = "default_none")]
    pub auth: String,
    /// `"native"` or `"parsed:<syntax>"` (from `[model]`, not `[model.quirks]`).
    #[serde(default = "default_native")]
    pub tool_format: String,
}

fn default_none() -> String {
    "none".to_string()
}

fn default_native() -> String {
    "native".to_string()
}

impl Default for QuirksProfile {
    fn default() -> Self {
        QuirksProfile {
            reasoning_field: default_none(),
            supports_structured_output: false,
            supports_stream_usage: false,
            strict_tool_schema: false,
            auth: default_none(),
            tool_format: default_native(),
        }
    }
}

/// Why a [`QuirksProfile`] could not become a [`Quirks`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QuirksError {
    /// `reasoning_field` names a field this client does not know how to read.
    #[error(
        "unsupported reasoning_field `{0}` (expected none, reasoning_content, reasoning, provider_specific_fields, inline_think)"
    )]
    ReasoningField(String),
    /// `auth` is neither `none` nor `bearer:<SECRET_NAME>` with a well-formed name.
    #[error("invalid auth `{0}` (expected \"none\" or \"bearer:<SECRET_NAME>\")")]
    Auth(String),
    /// `tool_format` is neither `native` nor `parsed:<syntax>` with a well-formed syntax.
    #[error("invalid tool_format `{0}` (expected \"native\" or \"parsed:<syntax>\")")]
    ToolFormat(String),
}

fn is_secret_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn is_syntax_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

impl Quirks {
    /// Build the typed flags from the profile strings. `auth = "bearer:NAME"` becomes
    /// `Auth::Bearer(SecretHandle::new("NAME"))`; the value is never touched here.
    /// `supports_structured_output` maps `true`/`false` to `Tri::Yes`/`Tri::No` (the profile
    /// has no way to say `Unknown`). The two informational flags keep their defaults.
    pub fn from_profile(profile: &QuirksProfile) -> Result<Quirks, QuirksError> {
        let reasoning_field = match profile.reasoning_field.as_str() {
            "none" => ReasoningField::None,
            "reasoning_content" => ReasoningField::ReasoningContent,
            "reasoning" => ReasoningField::Reasoning,
            "provider_specific_fields" => ReasoningField::ProviderSpecificFields,
            "inline_think" => ReasoningField::InlineThink,
            other => return Err(QuirksError::ReasoningField(other.to_string())),
        };
        let auth = match profile.auth.as_str() {
            "none" => Auth::None,
            s => match s.strip_prefix("bearer:") {
                Some(name) if is_secret_name(name) => Auth::Bearer(SecretHandle::new(name)),
                _ => return Err(QuirksError::Auth(s.to_string())),
            },
        };
        let tool_format = match profile.tool_format.as_str() {
            "native" => ToolFormat::Native,
            s => match s.strip_prefix("parsed:") {
                Some(syntax) if is_syntax_name(syntax) => ToolFormat::Parsed(syntax.to_string()),
                _ => return Err(QuirksError::ToolFormat(s.to_string())),
            },
        };
        Ok(Quirks {
            reasoning_field,
            tool_format,
            supports_structured_output: if profile.supports_structured_output {
                Tri::Yes
            } else {
                Tri::No
            },
            supports_stream_usage: profile.supports_stream_usage,
            auth,
            strict_tool_schema: profile.strict_tool_schema,
            ..Quirks::default()
        })
    }
}

impl TryFrom<QuirksProfile> for Quirks {
    type Error = QuirksError;

    fn try_from(profile: QuirksProfile) -> Result<Self, Self::Error> {
        Quirks::from_profile(&profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_profile_parses_every_key() {
        let p: QuirksProfile = serde_json::from_value(serde_json::json!({
            "reasoning_field": "reasoning_content",
            "supports_structured_output": true,
            "supports_stream_usage": true,
            "strict_tool_schema": true,
            "auth": "bearer:GRIST_LITELLM_API_KEY",
            "tool_format": "parsed:hermes"
        }))
        .unwrap();
        let q = Quirks::from_profile(&p).unwrap();
        assert_eq!(q.reasoning_field, ReasoningField::ReasoningContent);
        assert_eq!(q.supports_structured_output, Tri::Yes);
        assert!(q.supports_stream_usage);
        assert!(q.strict_tool_schema);
        assert_eq!(
            q.auth,
            Auth::Bearer(SecretHandle::new("GRIST_LITELLM_API_KEY"))
        );
        assert_eq!(q.tool_format, ToolFormat::Parsed("hermes".into()));
        assert!(q.sends_finish_reason_tool_calls);
        assert!(q.streams_tool_call_fragments);
    }

    #[test]
    fn from_profile_defaults_match_the_schema_defaults() {
        let p: QuirksProfile = serde_json::from_value(serde_json::json!({})).unwrap();
        let q = Quirks::from_profile(&p).unwrap();
        assert_eq!(q.reasoning_field, ReasoningField::None);
        assert_eq!(q.tool_format, ToolFormat::Native);
        assert_eq!(q.supports_structured_output, Tri::No);
        assert!(!q.supports_stream_usage);
        assert_eq!(q.auth, Auth::None);
        assert!(!q.strict_tool_schema);
    }

    #[test]
    fn from_profile_rejects_bad_values_and_unknown_keys() {
        let bad = |v: serde_json::Value| {
            let p: QuirksProfile = serde_json::from_value(v).unwrap();
            Quirks::from_profile(&p).unwrap_err()
        };
        assert!(matches!(
            bad(serde_json::json!({"reasoning_field": "thoughts"})),
            QuirksError::ReasoningField(_)
        ));
        assert!(matches!(
            bad(serde_json::json!({"auth": "bearer:lowercase"})),
            QuirksError::Auth(_)
        ));
        assert!(matches!(
            bad(serde_json::json!({"auth": "basic:X"})),
            QuirksError::Auth(_)
        ));
        assert!(matches!(
            bad(serde_json::json!({"tool_format": "parsed:"})),
            QuirksError::ToolFormat(_)
        ));
        assert!(matches!(
            bad(serde_json::json!({"tool_format": "xml"})),
            QuirksError::ToolFormat(_)
        ));
        assert!(
            serde_json::from_value::<QuirksProfile>(serde_json::json!({"unknown_key": true}))
                .is_err()
        );
    }

    #[test]
    fn quirks_serde_round_trip_keeps_the_secret_name_only() {
        let q = Quirks {
            auth: Auth::Bearer(SecretHandle::with_locator("KEY", "env:KEY")),
            ..Quirks::default()
        };
        let json = serde_json::to_string(&q).unwrap();
        assert!(json.contains("\"KEY\""));
        assert!(!json.contains("env:KEY"));
        let back: Quirks = serde_json::from_str(&json).unwrap();
        assert_eq!(back.auth, Auth::Bearer(SecretHandle::new("KEY")));
    }
}
