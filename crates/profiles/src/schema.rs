//! Per-file validation (`profile-schema.md` §7.2 step 2) performed on the parsed `toml::Table`
//! so that every diagnostic carries the exact TOML path (`agent.contxt_budget_tokens`,
//! `middleware[0].prio`, …).
//!
//! The shape of each file kind is described by a small static schema (`TableSpec`) that a
//! generic walker checks for unknown keys, kernel-only keys, and types; the semantic rules
//! (ranges, grammars, text sources, middleware slots, layer-3 forbidden keys) follow in a
//! second pass over the same table.

use std::path::{Path, PathBuf};

use kernel::{Hash, MiddlewareSource, TOOL_CALL_PARSER_NAME};
use toml::{Table, Value};

use crate::atom::{
    self, AtomError, GrantItem, check_atom_grammar, check_path_grammar, classify, is_ident,
    is_snake_ident, is_tool_name,
};
use crate::diagnostic::{Diagnostic, idx, join};

/// The schema version this loader supports (§12).
pub const SCHEMA_VERSION: u32 = 1;

/// Which file a table came from; decides the schema and the middleware priority range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// `profiles/models/<name>.toml` (layer 1).
    Model,
    /// `profiles/agents/<name>.toml` (layer 2).
    Agent,
    /// `<workdir>/.grist/agent.toml` (layer 3).
    Project,
    /// `profiles/bundles.toml`.
    Bundles,
    /// `profiles/catalog.toml`.
    Catalog,
}

impl FileKind {
    /// Middleware priority range allowed for entries declared in this file kind (§2.7).
    pub fn priority_range(self) -> Option<std::ops::RangeInclusive<i64>> {
        match self {
            FileKind::Model => Some(100..=199),
            FileKind::Agent | FileKind::Project => Some(200..=899),
            FileKind::Bundles | FileKind::Catalog => None,
        }
    }

    /// The `MiddlewareSource` for entries declared here.
    pub fn middleware_source(self) -> MiddlewareSource {
        match self {
            FileKind::Model => MiddlewareSource::Model,
            FileKind::Agent => MiddlewareSource::Agent,
            FileKind::Project => MiddlewareSource::Project,
            FileKind::Bundles | FileKind::Catalog => MiddlewareSource::Kernel,
        }
    }
}

/// Kernel-reserved middleware names (§2.7).
pub const RESERVED_MIDDLEWARE_NAMES: &[&str] = &["recorder", "replay"];

/// Top-level kernel-only keys (§4.3), rejected in every profile file.
pub const KERNEL_ONLY_TOP: &[&str] = &[
    "loop",
    "checkpoint",
    "spill",
    "event_log",
    "hashing",
    "state",
    "record",
    "replay",
    "kernel",
    "protocol",
    "orchestrator",
    "provenance",
    "outer_sandbox",
];

/// Kernel-only keys under `[sandbox]` (§4.3).
pub const KERNEL_ONLY_SANDBOX: &[&str] = &["outer", "seccomp", "uidmap"];

/// Keys a project override may not touch (§4.2), as TOML paths.
pub const PROJECT_FORBIDDEN: &[&str] = &[
    "agent.name",
    "agent.eval_set",
    "sandbox.backend",
    "model.id",
    "model.endpoint",
    "model.context_length",
    "model.tool_format",
    "model.quirks",
    "prompt",
    "fine_tune",
];

/// Layer-4 runtime override whitelist (§4.4).
pub const RUNTIME_OVERRIDE_KEYS: &[&str] = &[
    "model.temperature",
    "model.max_output_tokens",
    "model.thinking.enabled",
    "model.thinking.budget_tokens",
    "agent.context_budget_tokens",
    "notebook.path",
];

// ---- schema description ---------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Ty {
    Str,
    Int,
    Bool,
    Num,
    StrList,
    /// A text source (§2.3): string, `{ text = … }` or `{ file = … }`.
    Text,
    /// Opaque table (middleware / memory config).
    Any,
    Table(&'static TableSpec),
    /// Array of tables.
    Tables(&'static TableSpec),
}

struct TableSpec {
    keys: &'static [(&'static str, Ty)],
    kernel_only: &'static [&'static str],
    /// Values of keys not listed in `keys`, for open tables.
    open: Option<Ty>,
}

const fn closed(keys: &'static [(&'static str, Ty)]) -> TableSpec {
    TableSpec {
        keys,
        kernel_only: &[],
        open: None,
    }
}

static QUIRKS: TableSpec = closed(&[
    ("reasoning_field", Ty::Str),
    ("supports_structured_output", Ty::Bool),
    ("supports_stream_usage", Ty::Bool),
    ("strict_tool_schema", Ty::Bool),
    ("auth", Ty::Str),
]);
static THINKING: TableSpec = closed(&[("enabled", Ty::Bool), ("budget_tokens", Ty::Int)]);
static MODEL: TableSpec = closed(&[
    ("id", Ty::Str),
    ("endpoint", Ty::Str),
    ("context_length", Ty::Int),
    ("tool_format", Ty::Str),
    ("temperature", Ty::Num),
    ("max_output_tokens", Ty::Int),
    ("thinking", Ty::Table(&THINKING)),
    ("quirks", Ty::Table(&QUIRKS)),
]);
static COMPACTION: TableSpec = closed(&[("trigger_at", Ty::Num), ("target", Ty::Num)]);
static FINE_TUNE: TableSpec = closed(&[
    ("trained_against_profile_hash", Ty::Str),
    ("warn_on_drift", Ty::Bool),
]);
static MIDDLEWARE_ENTRY: TableSpec = closed(&[
    ("name", Ty::Str),
    ("priority", Ty::Int),
    ("config", Ty::Any),
]);
static TOOL_DESCRIPTIONS: TableSpec = TableSpec {
    keys: &[],
    kernel_only: &[],
    open: Some(Ty::Text),
};
static EVAL_SET: TableSpec = closed(&[("dev", Ty::Str), ("hidden", Ty::Str)]);
static AGENT: TableSpec = closed(&[
    ("name", Ty::Str),
    ("description", Ty::Str),
    ("role_prompt", Ty::Text),
    ("agents_md", Ty::Str),
    ("context_budget_tokens", Ty::Int),
    ("spill_cap_bytes", Ty::Int),
    ("eval_set", Ty::Table(&EVAL_SET)),
]);
static CAPABILITIES: TableSpec = closed(&[("grants", Ty::StrList)]);
static TOOLS: TableSpec = closed(&[("allow", Ty::StrList)]);
static STR_MAP: TableSpec = TableSpec {
    keys: &[],
    kernel_only: &[],
    open: Some(Ty::Str),
};
static MCP_SERVER: TableSpec = closed(&[
    ("name", Ty::Str),
    ("transport", Ty::Str),
    ("command", Ty::StrList),
    ("url", Ty::Str),
    ("capabilities", Ty::StrList),
    ("tools", Ty::StrList),
    ("lazy", Ty::Bool),
    ("env", Ty::Table(&STR_MAP)),
]);
static SKILLS: TableSpec = closed(&[("paths", Ty::StrList)]);
static SUBAGENT: TableSpec = closed(&[
    ("name", Ty::Str),
    ("catalog", Ty::Str),
    ("capabilities", Ty::StrList),
    ("definition", Ty::Str),
    ("description", Ty::Str),
]);
static MEMORY: TableSpec = closed(&[("module", Ty::Str), ("config", Ty::Any)]);
static SANDBOX: TableSpec = TableSpec {
    keys: &[
        ("backend", Ty::Str),
        ("timeout_s", Ty::Int),
        ("scratch_tmpfs_mb", Ty::Int),
        ("env_allow", Ty::StrList),
        ("network", Ty::Bool),
    ],
    kernel_only: KERNEL_ONLY_SANDBOX,
    open: None,
};
static NOTEBOOK: TableSpec = closed(&[
    ("path", Ty::Str),
    ("inject_on_resume", Ty::Bool),
    ("max_tokens", Ty::Int),
]);

static MODEL_FILE: TableSpec = TableSpec {
    keys: &[
        ("schema_version", Ty::Int),
        ("model", Ty::Table(&MODEL)),
        ("prompt", Ty::Text),
        ("tool_descriptions", Ty::Table(&TOOL_DESCRIPTIONS)),
        ("compaction", Ty::Table(&COMPACTION)),
        ("fine_tune", Ty::Table(&FINE_TUNE)),
        ("middleware", Ty::Tables(&MIDDLEWARE_ENTRY)),
    ],
    kernel_only: KERNEL_ONLY_TOP,
    open: None,
};
static AGENT_FILE: TableSpec = TableSpec {
    keys: &[
        ("schema_version", Ty::Int),
        ("agent", Ty::Table(&AGENT)),
        ("capabilities", Ty::Table(&CAPABILITIES)),
        ("tools", Ty::Table(&TOOLS)),
        ("mcp_servers", Ty::Tables(&MCP_SERVER)),
        ("skills", Ty::Table(&SKILLS)),
        ("subagents", Ty::Tables(&SUBAGENT)),
        ("middleware", Ty::Tables(&MIDDLEWARE_ENTRY)),
        ("memory", Ty::Table(&MEMORY)),
        ("sandbox", Ty::Table(&SANDBOX)),
        ("notebook", Ty::Table(&NOTEBOOK)),
    ],
    kernel_only: KERNEL_ONLY_TOP,
    open: None,
};
static PROJECT_FILE: TableSpec = TableSpec {
    keys: &[
        ("schema_version", Ty::Int),
        ("model", Ty::Table(&MODEL)),
        ("prompt", Ty::Text),
        ("tool_descriptions", Ty::Table(&TOOL_DESCRIPTIONS)),
        ("compaction", Ty::Table(&COMPACTION)),
        ("fine_tune", Ty::Table(&FINE_TUNE)),
        ("agent", Ty::Table(&AGENT)),
        ("capabilities", Ty::Table(&CAPABILITIES)),
        ("tools", Ty::Table(&TOOLS)),
        ("mcp_servers", Ty::Tables(&MCP_SERVER)),
        ("skills", Ty::Table(&SKILLS)),
        ("subagents", Ty::Tables(&SUBAGENT)),
        ("middleware", Ty::Tables(&MIDDLEWARE_ENTRY)),
        ("memory", Ty::Table(&MEMORY)),
        ("sandbox", Ty::Table(&SANDBOX)),
        ("notebook", Ty::Table(&NOTEBOOK)),
    ],
    kernel_only: KERNEL_ONLY_TOP,
    open: None,
};
static BUNDLE: TableSpec = closed(&[("description", Ty::Str), ("atoms", Ty::StrList)]);
static BUNDLES_MAP: TableSpec = TableSpec {
    keys: &[],
    kernel_only: &[],
    open: Some(Ty::Table(&BUNDLE)),
};
static BUNDLES_FILE: TableSpec = TableSpec {
    keys: &[
        ("schema_version", Ty::Int),
        ("install", Ty::Table(&STR_MAP)),
        ("bundles", Ty::Table(&BUNDLES_MAP)),
    ],
    kernel_only: KERNEL_ONLY_TOP,
    open: None,
};
static CATALOG_ENTRY: TableSpec = closed(&[
    ("name", Ty::Str),
    ("description", Ty::Str),
    ("model_profile", Ty::Str),
    ("agent_profile", Ty::Str),
]);
static CATALOG_FILE: TableSpec = TableSpec {
    keys: &[
        ("schema_version", Ty::Int),
        ("agents", Ty::Tables(&CATALOG_ENTRY)),
    ],
    kernel_only: KERNEL_ONLY_TOP,
    open: None,
};

fn spec_for(kind: FileKind) -> &'static TableSpec {
    match kind {
        FileKind::Model => &MODEL_FILE,
        FileKind::Agent => &AGENT_FILE,
        FileKind::Project => &PROJECT_FILE,
        FileKind::Bundles => &BUNDLES_FILE,
        FileKind::Catalog => &CATALOG_FILE,
    }
}

// ---- output ----------------------------------------------------------------------------------

/// One `[[middleware]]` entry as declared in a file (or synthesized by the resolver).
#[derive(Clone, Debug, PartialEq)]
pub struct MwEntry {
    /// `name`
    pub name: String,
    /// `priority`
    pub priority: i64,
    /// `config` (opaque; `{}` when absent).
    pub config: Table,
    /// The layer that declared (or last overrode) the entry.
    pub source: MiddlewareSource,
    /// The declaring file (`None` for kernel entries).
    pub file: Option<PathBuf>,
    /// Index within the declaring file's `[[middleware]]` list.
    pub index: usize,
}

/// A parsed, per-file-validated profile file.
#[derive(Clone, Debug)]
pub struct ValidatedFile {
    /// Which schema it was checked against.
    pub kind: FileKind,
    /// Absolute path.
    pub path: PathBuf,
    /// `Hash::of_bytes` of the raw file bytes (§7.2 step 1).
    pub hash: Hash,
    /// The table with text sources resolved to strings, relative paths made absolute, and the
    /// `middleware` array removed (it lives in `middleware`).
    pub table: Table,
    /// The file's middleware entries in declaration order.
    pub middleware: Vec<MwEntry>,
}

/// The diagnostics collector for one file.
pub(crate) struct Cx<'a> {
    file: &'a Path,
    dir: PathBuf,
    diags: Vec<Diagnostic>,
}

impl<'a> Cx<'a> {
    /// A collector for post-merge checks that reuse the per-file rules.
    pub(crate) fn for_merged(file: &'a Path) -> Self {
        Cx {
            file,
            dir: PathBuf::from("/"),
            diags: Vec::new(),
        }
    }

    /// Take the collected diagnostics.
    pub(crate) fn into_diags(self) -> Vec<Diagnostic> {
        self.diags
    }

    fn err(&mut self, code: &'static str, path: &str, msg: impl Into<String>) {
        self.diags
            .push(Diagnostic::new(code, Some(self.file), path, msg));
    }

    fn atom_err(&mut self, path: &str, e: &AtomError) {
        self.err(e.code(), path, e.message());
    }
}

/// Parse `bytes` as TOML and validate it as a `kind` file. Returns every error of the file.
pub fn validate_file(
    kind: FileKind,
    path: &Path,
    bytes: &[u8],
) -> Result<ValidatedFile, Vec<Diagnostic>> {
    let hash = Hash::of_bytes(bytes);
    let text = match std::str::from_utf8(bytes) {
        Ok(t) => t,
        Err(e) => {
            return Err(vec![Diagnostic::new(
                "E_TOML_PARSE",
                Some(path),
                "",
                format!("not UTF-8: {e}"),
            )]);
        }
    };
    let mut table: Table = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            return Err(vec![Diagnostic::new(
                "E_TOML_PARSE",
                Some(path),
                "",
                e.message().to_owned(),
            )]);
        }
    };
    let mut cx = Cx {
        file: path,
        dir: path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/")),
        diags: Vec::new(),
    };
    check_schema_version(&table, &mut cx);
    walk_table(&table, spec_for(kind), "", &mut cx);
    let mut middleware = Vec::new();
    match kind {
        FileKind::Model => {
            check_model_semantics(&mut table, &mut cx);
            middleware = check_middleware(&table, kind, &mut cx);
        }
        FileKind::Agent => {
            check_agent_semantics(&mut table, &mut cx);
            middleware = check_middleware(&table, kind, &mut cx);
        }
        FileKind::Project => {
            check_project_forbidden(&table, &mut cx);
            check_model_semantics(&mut table, &mut cx);
            check_agent_semantics(&mut table, &mut cx);
            middleware = check_middleware(&table, kind, &mut cx);
        }
        FileKind::Bundles => check_bundles_semantics(&table, &mut cx),
        FileKind::Catalog => check_catalog_semantics(&table, &mut cx),
    }
    table.remove("middleware");
    if cx.diags.iter().any(Diagnostic::is_error) {
        return Err(cx.diags);
    }
    Ok(ValidatedFile {
        kind,
        path: path.to_path_buf(),
        hash,
        table,
        middleware,
    })
}

fn check_schema_version(t: &Table, cx: &mut Cx<'_>) {
    match t.get("schema_version") {
        None => cx.err("E_SCHEMA_VERSION", "schema_version", "missing"),
        Some(Value::Integer(v)) => {
            if *v > i64::from(SCHEMA_VERSION) {
                cx.err(
                    "E_SCHEMA_VERSION",
                    "schema_version",
                    format!("{v} unsupported (max {SCHEMA_VERSION})"),
                );
            } else if *v < 1 {
                cx.err(
                    "E_SCHEMA_VERSION",
                    "schema_version",
                    format!("{v} unsupported (no migration registered)"),
                );
            }
        }
        Some(_) => cx.err("E_SCHEMA_VERSION", "schema_version", "must be an integer"),
    }
}

// ---- generic walker --------------------------------------------------------------------------

fn walk_table(t: &Table, spec: &TableSpec, path: &str, cx: &mut Cx<'_>) {
    for (k, v) in t {
        let kp = join(path, k);
        if path.is_empty() && k == "schema_version" {
            continue; // checked separately with its own code
        }
        if let Some((_, ty)) = spec.keys.iter().find(|(name, _)| name == k) {
            walk_value(v, *ty, &kp, cx);
        } else if spec.kernel_only.contains(&k.as_str()) {
            cx.err(
                "E_KERNEL_ONLY_KEY",
                &kp,
                "this names a kernel behaviour; profiles cannot override it",
            );
        } else if let Some(ty) = spec.open {
            walk_value(v, ty, &kp, cx);
        } else {
            cx.err("E_UNKNOWN_KEY", &kp, "unknown key");
        }
    }
}

fn walk_value(v: &Value, ty: Ty, path: &str, cx: &mut Cx<'_>) {
    match ty {
        Ty::Str => {
            if !v.is_str() {
                cx.err("E_VALUE_RANGE", path, "must be a string");
            }
        }
        Ty::Int => {
            if !v.is_integer() {
                cx.err("E_VALUE_RANGE", path, "must be an integer");
            }
        }
        Ty::Bool => {
            if !v.is_bool() {
                cx.err("E_VALUE_RANGE", path, "must be a boolean");
            }
        }
        Ty::Num => {
            if !(v.is_float() || v.is_integer()) {
                cx.err("E_VALUE_RANGE", path, "must be a number");
            }
        }
        Ty::StrList => match v.as_array() {
            Some(items) => {
                for (i, item) in items.iter().enumerate() {
                    if !item.is_str() {
                        cx.err("E_VALUE_RANGE", &idx(path, i), "must be a string");
                    }
                }
            }
            None => cx.err("E_VALUE_RANGE", path, "must be a list of strings"),
        },
        Ty::Text => match v {
            Value::String(_) => {}
            Value::Table(t) => {
                let keys: Vec<&String> = t.keys().collect();
                let ok = keys.len() == 1
                    && (keys[0] == "text" || keys[0] == "file")
                    && t.values().all(Value::is_str);
                if !ok {
                    cx.err("E_TEXT_SOURCE", path, "exactly one of 'text' or 'file'");
                }
            }
            _ => cx.err("E_TEXT_SOURCE", path, "exactly one of 'text' or 'file'"),
        },
        Ty::Any => {
            if !v.is_table() {
                cx.err("E_VALUE_RANGE", path, "must be a table");
            }
        }
        Ty::Table(spec) => match v.as_table() {
            Some(t) => walk_table(t, spec, path, cx),
            None => cx.err("E_VALUE_RANGE", path, "must be a table"),
        },
        Ty::Tables(spec) => match v.as_array() {
            Some(items) => {
                for (i, item) in items.iter().enumerate() {
                    let ip = idx(path, i);
                    match item.as_table() {
                        Some(t) => walk_table(t, spec, &ip, cx),
                        None => cx.err("E_VALUE_RANGE", &ip, "must be a table"),
                    }
                }
            }
            None => cx.err("E_VALUE_RANGE", path, "must be an array of tables"),
        },
    }
}

// ---- helpers ---------------------------------------------------------------------------------

fn get<'t>(t: &'t Table, dotted: &str) -> Option<&'t Value> {
    let mut segs = dotted.split('.');
    let mut node = t.get(segs.next()?)?;
    for seg in segs {
        node = node.as_table()?.get(seg)?;
    }
    Some(node)
}

fn get_mut<'t>(t: &'t mut Table, dotted: &str) -> Option<&'t mut Value> {
    let mut segs = dotted.split('.');
    let first = segs.next()?;
    let mut node = t.get_mut(first)?;
    for seg in segs {
        node = node.as_table_mut()?.get_mut(seg)?;
    }
    Some(node)
}

fn int(t: &Table, dotted: &str) -> Option<i64> {
    get(t, dotted).and_then(Value::as_integer)
}

fn num(t: &Table, dotted: &str) -> Option<f64> {
    match get(t, dotted)? {
        Value::Float(f) => Some(*f),
        Value::Integer(i) => Some(*i as f64),
        _ => None,
    }
}

fn string<'t>(t: &'t Table, dotted: &str) -> Option<&'t str> {
    get(t, dotted).and_then(Value::as_str)
}

fn boolean(t: &Table, dotted: &str) -> Option<bool> {
    get(t, dotted).and_then(Value::as_bool)
}

fn str_list<'t>(t: &'t Table, dotted: &str) -> Vec<(usize, &'t str)> {
    get(t, dotted)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .enumerate()
                .filter_map(|(i, v)| v.as_str().map(|s| (i, s)))
                .collect()
        })
        .unwrap_or_default()
}

fn require_positive(t: &Table, dotted: &str, cx: &mut Cx<'_>) {
    if let Some(v) = int(t, dotted)
        && v <= 0
    {
        cx.err("E_VALUE_RANGE", dotted, "must be > 0");
    }
}

/// Resolve a text source at `dotted` (string, `{text}`, or `{file}`) to an inline string,
/// rewriting the table. Reports `E_FILE_NOT_FOUND at <path>.file`.
fn resolve_text_source(t: &mut Table, dotted: &str, cx: &mut Cx<'_>) {
    let Some(v) = get_mut(t, dotted) else { return };
    let Value::Table(src) = v else { return };
    let replacement = if let Some(Value::String(s)) = src.get("text") {
        Some(s.clone())
    } else if let Some(Value::String(f)) = src.get("file") {
        let full = cx.dir.join(f);
        match std::fs::read_to_string(&full) {
            Ok(s) => Some(s),
            Err(_) => {
                cx.err(
                    "E_FILE_NOT_FOUND",
                    &format!("{dotted}.file"),
                    format!("'{f}'"),
                );
                None
            }
        }
    } else {
        None
    };
    if let Some(s) = replacement
        && let Some(v) = get_mut(t, dotted)
    {
        *v = Value::String(s);
    }
}

fn is_url(s: &str) -> bool {
    s.contains("://")
}

/// Check the grammar of a path-typed field and make a relative (non-placeholder, non-URL) path
/// absolute against the containing file (§1.3 step 6).
fn resolve_path_field(v: &mut Value, dotted: &str, allow_url: bool, cx: &mut Cx<'_>) {
    let Value::String(s) = v else { return };
    if allow_url && is_url(s) {
        return;
    }
    if let Err(e) = check_path_grammar(s) {
        cx.atom_err(dotted, &e);
        return;
    }
    if !s.starts_with("${") && !s.starts_with('/') {
        *s = cx.dir.join(&*s).to_string_lossy().into_owned();
    }
}

fn resolve_path_at(t: &mut Table, dotted: &str, allow_url: bool, cx: &mut Cx<'_>) {
    if let Some(v) = get_mut(t, dotted) {
        resolve_path_field(v, dotted, allow_url, cx);
    }
}

fn check_grant_list(t: &Table, dotted: &str, allow_bundles: bool, cx: &mut Cx<'_>) {
    for (i, s) in str_list(t, dotted) {
        let p = idx(dotted, i);
        match classify(s) {
            Err(e) => cx.atom_err(&p, &e),
            Ok(GrantItem::Bundle(_)) => {
                if !allow_bundles {
                    cx.err(
                        "E_MALFORMED_ATOM",
                        &p,
                        format!("'{s}' (bundles may not nest)"),
                    );
                }
            }
            Ok(GrantItem::Atom { prefix, .. }) => {
                if let Err(e) = check_atom_grammar(s) {
                    cx.atom_err(&p, &e);
                } else if dotted == "capabilities.grants" || !allow_bundles {
                    if prefix == "tool" {
                        cx.err("E_DERIVED_ATOM_IN_GRANTS", &p, "use [tools].allow");
                    } else if prefix == "spawn" {
                        cx.err("E_DERIVED_ATOM_IN_GRANTS", &p, "use [[subagents]]");
                    }
                }
            }
        }
    }
}

fn check_env_names(names: Vec<(String, String)>, cx: &mut Cx<'_>) {
    for (path, name) in names {
        if kernel::sandbox::env_name_is_secret_like(&name) {
            cx.err("E_ENV_ALLOW_SECRET_PATTERN", &path, format!("'{name}'"));
        }
    }
}

// ---- model file semantics --------------------------------------------------------------------

fn check_model_semantics(t: &mut Table, cx: &mut Cx<'_>) {
    if let Some(id) = string(t, "model.id")
        && id.is_empty()
    {
        cx.err("E_VALUE_RANGE", "model.id", "must not be empty");
    }
    if let Some(ep) = string(t, "model.endpoint")
        && !atom::is_endpoint_name(ep)
    {
        cx.err(
            "E_VALUE_RANGE",
            "model.endpoint",
            "must match [a-z][a-z0-9-]*",
        );
    }
    require_positive(t, "model.context_length", cx);
    if let Some(tf) = string(t, "model.tool_format")
        && !tool_format_is_valid(tf)
    {
        cx.err(
            "E_VALUE_RANGE",
            "model.tool_format",
            "must be \"native\" or \"parsed:<syntax>\"",
        );
    }
    if let Some(temp) = num(t, "model.temperature")
        && !(0.0..=2.0).contains(&temp)
    {
        cx.err("E_VALUE_RANGE", "model.temperature", "must be in [0, 2]");
    }
    require_positive(t, "model.max_output_tokens", cx);
    if let Some(b) = int(t, "model.thinking.budget_tokens") {
        if b < 0 {
            cx.err(
                "E_VALUE_RANGE",
                "model.thinking.budget_tokens",
                "must be >= 0",
            );
        } else if b > 0 && boolean(t, "model.thinking.enabled") == Some(false) {
            cx.err(
                "E_VALUE_RANGE",
                "model.thinking.budget_tokens",
                "must be 0 when enabled = false",
            );
        }
    }
    if let Some(rf) = string(t, "model.quirks.reasoning_field")
        && rf != "none"
        && !is_snake_ident(rf)
    {
        cx.err(
            "E_VALUE_RANGE",
            "model.quirks.reasoning_field",
            "must be \"none\" or match [a-z][a-z0-9_]*",
        );
    }
    if let Some(auth) = string(t, "model.quirks.auth")
        && !auth_is_valid(auth)
    {
        cx.err(
            "E_VALUE_RANGE",
            "model.quirks.auth",
            "must be \"none\" or \"bearer:<SECRET_NAME>\"",
        );
    }
    resolve_text_source(t, "prompt", cx);
    let desc_keys: Vec<String> = get(t, "tool_descriptions")
        .and_then(Value::as_table)
        .map(|d| d.keys().cloned().collect())
        .unwrap_or_default();
    for k in desc_keys {
        if !is_tool_name(&k) {
            cx.err(
                "E_VALUE_RANGE",
                &format!("tool_descriptions.{k}"),
                "not a tool name",
            );
        }
        resolve_text_source(t, &format!("tool_descriptions.{k}"), cx);
    }
    check_compaction(t, cx);
    if let Some(h) = string(t, "fine_tune.trained_against_profile_hash")
        && Hash::parse(h).is_err()
    {
        cx.err(
            "E_VALUE_RANGE",
            "fine_tune.trained_against_profile_hash",
            "must be a b3:<64 hex> hash string",
        );
    }
}

/// `native` or `parsed:<[a-z][a-z0-9_-]*>`.
pub fn tool_format_is_valid(tf: &str) -> bool {
    tf == "native" || tf.strip_prefix("parsed:").is_some_and(is_ident)
}

/// `none` or `bearer:<[A-Z][A-Z0-9_]*>`.
pub fn auth_is_valid(auth: &str) -> bool {
    auth == "none"
        || auth
            .strip_prefix("bearer:")
            .is_some_and(atom::is_secret_name_upper)
}

/// `0 < target < trigger_at <= 1.0` over the keys present in `t`. Shared with the post-merge check.
pub(crate) fn check_compaction(t: &Table, cx: &mut Cx<'_>) {
    let trigger = num(t, "compaction.trigger_at");
    let target = num(t, "compaction.target");
    if let Some(tr) = trigger
        && !(tr > 0.0 && tr <= 1.0)
    {
        cx.err(
            "E_VALUE_RANGE",
            "compaction.trigger_at",
            "must satisfy 0 < trigger_at <= 1.0",
        );
    }
    if let Some(ta) = target {
        if !(ta > 0.0 && ta < 1.0) {
            cx.err(
                "E_VALUE_RANGE",
                "compaction.target",
                "must satisfy 0 < target < trigger_at",
            );
        } else if let Some(tr) = trigger
            && ta >= tr
        {
            cx.err(
                "E_VALUE_RANGE",
                "compaction.target",
                format!("must satisfy 0 < target < trigger_at ({ta} >= {tr})"),
            );
        }
    }
}

// ---- agent file semantics --------------------------------------------------------------------

fn check_agent_semantics(t: &mut Table, cx: &mut Cx<'_>) {
    if let Some(n) = string(t, "agent.name")
        && !is_ident(n)
    {
        cx.err("E_VALUE_RANGE", "agent.name", "must match [a-z][a-z0-9_-]*");
    }
    resolve_text_source(t, "agent.role_prompt", cx);
    resolve_path_at(t, "agent.agents_md", false, cx);
    require_positive(t, "agent.context_budget_tokens", cx);
    if let Some(v) = int(t, "agent.spill_cap_bytes")
        && !(1024..=1_048_576).contains(&v)
    {
        cx.err(
            "E_VALUE_RANGE",
            "agent.spill_cap_bytes",
            "must be in [1024, 1048576]",
        );
    }
    if get(t, "agent.eval_set").is_some() {
        for k in ["dev", "hidden"] {
            let p = format!("agent.eval_set.{k}");
            if get(t, &p).is_none() {
                cx.err(
                    "E_MISSING_KEY",
                    &p,
                    "required when [agent.eval_set] is present",
                );
            } else {
                resolve_path_at(t, &p, true, cx);
            }
        }
    }
    check_grant_list(t, "capabilities.grants", true, cx);
    for (i, name) in str_list(t, "tools.allow") {
        if !is_tool_name(name) {
            cx.err("E_VALUE_RANGE", &idx("tools.allow", i), "not a tool name");
        }
    }
    check_mcp_servers(t, cx);
    let n_skills = str_list(t, "skills.paths").len();
    for i in 0..n_skills {
        if let Some(Value::Array(a)) = get_mut(t, "skills.paths")
            && let Some(v) = a.get_mut(i)
        {
            resolve_path_field(v, &idx("skills.paths", i), false, cx);
        }
    }
    check_subagents(t, cx);
    if let Some(m) = string(t, "memory.module")
        && !is_ident(m)
    {
        cx.err(
            "E_VALUE_RANGE",
            "memory.module",
            "must match [a-z][a-z0-9_-]*",
        );
    }
    if let Some(b) = string(t, "sandbox.backend")
        && !is_ident(b)
    {
        cx.err(
            "E_VALUE_RANGE",
            "sandbox.backend",
            "must match [a-z][a-z0-9_-]*",
        );
    }
    require_positive(t, "sandbox.timeout_s", cx);
    require_positive(t, "sandbox.scratch_tmpfs_mb", cx);
    let env: Vec<(String, String)> = str_list(t, "sandbox.env_allow")
        .into_iter()
        .map(|(i, s)| (idx("sandbox.env_allow", i), s.to_owned()))
        .collect();
    check_env_names(env, cx);
    resolve_path_at(t, "notebook.path", false, cx);
    require_positive(t, "notebook.max_tokens", cx);
}

fn check_mcp_servers(t: &mut Table, cx: &mut Cx<'_>) {
    let Some(Value::Array(servers)) = t.get("mcp_servers") else {
        return;
    };
    let mut seen: Vec<String> = Vec::new();
    for (i, s) in servers.iter().enumerate() {
        let Some(s) = s.as_table() else { continue };
        let p = idx("mcp_servers", i);
        match s.get("name").and_then(Value::as_str) {
            None => cx.err("E_MISSING_KEY", &format!("{p}.name"), "required"),
            Some(n) if !is_ident(n) => cx.err(
                "E_VALUE_RANGE",
                &format!("{p}.name"),
                "must match [a-z][a-z0-9_-]*",
            ),
            Some(n) if seen.iter().any(|x| x == n) => {
                cx.err("E_DUP_NAME", &format!("{p}.name"), format!("'{n}'"));
            }
            Some(n) => seen.push(n.to_owned()),
        }
        let transport = s.get("transport").and_then(Value::as_str);
        match transport {
            None => cx.err("E_MISSING_KEY", &format!("{p}.transport"), "required"),
            Some("stdio") => {
                if s.get("command")
                    .and_then(Value::as_array)
                    .is_none_or(Vec::is_empty)
                {
                    cx.err(
                        "E_MISSING_KEY",
                        &format!("{p}.command"),
                        "required for transport = \"stdio\"",
                    );
                }
            }
            Some("http") => {
                if s.get("url").and_then(Value::as_str).is_none() {
                    cx.err(
                        "E_MISSING_KEY",
                        &format!("{p}.url"),
                        "required for transport = \"http\"",
                    );
                }
            }
            Some(_) => cx.err(
                "E_VALUE_RANGE",
                &format!("{p}.transport"),
                "must be \"stdio\" or \"http\"",
            ),
        }
        if let Some(Value::Array(cmd)) = s.get("command") {
            for (j, a) in cmd.iter().enumerate() {
                if let Some(a) = a.as_str()
                    && a.contains("${")
                    && let Err(e) = check_path_grammar(a)
                {
                    cx.atom_err(&idx(&format!("{p}.command"), j), &e);
                }
            }
        }
        let caps_path = format!("{p}.capabilities");
        match s.get("capabilities").and_then(Value::as_array) {
            None => cx.err("E_MISSING_KEY", &caps_path, "required"),
            Some(caps) => {
                let mut sub = Cx {
                    file: cx.file,
                    dir: cx.dir.clone(),
                    diags: Vec::new(),
                };
                check_grant_list(s, "capabilities", true, &mut sub);
                for mut d in sub.diags {
                    d.toml_path = format!("{p}.{}", d.toml_path);
                    cx.diags.push(d);
                }
                if transport == Some("stdio")
                    && let Some(prog) = s
                        .get("command")
                        .and_then(Value::as_array)
                        .and_then(|c| c.first())
                        .and_then(Value::as_str)
                {
                    let want = format!("proc:{prog}");
                    if !caps.iter().any(|c| c.as_str() == Some(want.as_str())) {
                        cx.err(
                            "E_VALUE_RANGE",
                            &caps_path,
                            format!("must include '{want}' for a stdio server"),
                        );
                    }
                }
            }
        }
        if let Some(Value::Table(env)) = s.get("env") {
            let names = env
                .keys()
                .map(|k| (format!("{p}.env.{k}"), k.clone()))
                .collect();
            check_env_names(names, cx);
        }
    }
}

fn check_subagents(t: &mut Table, cx: &mut Cx<'_>) {
    let n = t
        .get("subagents")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let mut seen: Vec<String> = Vec::new();
    for i in 0..n {
        let p = idx("subagents", i);
        let Some(Value::Array(list)) = t.get_mut("subagents") else {
            return;
        };
        let Some(Value::Table(s)) = list.get_mut(i) else {
            continue;
        };
        match s.get("name").and_then(Value::as_str) {
            None => cx.err("E_MISSING_KEY", &format!("{p}.name"), "required"),
            Some(n) if seen.iter().any(|x| x == n) => {
                cx.err("E_DUP_NAME", &format!("{p}.name"), format!("'{n}'"));
            }
            Some(n) => seen.push(n.to_owned()),
        }
        match s.get("catalog").and_then(Value::as_str) {
            None => cx.err("E_MISSING_KEY", &format!("{p}.catalog"), "required"),
            Some(c) if !is_ident(c) => cx.err(
                "E_VALUE_RANGE",
                &format!("{p}.catalog"),
                "must match [a-z][a-z0-9_-]*",
            ),
            Some(_) => {}
        }
        if s.get("capabilities").is_none() {
            cx.err("E_MISSING_KEY", &format!("{p}.capabilities"), "required");
        } else {
            let mut sub = Cx {
                file: cx.file,
                dir: cx.dir.clone(),
                diags: Vec::new(),
            };
            check_grant_list(s, "capabilities", true, &mut sub);
            for mut d in sub.diags {
                d.toml_path = format!("{p}.{}", d.toml_path);
                cx.diags.push(d);
            }
        }
        if let Some(v) = s.get_mut("definition") {
            resolve_path_field(v, &format!("{p}.definition"), false, cx);
        }
    }
}

// ---- middleware ------------------------------------------------------------------------------

fn check_middleware(t: &Table, kind: FileKind, cx: &mut Cx<'_>) -> Vec<MwEntry> {
    let mut out = Vec::new();
    let Some(Value::Array(entries)) = t.get("middleware") else {
        return out;
    };
    let range = kind.priority_range().unwrap_or(0..=0);
    let tool_format = string(t, "model.tool_format").unwrap_or("native");
    let parsed_syntax = tool_format.strip_prefix("parsed:");
    for (i, e) in entries.iter().enumerate() {
        let Some(e) = e.as_table() else { continue };
        let p = idx("middleware", i);
        let name_path = format!("{p}.name");
        let prio_path = format!("{p}.priority");
        let Some(name) = e.get("name").and_then(Value::as_str) else {
            cx.err("E_MISSING_KEY", &name_path, "required");
            continue;
        };
        let Some(priority) = e.get("priority").and_then(Value::as_integer) else {
            cx.err("E_MISSING_KEY", &prio_path, "required");
            continue;
        };
        if !is_snake_ident(name) {
            cx.err("E_VALUE_RANGE", &name_path, "must match [a-z][a-z0-9_]*");
        }
        if RESERVED_MIDDLEWARE_NAMES.contains(&name) {
            cx.err(
                "E_RESERVED_MIDDLEWARE_NAME",
                &name_path,
                format!("'{name}'"),
            );
        }
        if out.iter().any(|m: &MwEntry| m.name == name) {
            cx.err("E_DUP_MIDDLEWARE", &name_path, format!("'{name}'"));
        }
        if name == TOOL_CALL_PARSER_NAME {
            match kind {
                FileKind::Model => match parsed_syntax {
                    None => cx.err(
                        "E_PARSER_SLOT",
                        &name_path,
                        "no parser without parsed tool_format",
                    ),
                    Some(syntax) => {
                        if priority != i64::from(kernel::TOOL_CALL_PARSER_PRIORITY) {
                            cx.err(
                                "E_PARSER_SLOT",
                                &prio_path,
                                format!(
                                    "{TOOL_CALL_PARSER_NAME} must be {}",
                                    kernel::TOOL_CALL_PARSER_PRIORITY
                                ),
                            );
                        }
                        if let Some(declared) = e
                            .get("config")
                            .and_then(Value::as_table)
                            .and_then(|c| c.get("syntax"))
                            .and_then(Value::as_str)
                            && declared != syntax
                        {
                            cx.err(
                                "E_PARSER_SLOT",
                                &format!("{p}.config.syntax"),
                                format!("'{declared}' != tool_format syntax '{syntax}'"),
                            );
                        }
                    }
                },
                _ => cx.err(
                    "E_PARSER_SLOT",
                    &name_path,
                    "only the model profile may declare tool_call_parser",
                ),
            }
        } else if !range.contains(&priority) {
            cx.err(
                "E_PRIORITY_RANGE",
                &prio_path,
                format!("{priority} not in {}..={}", range.start(), range.end()),
            );
        }
        let config = e
            .get("config")
            .and_then(Value::as_table)
            .cloned()
            .unwrap_or_default();
        out.push(MwEntry {
            name: name.to_owned(),
            priority,
            config,
            source: kind.middleware_source(),
            file: Some(cx.file.to_path_buf()),
            index: i,
        });
    }
    out
}

// ---- project overrides -----------------------------------------------------------------------

fn check_project_forbidden(t: &Table, cx: &mut Cx<'_>) {
    for key in PROJECT_FORBIDDEN {
        if get(t, key).is_some() {
            cx.err(
                "E_OVERRIDE_FORBIDDEN_KEY",
                key,
                "not overridable from the working directory (profile-schema.md §4.2)",
            );
        }
    }
}

// ---- bundles ---------------------------------------------------------------------------------

fn check_bundles_semantics(t: &Table, cx: &mut Cx<'_>) {
    let install: Vec<(String, String)> = get(t, "install")
        .and_then(Value::as_table)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    for (k, v) in &install {
        let p = format!("install.{k}");
        if !is_ident(k) {
            cx.err(
                "E_VALUE_RANGE",
                &p,
                "install keys must match [a-z][a-z0-9_-]*",
            );
        }
        if let Err(e) = atom::normalize_abs_path(v) {
            cx.atom_err(&p, &e);
        }
    }
    let Some(bundles) = get(t, "bundles").and_then(Value::as_table) else {
        return;
    };
    for (name, b) in bundles {
        let p = format!("bundles.{name}");
        if !is_ident(name) || atom::RESERVED_BUNDLE_NAMES.contains(&name.as_str()) {
            cx.err(
                "E_VALUE_RANGE",
                &p,
                "bundle names must match [a-z][a-z0-9_-]* and not be a reserved prefix",
            );
        }
        let Some(b) = b.as_table() else { continue };
        if b.get("atoms").is_none() {
            cx.err("E_MISSING_KEY", &format!("{p}.atoms"), "required");
            continue;
        }
        let mut sub = Cx {
            file: cx.file,
            dir: cx.dir.clone(),
            diags: Vec::new(),
        };
        check_grant_list(b, "atoms", false, &mut sub);
        for mut d in sub.diags {
            d.toml_path = format!("{p}.{}", d.toml_path);
            cx.diags.push(d);
        }
        for (i, a) in str_list(b, "atoms") {
            if let Some((_, operand)) = a.split_once(':')
                && let Ok(Some((atom::Placeholder::Install(tool), _))) =
                    atom::leading_placeholder(operand)
                && !install.iter().any(|(k, _)| *k == tool)
            {
                cx.err(
                    "E_UNKNOWN_PLACEHOLDER",
                    &idx(&format!("{p}.atoms"), i),
                    format!("'${{install:{tool}}}'"),
                );
            }
        }
    }
}

// ---- catalog ---------------------------------------------------------------------------------

fn check_catalog_semantics(t: &Table, cx: &mut Cx<'_>) {
    let Some(Value::Array(agents)) = t.get("agents") else {
        cx.err("E_MISSING_KEY", "agents", "required");
        return;
    };
    let mut seen: Vec<String> = Vec::new();
    for (i, a) in agents.iter().enumerate() {
        let Some(a) = a.as_table() else { continue };
        let p = idx("agents", i);
        match a.get("name").and_then(Value::as_str) {
            None => cx.err("E_MISSING_KEY", &format!("{p}.name"), "required"),
            Some(n) if !is_ident(n) => cx.err(
                "E_VALUE_RANGE",
                &format!("{p}.name"),
                "must match [a-z][a-z0-9_-]*",
            ),
            Some(n) if seen.iter().any(|x| x == n) => {
                cx.err("E_DUP_NAME", &format!("{p}.name"), format!("'{n}'"));
            }
            Some(n) => seen.push(n.to_owned()),
        }
        for k in ["model_profile", "agent_profile"] {
            let kp = format!("{p}.{k}");
            match a.get(k).and_then(Value::as_str) {
                None => cx.err("E_MISSING_KEY", &kp, "required"),
                Some(rel) => {
                    let full = cx.dir.join(rel);
                    if !full.is_file() {
                        cx.err("E_FILE_NOT_FOUND", &kp, format!("'{rel}'"));
                    }
                }
            }
        }
    }
    if !seen.iter().any(|n| n == "default") {
        cx.err("E_CATALOG_REF", "agents", "no entry named 'default'");
    }
}
