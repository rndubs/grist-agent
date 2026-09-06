//! The resolved profile (`profile-schema.md` §7.6) — the struct whose canonical JSON is
//! `resolved_profile_hash`. Paths are symbolic (`${workdir}/…`), text sources are resolved to
//! text, `fine_tune` is excluded. Names and types are load-bearing (§12).

use std::collections::BTreeMap;

use kernel::MiddlewareSource;
use serde::{Deserialize, Serialize};

/// `[model.thinking]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thinking {
    /// Request extended reasoning.
    pub enabled: bool,
    /// Reasoning budget; `0` = provider default.
    pub budget_tokens: u64,
}

/// `[model.quirks]` — the closed per-endpoint quirk table (§2.1), plain strings for `providers`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quirks {
    /// Response field carrying reasoning text; `"none"` when absent.
    pub reasoning_field: String,
    /// JSON-schema / guided decoding available.
    pub supports_structured_output: bool,
    /// Usage object in the final SSE chunk.
    pub supports_stream_usage: bool,
    /// The endpoint validates tool-call arguments (`strict: true`).
    pub strict_tool_schema: bool,
    /// `"none"` or `"bearer:<SECRET_NAME>"`.
    pub auth: String,
}

/// `[model]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelSection {
    /// Full endpoint model string.
    pub id: String,
    /// Endpoint name (resolved to a URL by the launcher).
    pub endpoint: String,
    /// Context window in tokens.
    pub context_length: u64,
    /// `"native"` or `"parsed:<syntax>"`.
    pub tool_format: String,
    /// Absent = do not send.
    pub temperature: Option<f64>,
    /// Sent as `max_tokens`.
    pub max_output_tokens: u64,
    /// `[model.thinking]`.
    pub thinking: Thinking,
    /// `[model.quirks]`.
    pub quirks: Quirks,
}

/// `[compaction]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Compaction {
    /// Fraction of `context_length` that triggers compaction.
    pub trigger_at: f64,
    /// Fraction to compact down to.
    pub target: f64,
}

/// `[agent.eval_set]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSet {
    /// Visible development set (path, symbolic, or URL).
    pub dev: String,
    /// Held-out set (path, symbolic, or URL).
    pub hidden: String,
}

/// `[agent]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSection {
    /// Catalog entry name.
    pub name: String,
    /// UI description.
    pub description: Option<String>,
    /// Role prompt resolved to text.
    pub role_prompt: Option<String>,
    /// Symbolic path of the project instructions file.
    pub agents_md: String,
    /// Per-turn context target (D16).
    pub context_budget_tokens: u64,
    /// Spill threshold (D12).
    pub spill_cap_bytes: u64,
    /// `[agent.eval_set]`.
    pub eval_set: Option<EvalSet>,
}

/// One `[[mcp_servers]]` entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    /// Server name (`mcp.<name>.<tool>`).
    pub name: String,
    /// `"stdio"` or `"http"`.
    pub transport: String,
    /// argv for `stdio` (symbolic).
    pub command: Vec<String>,
    /// URL for `http`.
    pub url: Option<String>,
    /// Bundle-expanded, sorted, symbolic atoms.
    pub capabilities: Vec<String>,
    /// Restrict to these server-side tools; `None` = all.
    pub tools: Option<Vec<String>>,
    /// Keep schemas out of the prompt until named.
    pub lazy: bool,
    /// Extra environment.
    pub env: BTreeMap<String, String>,
}

/// One `[[subagents]]` entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subagent {
    /// Local alias.
    pub name: String,
    /// Catalog entry name.
    pub catalog: String,
    /// Ceiling, bundle-expanded, sorted, symbolic.
    pub capabilities: Vec<String>,
    /// Definition file (symbolic path).
    pub definition: Option<String>,
    /// Description.
    pub description: Option<String>,
}

/// One entry of the resolved middleware chain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MiddlewareEntry {
    /// Middleware name (a compiled implementation the launcher instantiates).
    pub name: String,
    /// Sort key.
    pub priority: i32,
    /// Contributing layer.
    pub source: MiddlewareSource,
    /// Opaque config (`{}` when absent).
    pub config: serde_json::Value,
}

/// `[memory]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    /// Module name (`"none"` = no-op).
    pub module: String,
    /// Opaque config.
    pub config: serde_json::Value,
}

/// `[sandbox]` limits (the profile-side shape; `kernel::SandboxLimits` is the kernel's).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSection {
    /// Backend name.
    pub backend: String,
    /// Wall-clock limit per invocation.
    pub timeout_s: u64,
    /// tmpfs size at `/tmp`.
    pub scratch_tmpfs_mb: u64,
    /// Environment names passed through (sorted).
    pub env_allow: Vec<String>,
    /// Network master switch.
    pub network: bool,
}

/// `[notebook]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notebook {
    /// Symbolic path.
    pub path: String,
    /// Inject block 5 on resume.
    pub inject_on_resume: bool,
    /// Size bound.
    pub max_tokens: u64,
}

/// The resolved profile (§7.6). `resolved_profile_hash = Hash::of_canonical_json(&this)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolvedProfile {
    /// Schema version (§12).
    pub schema_version: u32,
    /// The catalog entry that was resolved.
    pub catalog_entry: String,
    /// `[model]`.
    pub model: ModelSection,
    /// `[prompt]` resolved to text.
    pub prompt: Option<String>,
    /// `[tool_descriptions]` resolved to text.
    pub tool_descriptions: BTreeMap<String, String>,
    /// `[compaction]`.
    pub compaction: Compaction,
    /// `[agent]`.
    pub agent: AgentSection,
    /// Bundle-expanded, derived atoms included, symbolic, normalized, sorted, deduped.
    pub grants: Vec<String>,
    /// `[tools].allow`, sorted.
    pub tools: Vec<String>,
    /// `[[mcp_servers]]` in file order.
    pub mcp_servers: Vec<McpServer>,
    /// `[skills].paths`, symbolic.
    pub skills_paths: Vec<String>,
    /// `[[subagents]]` in file order.
    pub subagents: Vec<Subagent>,
    /// The resolved chain in sorted order, kernel entries included.
    pub middleware: Vec<MiddlewareEntry>,
    /// `[memory]`.
    pub memory: Memory,
    /// `[sandbox]`.
    pub sandbox: SandboxSection,
    /// `[notebook]`.
    pub notebook: Notebook,
    /// Layer 4, verbatim.
    pub runtime_overrides: BTreeMap<String, serde_json::Value>,
}
