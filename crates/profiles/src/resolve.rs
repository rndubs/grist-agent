//! The resolution algorithm (`profile-schema.md` §7): `resolve(inputs)` runs steps 1–12 in
//! order, stops at the first failing step, and reports every error of that step.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use kernel::{
    ActiveProfiles, Capability, FsMode, Hash, MiddlewareSource, ModelParams, ProfileKind,
    ProfileLoadPayload, PromptBlock, RECORDER_PRIORITY, SandboxLimits, SandboxPolicy, SpillConfig,
    TOOL_CALL_PARSER_NAME, TOOL_CALL_PARSER_PRIORITY, ThinkingConfig, ToolKind,
};
use toml::{Table, Value};

use crate::atom::{AtomError, Expander, check_path_grammar, symbolize};
use crate::bundles::Bundles;
use crate::catalog::{Catalog, CatalogEntry};
use crate::diagnostic::{Diagnostic, idx};
use crate::merge::{Layered, toml_to_json};
use crate::prompt::{self, PromptSources};
use crate::resolved::{
    AgentSection, Compaction, EvalSet, McpServer, Memory, MiddlewareEntry, ModelSection, Notebook,
    Quirks, ResolvedProfile, SandboxSection, Subagent, Thinking,
};
use crate::schema::{
    FileKind, MwEntry, RUNTIME_OVERRIDE_KEYS, SCHEMA_VERSION, ValidatedFile, validate_file,
};

/// What the validator must know about this build (§7.1). Supplied by the kernel binary that
/// assembles tools and middleware; `profiles` discovers nothing itself.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    /// Tool name → declaration (concrete capabilities, constructed with the session workdir).
    pub tools: BTreeMap<String, ToolDecl>,
    /// Compiled middleware names.
    pub middleware: BTreeSet<String>,
    /// Tool-call syntaxes with a compiled parser.
    pub parsers: BTreeSet<String>,
    /// Memory module names (`"none"` is the no-op).
    pub memory_modules: BTreeSet<String>,
    /// Sandbox backend names.
    pub sandbox_backends: BTreeSet<String>,
    /// `dev-sandbox-none` feature on (D14).
    pub dev_build: bool,
}

/// What a registered tool declares.
#[derive(Clone, Debug)]
pub struct ToolDecl {
    /// Isolation kind (D5).
    pub kind: ToolKind,
    /// Concrete capabilities the tool needs.
    pub capabilities: Vec<Capability>,
}

/// Inputs of `resolve` (§7.1).
#[derive(Clone, Debug)]
pub struct ResolveInputs<'a> {
    /// Holds `catalog.toml` and `bundles.toml`.
    pub profiles_dir: &'a Path,
    /// Absolute session working directory (`${workdir}`).
    pub workdir: &'a Path,
    /// The kernel user's home (`${home}`), supplied by the launcher.
    pub home: &'a Path,
    /// Catalog entry name.
    pub agent: &'a str,
    /// Layer 4: a flat map of whitelisted dotted keys (§4.4).
    pub runtime_overrides: &'a Table,
    /// This build's registry.
    pub registry: &'a Registry,
    /// Resuming: inject the notebook block (§8).
    pub resume: bool,
}

/// One entry of the resolved chain for the launcher, which instantiates the compiled
/// middleware by `name`.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedMiddleware {
    /// Middleware name.
    pub name: String,
    /// Priority (kernel `MiddlewareEntry.priority`).
    pub priority: i32,
    /// Contributing layer.
    pub source: MiddlewareSource,
    /// Opaque config (`{}` when absent).
    pub config: serde_json::Value,
    /// `Hash::of_canonical_json(config)` when the config is non-empty.
    pub config_hash: Option<Hash>,
}

/// A `[[mcp_servers]]` entry after placeholder expansion.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedMcpServer {
    /// The symbolic entry.
    pub server: McpServer,
    /// argv with placeholders expanded.
    pub command: Vec<String>,
    /// Concrete capabilities the server process runs under.
    pub capabilities: Vec<Capability>,
}

/// A `[[subagents]]` entry after placeholder expansion.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedSubagent {
    /// The symbolic entry.
    pub subagent: Subagent,
    /// Concrete ceiling.
    pub ceiling: Vec<Capability>,
    /// Expanded definition path.
    pub definition: Option<PathBuf>,
}

/// `[agent.eval_set]` after expansion (URLs are kept verbatim).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedEvalSet {
    /// Dev set: expanded path or URL.
    pub dev: String,
    /// Hidden set: expanded path or URL.
    pub hidden: String,
}

/// Everything the launcher/kernel consumes, already in kernel types where one exists
/// (`kernel-interface.md` §9).
#[derive(Clone, Debug)]
pub struct KernelInputs {
    /// `model.id`.
    pub model_id: String,
    /// `model.endpoint` (resolved to a URL by the launcher via `GRIST_ENDPOINT_<NAME>_URL`).
    pub endpoint: String,
    /// `model.tool_format` (`"native"` or `"parsed:<syntax>"`).
    pub tool_format: String,
    /// `model.context_length`.
    pub context_length: u64,
    /// `[model.quirks]` for the `providers` crate.
    pub quirks: Quirks,
    /// Sampling parameters (`max_output_tokens` → `max_tokens`).
    pub model_params: ModelParams,
    /// Blocks 1–3 (+5 on resume) in D7 order.
    pub system_prompt: Vec<PromptBlock>,
    /// The resolved chain, sorted, kernel entries included.
    pub middleware: Vec<ResolvedMiddleware>,
    /// `grants_resolved`: expanded, derived, deduped, sorted, concrete.
    pub grants: Vec<Capability>,
    /// `[tools].allow`, sorted.
    pub tools: Vec<String>,
    /// `[tool_descriptions]` resolved to text.
    pub tool_descriptions: BTreeMap<String, String>,
    /// `sandbox.backend`.
    pub sandbox_backend: String,
    /// `[sandbox]` limits (`env_allow` → `env_allowlist`).
    pub sandbox_limits: SandboxLimits,
    /// Spill config with `cap_bytes = agent.spill_cap_bytes`.
    pub spill: SpillConfig,
    /// `agent.context_budget_tokens` (D16; consumed by P2.9).
    pub context_budget_tokens: u64,
    /// `[compaction]`.
    pub compaction: Compaction,
    /// Expanded `notebook.path`.
    pub notebook_path: Option<PathBuf>,
    /// `[notebook]` (symbolic path plus the other keys).
    pub notebook: Notebook,
    /// The session envelope, `derive_policy_with(grants − secrets, grants, limits)`.
    pub envelope_policy: SandboxPolicy,
    /// `envelope_policy.hash()`.
    pub sandbox_policy_hash: Hash,
    /// Expanded `agent.agents_md`.
    pub agents_md_path: PathBuf,
    /// Expanded `skills.paths`.
    pub skills_paths: Vec<PathBuf>,
    /// `[[mcp_servers]]` expanded.
    pub mcp_servers: Vec<ResolvedMcpServer>,
    /// `[[subagents]]` expanded.
    pub subagents: Vec<ResolvedSubagent>,
    /// `[memory]`.
    pub memory: Memory,
    /// `[agent.eval_set]` expanded.
    pub eval_set: Option<ResolvedEvalSet>,
    /// The catalog entry name.
    pub catalog_entry: String,
}

/// Fine-tune drift (§2.6): the profile hash the weights were trained against differs from
/// `resolved_profile_hash`. Emitted as `warning{class: "profile_drift", detail: {expected, actual}}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileDrift {
    /// `fine_tune.trained_against_profile_hash`.
    pub expected: Hash,
    /// `resolved_profile_hash`.
    pub actual: Hash,
    /// `fine_tune.warn_on_drift` (when `false`, no `W_PROFILE_DRIFT` warning is emitted).
    pub warn: bool,
}

/// The output of `resolve`.
#[derive(Clone, Debug)]
pub struct Resolved {
    /// The symbolic resolved profile (§7.6).
    pub resolved_profile: ResolvedProfile,
    /// `Hash::of_canonical_json(&resolved_profile)`.
    pub resolved_profile_hash: Hash,
    /// Hashes for `State.profiles`.
    pub active_profiles: ActiveProfiles,
    /// `Hash::of_bytes` of `catalog.toml`.
    pub catalog_hash: Hash,
    /// `profile_load` payloads in §7.2 step 11 order: bundles, model, agent, project (if present).
    pub profile_loads: Vec<ProfileLoadPayload>,
    /// `W_*` diagnostics (logged as `warning{class: "profile_warning"}`).
    pub warnings: Vec<Diagnostic>,
    /// Fine-tune drift, if `[fine_tune]` is present and the hash differs.
    pub drift: Option<ProfileDrift>,
    /// What the kernel consumes.
    pub kernel_inputs: KernelInputs,
}

// ---- internal --------------------------------------------------------------------------------

enum ChildMemo {
    InProgress,
    Done(Result<Vec<Capability>, Vec<Diagnostic>>),
}

struct Ctx<'a> {
    inputs: &'a ResolveInputs<'a>,
    catalog: Catalog,
    bundles: Bundles,
    memo: RefCell<BTreeMap<String, ChildMemo>>,
}

/// Which layers to include.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layers {
    /// 0–2 (used for the layer-3 comparison and for sub-agent ceilings).
    Base,
    /// 0–4.
    Full,
}

/// One grant with provenance for messages.
#[derive(Clone, Debug)]
struct Grant {
    /// TOML path of the item that produced it.
    path: String,
    /// Symbolic string form.
    sym: String,
    /// Concrete atom (set in step 7).
    cap: Option<Capability>,
}

/// State carried through steps 2–8 for one catalog entry.
struct Stage {
    model_file: ValidatedFile,
    agent_file: ValidatedFile,
    project_file: Option<ValidatedFile>,
    layered: Layered,
    profile: ResolvedProfile,
    hash: Hash,
    grants: Vec<Grant>,
    concrete_grants: Vec<Capability>,
    chain: Vec<MwEntry>,
    limits: SandboxLimits,
    agents_md: PathBuf,
    skills: Vec<PathBuf>,
    notebook: PathBuf,
    mcp: Vec<ResolvedMcpServer>,
    subagents: Vec<ResolvedSubagent>,
    eval_set: Option<ResolvedEvalSet>,
    fine_tune: Option<(Hash, bool)>,
    warnings: Vec<Diagnostic>,
    /// Step 8 (a–e) errors, returned rather than raised so the caller can add the layer-3 rules.
    narrowing_errors: Vec<Diagnostic>,
}

fn errs_of(diags: Vec<Diagnostic>) -> Result<(), Vec<Diagnostic>> {
    if diags.iter().any(Diagnostic::is_error) {
        Err(diags)
    } else {
        Ok(())
    }
}

/// Run the resolution algorithm (§7.2).
pub fn resolve(inputs: &ResolveInputs<'_>) -> Result<Resolved, Vec<Diagnostic>> {
    // Step 1–2: catalog and bundles (load, hash, validate).
    let catalog = Catalog::load(&inputs.profiles_dir.join("catalog.toml"))?;
    let bundles = Bundles::load(&inputs.profiles_dir.join("bundles.toml"))?;
    let ctx = Ctx {
        inputs,
        catalog,
        bundles,
        memo: RefCell::new(BTreeMap::new()),
    };
    let entry = ctx.catalog.get(inputs.agent).cloned().ok_or_else(|| {
        vec![Diagnostic::new(
            "E_CATALOG_REF",
            Some(&ctx.catalog.path),
            "agents",
            format!("no entry named '{}'", inputs.agent),
        )]
    })?;

    let mut full = resolve_entry(&ctx, &entry, Layers::Full)?;

    // Step 8f: layer-3 narrowing rules.
    let mut step8 = std::mem::take(&mut full.narrowing_errors);
    if full.project_file.is_some() {
        let base = resolve_entry(&ctx, &entry, Layers::Base)?;
        step8.extend(check_project_narrowing(&full, &base));
    }
    errs_of(step8)?;

    // Step 9: the envelope.
    let grants_without_secrets: Vec<Capability> = full
        .concrete_grants
        .iter()
        .filter(|c| !matches!(c, Capability::Secret { .. }))
        .cloned()
        .collect();
    let envelope_policy =
        kernel::derive_policy_with(&grants_without_secrets, &full.concrete_grants, &full.limits)
            .map_err(|e| {
                vec![Diagnostic::new(
                    "E_SANDBOX_POLICY",
                    Some(&full.agent_file.path),
                    "sandbox",
                    e.to_string(),
                )]
            })?;
    let sandbox_policy_hash = envelope_policy.hash().map_err(|e| {
        vec![Diagnostic::new(
            "E_SANDBOX_POLICY",
            Some(&full.agent_file.path),
            "sandbox",
            e.to_string(),
        )]
    })?;

    // Step 10: system prompt.
    let mut warnings = std::mem::take(&mut full.warnings);
    let system_prompt = assemble_prompt(&full, inputs, &mut warnings)?;

    // Drift (§2.6).
    let drift = full.fine_tune.as_ref().and_then(|(expected, warn)| {
        (*expected != full.hash).then(|| ProfileDrift {
            expected: expected.clone(),
            actual: full.hash.clone(),
            warn: *warn,
        })
    });
    if let Some(d) = &drift
        && d.warn
    {
        warnings.push(Diagnostic::new(
            "W_PROFILE_DRIFT",
            Some(&full.model_file.path),
            "fine_tune.trained_against_profile_hash",
            format!("expected {} != actual {}", d.expected, d.actual),
        ));
    }

    // Step 11: events.
    let load = |kind: ProfileKind, name: &str, file: &ValidatedFile| ProfileLoadPayload {
        kind,
        name: name.to_owned(),
        path: Some(file.path.clone()),
        hash: file.hash.clone(),
        rejected: false,
        turn: 0,
    };
    let mut profile_loads = vec![
        ProfileLoadPayload {
            kind: ProfileKind::Bundles,
            name: "bundles".to_owned(),
            path: Some(ctx.bundles.path.clone()),
            hash: ctx.bundles.hash.clone(),
            rejected: false,
            turn: 0,
        },
        load(ProfileKind::Model, &full.profile.model.id, &full.model_file),
        load(
            ProfileKind::Agent,
            &full.profile.agent.name,
            &full.agent_file,
        ),
    ];
    if let Some(p) = &full.project_file {
        profile_loads.push(load(ProfileKind::Project, &full.profile.agent.name, p));
    }

    // Step 12: what `State` records.
    let active_profiles = ActiveProfiles {
        model_profile_hash: full.model_file.hash.clone(),
        agent_profile_hash: full.agent_file.hash.clone(),
        resolved_profile_hash: full.hash.clone(),
        project_profile_hash: full.project_file.as_ref().map(|p| p.hash.clone()),
        bundles_hash: Some(ctx.bundles.hash.clone()),
    };

    let p = &full.profile;
    let middleware = full
        .chain
        .iter()
        .map(|m| {
            let config = toml_to_json(&Value::Table(m.config.clone()));
            let config_hash = if m.config.is_empty() {
                None
            } else {
                Hash::of_canonical_json(&config).ok()
            };
            ResolvedMiddleware {
                name: m.name.clone(),
                priority: m.priority as i32,
                source: m.source,
                config,
                config_hash,
            }
        })
        .collect();
    let kernel_inputs = KernelInputs {
        model_id: p.model.id.clone(),
        endpoint: p.model.endpoint.clone(),
        tool_format: p.model.tool_format.clone(),
        context_length: p.model.context_length,
        quirks: p.model.quirks.clone(),
        model_params: ModelParams {
            temperature: p.model.temperature,
            top_p: None,
            max_tokens: Some(p.model.max_output_tokens.min(u64::from(u32::MAX)) as u32),
            stop: Vec::new(),
            thinking: Some(ThinkingConfig {
                enabled: p.model.thinking.enabled,
                budget_tokens: (p.model.thinking.budget_tokens > 0)
                    .then(|| p.model.thinking.budget_tokens.min(u64::from(u32::MAX)) as u32),
            }),
            extra: serde_json::Value::Null,
        },
        system_prompt,
        middleware,
        grants: full.concrete_grants.clone(),
        tools: p.tools.clone(),
        tool_descriptions: p.tool_descriptions.clone(),
        sandbox_backend: p.sandbox.backend.clone(),
        sandbox_limits: full.limits.clone(),
        spill: SpillConfig {
            cap_bytes: p.agent.spill_cap_bytes,
            ..SpillConfig::default()
        },
        context_budget_tokens: p.agent.context_budget_tokens,
        compaction: p.compaction.clone(),
        notebook_path: Some(full.notebook.clone()),
        notebook: p.notebook.clone(),
        envelope_policy,
        sandbox_policy_hash,
        agents_md_path: full.agents_md.clone(),
        skills_paths: full.skills.clone(),
        mcp_servers: full.mcp.clone(),
        subagents: full.subagents.clone(),
        memory: p.memory.clone(),
        eval_set: full.eval_set.clone(),
        catalog_entry: entry.name.clone(),
    };

    Ok(Resolved {
        resolved_profile_hash: full.hash.clone(),
        resolved_profile: full.profile,
        active_profiles,
        catalog_hash: ctx.catalog.hash.clone(),
        profile_loads,
        warnings,
        drift,
        kernel_inputs,
    })
}

/// Steps 2–8a–e for one catalog entry.
fn resolve_entry(
    ctx: &Ctx<'_>,
    entry: &CatalogEntry,
    layers: Layers,
) -> Result<Stage, Vec<Diagnostic>> {
    let inputs = ctx.inputs;

    // Step 1–2: load and validate the layer files.
    let read = |p: &Path, what: &str| -> Result<Vec<u8>, Vec<Diagnostic>> {
        std::fs::read(p).map_err(|e| {
            vec![Diagnostic::new(
                "E_FILE_NOT_FOUND",
                Some(&ctx.catalog.path),
                what.to_owned(),
                format!("'{}': {e}", p.display()),
            )]
        })
    };
    let entry_index = ctx
        .catalog
        .entries()
        .iter()
        .position(|e| e.name == entry.name)
        .unwrap_or(0);
    let model_bytes = read(
        &entry.model_profile,
        &format!("{}.model_profile", idx("agents", entry_index)),
    )?;
    let agent_bytes = read(
        &entry.agent_profile,
        &format!("{}.agent_profile", idx("agents", entry_index)),
    )?;
    let project_path = inputs.workdir.join(".grist").join("agent.toml");
    let project_bytes = if layers == Layers::Full && project_path.is_file() {
        Some(read(&project_path, "")?)
    } else {
        None
    };

    let mut diags = Vec::new();
    let model_file = validate_file(FileKind::Model, &entry.model_profile, &model_bytes)
        .map_err(|d| diags.extend(d));
    let agent_file = validate_file(FileKind::Agent, &entry.agent_profile, &agent_bytes)
        .map_err(|d| diags.extend(d));
    let project_file = project_bytes
        .as_ref()
        .map(|b| validate_file(FileKind::Project, &project_path, b).map_err(|d| diags.extend(d)));
    errs_of(diags)?;
    let (Ok(model_file), Ok(agent_file)) = (model_file, agent_file) else {
        unreachable!("errors were returned above");
    };
    let project_file = project_file.map(|r| r.expect("errors were returned above"));

    // Step 3: merge.
    let mut layered = Layered::from_defaults(crate::kernel_defaults());
    let mut diags = Vec::new();
    diags.extend(layered.apply(&model_file));
    diags.extend(layered.apply(&agent_file));
    if let Some(p) = &project_file {
        diags.extend(layered.apply(p));
    }
    let mut runtime_overrides = BTreeMap::new();
    if layers == Layers::Full {
        match apply_runtime_overrides(&mut layered, inputs.runtime_overrides) {
            Ok(ro) => runtime_overrides = ro,
            Err(d) => diags.extend(d),
        }
    }
    errs_of(diags)?;
    check_merged(&layered, &model_file, &agent_file, entry)?;

    // Step 4: bundles and derived atoms.
    let (grants, mcp_syms, sub_syms) = expand_grants(ctx, &layered)?;

    // Step 5: parser slot, registry checks, chain sort.
    let chain = finish_chain(ctx, &mut layered, &model_file, &agent_file)?;
    check_registry(ctx, &layered)?;

    // Step 6: the resolved struct and its hash.
    let profile = build_profile(
        &layered,
        entry,
        &grants,
        &mcp_syms,
        &sub_syms,
        &chain,
        runtime_overrides,
    );
    let hash = Hash::of_canonical_json(&profile).map_err(|e| {
        vec![Diagnostic::new(
            "E_HASH",
            Some(&agent_file.path),
            "",
            format!("resolved profile is not canonicalizable: {e}"),
        )]
    })?;

    // Step 7: placeholders and normalization.
    let expander = Expander {
        workdir: inputs.workdir,
        home: inputs.home,
        install: &ctx.bundles.install,
    };
    let mut stage = Stage {
        model_file,
        agent_file,
        project_file,
        layered,
        profile,
        hash,
        grants,
        concrete_grants: Vec::new(),
        chain,
        limits: SandboxLimits::default(),
        agents_md: PathBuf::new(),
        skills: Vec::new(),
        notebook: PathBuf::new(),
        mcp: Vec::new(),
        subagents: Vec::new(),
        eval_set: None,
        fine_tune: None,
        warnings: Vec::new(),
        narrowing_errors: Vec::new(),
    };
    expand_stage(&mut stage, &expander, &mcp_syms, &sub_syms)?;

    // Step 8a–e.
    let errors = narrowing_checks(ctx, &mut stage);
    stage.narrowing_errors = errors;
    Ok(stage)
}

// ---- step 3 helpers --------------------------------------------------------------------------

fn apply_runtime_overrides(
    layered: &mut Layered,
    overrides: &Table,
) -> Result<BTreeMap<String, serde_json::Value>, Vec<Diagnostic>> {
    let mut diags = Vec::new();
    let mut verbatim = BTreeMap::new();
    let mut nested = Table::new();
    for (k, v) in overrides {
        if !RUNTIME_OVERRIDE_KEYS.contains(&k.as_str()) {
            diags.push(Diagnostic::new(
                "E_OVERRIDE_FORBIDDEN_KEY",
                None,
                k.clone(),
                "not in the runtime override whitelist (profile-schema.md §4.4)",
            ));
            continue;
        }
        let ok = match k.as_str() {
            "model.temperature" => match v {
                Value::Float(f) => (0.0..=2.0).contains(f),
                Value::Integer(i) => (0..=2).contains(i),
                _ => false,
            },
            "model.max_output_tokens" | "agent.context_budget_tokens" => {
                matches!(v, Value::Integer(i) if *i > 0)
            }
            "model.thinking.enabled" => v.is_bool(),
            "model.thinking.budget_tokens" => matches!(v, Value::Integer(i) if *i >= 0),
            "notebook.path" => matches!(v, Value::String(s) if check_path_grammar(s).is_ok()),
            _ => false,
        };
        if !ok {
            diags.push(Diagnostic::new(
                "E_VALUE_RANGE",
                None,
                k.clone(),
                "runtime override has the wrong type or is out of range",
            ));
            continue;
        }
        verbatim.insert(k.clone(), toml_to_json(v));
        // Build the nested table.
        let mut cur = &mut nested;
        let segs: Vec<&str> = k.split('.').collect();
        for seg in &segs[..segs.len() - 1] {
            cur = cur
                .entry((*seg).to_owned())
                .or_insert_with(|| Value::Table(Table::new()))
                .as_table_mut()
                .expect("just inserted a table");
        }
        cur.insert(segs[segs.len() - 1].to_owned(), v.clone());
    }
    errs_of(diags)?;
    layered.apply_table(&nested, None);
    Ok(verbatim)
}

fn get<'t>(t: &'t Table, dotted: &str) -> Option<&'t Value> {
    let mut segs = dotted.split('.');
    let mut node = t.get(segs.next()?)?;
    for seg in segs {
        node = node.as_table()?.get(seg)?;
    }
    Some(node)
}

fn s_at(t: &Table, dotted: &str) -> Option<String> {
    get(t, dotted).and_then(Value::as_str).map(str::to_owned)
}

fn i_at(t: &Table, dotted: &str) -> Option<i64> {
    get(t, dotted).and_then(Value::as_integer)
}

fn u_at(t: &Table, dotted: &str, default: u64) -> u64 {
    i_at(t, dotted)
        .and_then(|i| u64::try_from(i).ok())
        .unwrap_or(default)
}

fn f_at(t: &Table, dotted: &str) -> Option<f64> {
    match get(t, dotted)? {
        Value::Float(f) => Some(*f),
        Value::Integer(i) => Some(*i as f64),
        _ => None,
    }
}

fn b_at(t: &Table, dotted: &str, default: bool) -> bool {
    get(t, dotted).and_then(Value::as_bool).unwrap_or(default)
}

fn list_at(t: &Table, dotted: &str) -> Vec<String> {
    get(t, dotted)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn tables_at<'t>(t: &'t Table, key: &str) -> Vec<&'t Table> {
    t.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_table).collect())
        .unwrap_or_default()
}

/// Required keys after merge (§7.5) and the cross-key rules that only hold after merge.
fn check_merged(
    layered: &Layered,
    model_file: &ValidatedFile,
    agent_file: &ValidatedFile,
    entry: &CatalogEntry,
) -> Result<(), Vec<Diagnostic>> {
    let t = &layered.table;
    let mut diags = Vec::new();
    for key in ["model.id", "model.endpoint", "model.context_length"] {
        if get(t, key).is_none() {
            diags.push(Diagnostic::new(
                "E_MISSING_KEY",
                Some(&model_file.path),
                key,
                "required",
            ));
        }
    }
    match s_at(t, "agent.name") {
        None => diags.push(Diagnostic::new(
            "E_MISSING_KEY",
            Some(&agent_file.path),
            "agent.name",
            "required",
        )),
        Some(n) if n != entry.name => diags.push(Diagnostic::new(
            "E_CATALOG_REF",
            Some(&agent_file.path),
            "agent.name",
            format!("'{n}' != catalog entry '{}'", entry.name),
        )),
        Some(_) => {}
    }
    // Cross-key rules over the merged values.
    let mut cx = crate::schema::Cx::for_merged(
        layered
            .origin_of("compaction.target")
            .unwrap_or(&model_file.path),
    );
    crate::schema::check_compaction(t, &mut cx);
    diags.extend(cx.into_diags());
    if b_at(t, "model.thinking.enabled", false)
        && i_at(t, "model.thinking.budget_tokens").is_some_and(|b| b < 0)
    {
        diags.push(Diagnostic::new(
            "E_VALUE_RANGE",
            layered.origin_of("model.thinking.budget_tokens"),
            "model.thinking.budget_tokens",
            "must be >= 0",
        ));
    }
    if !b_at(t, "model.thinking.enabled", false)
        && i_at(t, "model.thinking.budget_tokens").is_some_and(|b| b > 0)
    {
        diags.push(Diagnostic::new(
            "E_VALUE_RANGE",
            layered.origin_of("model.thinking.budget_tokens"),
            "model.thinking.budget_tokens",
            "must be 0 when enabled = false",
        ));
    }
    errs_of(diags)
}

// ---- step 4 ----------------------------------------------------------------------------------

type SymList = Vec<(String, Vec<Grant>)>;

/// Expand bundles in grants, MCP capabilities and sub-agent ceilings; add derived atoms.
/// Returns the grants and, per MCP server / sub-agent, its expanded list.
fn expand_grants(
    ctx: &Ctx<'_>,
    layered: &Layered,
) -> Result<(Vec<Grant>, SymList, SymList), Vec<Diagnostic>> {
    let t = &layered.table;
    let mut diags = Vec::new();
    let expand_list = |dotted: &str, items: &[String], diags: &mut Vec<Diagnostic>| -> Vec<Grant> {
        let refs: Vec<&str> = items.iter().map(String::as_str).collect();
        match ctx.bundles.expand_indexed(&refs) {
            Ok(list) => list
                .into_iter()
                .map(|(i, sym)| Grant {
                    path: idx(dotted, i),
                    sym,
                    cap: None,
                })
                .collect(),
            Err(errs) => {
                for (i, name) in errs {
                    diags.push(Diagnostic::new(
                        "E_UNKNOWN_BUNDLE",
                        layered.origin_of(dotted),
                        idx(dotted, i),
                        format!("'{name}'"),
                    ));
                }
                Vec::new()
            }
        }
    };
    let mut grants = expand_list(
        "capabilities.grants",
        &list_at(t, "capabilities.grants"),
        &mut diags,
    );
    for (i, tool) in list_at(t, "tools.allow").iter().enumerate() {
        grants.push(Grant {
            path: idx("tools.allow", i),
            sym: format!("tool:{tool}"),
            cap: None,
        });
    }
    let mut mcp = Vec::new();
    for (i, s) in tables_at(t, "mcp_servers").iter().enumerate() {
        let dotted = format!("{}.capabilities", idx("mcp_servers", i));
        let list = expand_list(&dotted, &list_at(s, "capabilities"), &mut diags);
        mcp.push((s_at(s, "name").unwrap_or_default(), list));
    }
    let mut subs = Vec::new();
    for (i, s) in tables_at(t, "subagents").iter().enumerate() {
        let p = idx("subagents", i);
        let catalog_name = s_at(s, "catalog").unwrap_or_default();
        if ctx.catalog.get(&catalog_name).is_none() {
            diags.push(Diagnostic::new(
                "E_CATALOG_REF",
                layered.origin_of("subagents"),
                format!("{p}.catalog"),
                format!("'{catalog_name}'"),
            ));
        }
        grants.push(Grant {
            path: format!("{p}.catalog"),
            sym: format!("spawn:{catalog_name}"),
            cap: None,
        });
        let list = expand_list(
            &format!("{p}.capabilities"),
            &list_at(s, "capabilities"),
            &mut diags,
        );
        subs.push((catalog_name, list));
    }
    errs_of(diags)?;
    Ok((grants, mcp, subs))
}

// ---- step 5 ----------------------------------------------------------------------------------

fn finish_chain(
    ctx: &Ctx<'_>,
    layered: &mut Layered,
    model_file: &ValidatedFile,
    agent_file: &ValidatedFile,
) -> Result<Vec<MwEntry>, Vec<Diagnostic>> {
    let mut diags = Vec::new();
    let tool_format =
        s_at(&layered.table, "model.tool_format").unwrap_or_else(|| "native".to_owned());
    if let Some(syntax) = tool_format.strip_prefix("parsed:") {
        if !ctx.inputs.registry.parsers.contains(syntax) {
            diags.push(Diagnostic::new(
                "E_PARSER_UNAVAILABLE",
                Some(&model_file.path),
                "model.tool_format",
                format!("no parser for '{syntax}'"),
            ));
        }
        if !layered
            .middleware
            .iter()
            .any(|m| m.name == TOOL_CALL_PARSER_NAME)
        {
            let mut config = Table::new();
            config.insert("syntax".to_owned(), Value::String(syntax.to_owned()));
            layered.middleware.push(MwEntry {
                name: TOOL_CALL_PARSER_NAME.to_owned(),
                priority: i64::from(TOOL_CALL_PARSER_PRIORITY),
                config,
                source: MiddlewareSource::Model,
                file: Some(model_file.path.clone()),
                index: layered.middleware.len(),
            });
        }
    }
    for m in &layered.middleware {
        if m.name != TOOL_CALL_PARSER_NAME && !ctx.inputs.registry.middleware.contains(&m.name) {
            diags.push(Diagnostic::new(
                "E_MIDDLEWARE_UNKNOWN",
                m.file.as_deref().or(Some(&agent_file.path)),
                format!("{}.name", idx("middleware", m.index)),
                format!("'{}'", m.name),
            ));
        }
    }
    errs_of(diags)?;
    let mut chain = layered.middleware.clone();
    chain.push(MwEntry {
        name: "recorder".to_owned(),
        priority: i64::from(RECORDER_PRIORITY),
        config: Table::new(),
        source: MiddlewareSource::Kernel,
        file: None,
        index: 0,
    });
    chain.sort_by_key(|m| m.priority); // stable
    Ok(chain)
}

fn check_registry(ctx: &Ctx<'_>, layered: &Layered) -> Result<(), Vec<Diagnostic>> {
    let reg = ctx.inputs.registry;
    let t = &layered.table;
    let mut diags = Vec::new();
    let module = s_at(t, "memory.module").unwrap_or_else(|| "none".to_owned());
    if !reg.memory_modules.contains(&module) {
        diags.push(Diagnostic::new(
            "E_MEMORY_UNKNOWN",
            layered.origin_of("memory.module"),
            "memory.module",
            format!("'{module}'"),
        ));
    }
    let backend = s_at(t, "sandbox.backend").unwrap_or_else(|| "bwrap".to_owned());
    if backend == "none" && !reg.dev_build {
        diags.push(Diagnostic::new(
            "E_SANDBOX_NONE_FORBIDDEN",
            layered.origin_of("sandbox.backend"),
            "sandbox.backend",
            "\"none\" is only accepted in a dev build (D14)",
        ));
    } else if !reg.sandbox_backends.contains(&backend) {
        diags.push(Diagnostic::new(
            "E_SANDBOX_BACKEND_UNKNOWN",
            layered.origin_of("sandbox.backend"),
            "sandbox.backend",
            format!("'{backend}'"),
        ));
    }
    for (i, tool) in list_at(t, "tools.allow").iter().enumerate() {
        if !reg.tools.contains_key(tool) {
            diags.push(Diagnostic::new(
                "E_TOOL_UNKNOWN",
                layered.origin_of("tools.allow"),
                idx("tools.allow", i),
                format!("'{tool}'"),
            ));
        }
    }
    if let Some(Value::Table(d)) = t.get("tool_descriptions") {
        for k in d.keys() {
            if !reg.tools.contains_key(k) {
                let p = format!("tool_descriptions.{k}");
                diags.push(Diagnostic::new(
                    "E_TOOL_UNKNOWN",
                    layered.origin_of(&p),
                    p.clone(),
                    format!("'{k}'"),
                ));
            }
        }
    }
    errs_of(diags)
}

// ---- step 6 ----------------------------------------------------------------------------------

fn sorted_syms(list: &[Grant]) -> Vec<String> {
    let mut v: Vec<String> = list.iter().map(|g| g.sym.clone()).collect();
    v.sort();
    v.dedup();
    v
}

fn build_profile(
    layered: &Layered,
    entry: &CatalogEntry,
    grants: &[Grant],
    mcp: &SymList,
    subs: &SymList,
    chain: &[MwEntry],
    runtime_overrides: BTreeMap<String, serde_json::Value>,
) -> ResolvedProfile {
    let t = &layered.table;
    let mut tools = list_at(t, "tools.allow");
    tools.sort();
    tools.dedup();
    let mut env_allow = list_at(t, "sandbox.env_allow");
    env_allow.sort();
    env_allow.dedup();
    let tool_descriptions = t
        .get("tool_descriptions")
        .and_then(Value::as_table)
        .map(|d| {
            d.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    ResolvedProfile {
        schema_version: SCHEMA_VERSION,
        catalog_entry: entry.name.clone(),
        model: ModelSection {
            id: s_at(t, "model.id").unwrap_or_default(),
            endpoint: s_at(t, "model.endpoint").unwrap_or_default(),
            context_length: u_at(t, "model.context_length", 0),
            tool_format: s_at(t, "model.tool_format").unwrap_or_else(|| "native".to_owned()),
            temperature: f_at(t, "model.temperature"),
            max_output_tokens: u_at(t, "model.max_output_tokens", 4096),
            thinking: Thinking {
                enabled: b_at(t, "model.thinking.enabled", false),
                budget_tokens: u_at(t, "model.thinking.budget_tokens", 0),
            },
            quirks: Quirks {
                reasoning_field: s_at(t, "model.quirks.reasoning_field")
                    .unwrap_or_else(|| "none".to_owned()),
                supports_structured_output: b_at(
                    t,
                    "model.quirks.supports_structured_output",
                    false,
                ),
                supports_stream_usage: b_at(t, "model.quirks.supports_stream_usage", false),
                strict_tool_schema: b_at(t, "model.quirks.strict_tool_schema", false),
                auth: s_at(t, "model.quirks.auth").unwrap_or_else(|| "none".to_owned()),
            },
        },
        prompt: s_at(t, "prompt"),
        tool_descriptions,
        compaction: Compaction {
            trigger_at: f_at(t, "compaction.trigger_at").unwrap_or(0.85),
            target: f_at(t, "compaction.target").unwrap_or(0.50),
        },
        agent: AgentSection {
            name: s_at(t, "agent.name").unwrap_or_default(),
            description: s_at(t, "agent.description"),
            role_prompt: s_at(t, "agent.role_prompt"),
            agents_md: s_at(t, "agent.agents_md")
                .unwrap_or_else(|| "${workdir}/AGENTS.md".to_owned()),
            context_budget_tokens: u_at(t, "agent.context_budget_tokens", 40000),
            spill_cap_bytes: u_at(t, "agent.spill_cap_bytes", 16384),
            eval_set: get(t, "agent.eval_set").map(|_| EvalSet {
                dev: s_at(t, "agent.eval_set.dev").unwrap_or_default(),
                hidden: s_at(t, "agent.eval_set.hidden").unwrap_or_default(),
            }),
        },
        grants: sorted_syms(grants),
        tools,
        mcp_servers: tables_at(t, "mcp_servers")
            .iter()
            .zip(mcp)
            .map(|(s, (_, caps))| McpServer {
                name: s_at(s, "name").unwrap_or_default(),
                transport: s_at(s, "transport").unwrap_or_default(),
                command: list_at(s, "command"),
                url: s_at(s, "url"),
                capabilities: sorted_syms(caps),
                tools: s.get("tools").map(|_| list_at(s, "tools")),
                lazy: b_at(s, "lazy", true),
                env: s
                    .get("env")
                    .and_then(Value::as_table)
                    .map(|e| {
                        e.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect(),
        skills_paths: list_at(t, "skills.paths"),
        subagents: tables_at(t, "subagents")
            .iter()
            .zip(subs)
            .map(|(s, (_, caps))| Subagent {
                name: s_at(s, "name").unwrap_or_default(),
                catalog: s_at(s, "catalog").unwrap_or_default(),
                capabilities: sorted_syms(caps),
                definition: s_at(s, "definition"),
                description: s_at(s, "description"),
            })
            .collect(),
        middleware: chain
            .iter()
            .map(|m| MiddlewareEntry {
                name: m.name.clone(),
                priority: m.priority as i32,
                source: m.source,
                config: toml_to_json(&Value::Table(m.config.clone())),
            })
            .collect(),
        memory: Memory {
            module: s_at(t, "memory.module").unwrap_or_else(|| "none".to_owned()),
            config: get(t, "memory.config")
                .map(toml_to_json)
                .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())),
        },
        sandbox: SandboxSection {
            backend: s_at(t, "sandbox.backend").unwrap_or_else(|| "bwrap".to_owned()),
            timeout_s: u_at(t, "sandbox.timeout_s", 600),
            scratch_tmpfs_mb: u_at(t, "sandbox.scratch_tmpfs_mb", 256),
            env_allow,
            network: b_at(t, "sandbox.network", false),
        },
        notebook: Notebook {
            path: s_at(t, "notebook.path")
                .unwrap_or_else(|| "${workdir}/.grist/notebook.md".to_owned()),
            inject_on_resume: b_at(t, "notebook.inject_on_resume", true),
            max_tokens: u_at(t, "notebook.max_tokens", 4000),
        },
        runtime_overrides,
    }
}

// ---- step 7 ----------------------------------------------------------------------------------

fn atom_diag(layered: &Layered, path: &str, e: &AtomError) -> Diagnostic {
    Diagnostic::new(e.code(), layered.origin_of(path), path, e.message())
}

fn expand_atoms(
    layered: &Layered,
    ex: &Expander<'_>,
    list: &mut [Grant],
    diags: &mut Vec<Diagnostic>,
) -> Vec<Capability> {
    let mut out = Vec::new();
    for g in list.iter_mut() {
        match ex.expand_atom(&g.sym) {
            Ok(c) => {
                g.cap = Some(c.clone());
                out.push(c);
            }
            Err(e) => diags.push(atom_diag(layered, &g.path, &e)),
        }
    }
    out.sort();
    out.dedup();
    out
}

fn expand_stage(
    stage: &mut Stage,
    ex: &Expander<'_>,
    mcp: &SymList,
    subs: &SymList,
) -> Result<(), Vec<Diagnostic>> {
    let mut diags = Vec::new();
    let layered = &stage.layered;
    let mut grants = std::mem::take(&mut stage.grants);
    let concrete = expand_atoms(layered, ex, &mut grants, &mut diags);
    stage.grants = grants;
    stage.concrete_grants = concrete;

    let p = &stage.profile;
    let path_of = |sym: &str, path: &str, diags: &mut Vec<Diagnostic>| -> Option<PathBuf> {
        match ex.expand_path(sym) {
            Ok(p) => Some(p),
            Err(e) => {
                diags.push(atom_diag(layered, path, &e));
                None
            }
        }
    };
    stage.agents_md =
        path_of(&p.agent.agents_md, "agent.agents_md", &mut diags).unwrap_or_default();
    stage.notebook = path_of(&p.notebook.path, "notebook.path", &mut diags).unwrap_or_default();
    stage.skills = p
        .skills_paths
        .iter()
        .enumerate()
        .filter_map(|(i, s)| path_of(s, &idx("skills.paths", i), &mut diags))
        .collect();
    if let Some(es) = &p.agent.eval_set {
        let one = |v: &str, key: &str, diags: &mut Vec<Diagnostic>| -> String {
            if v.contains("://") {
                v.to_owned()
            } else {
                path_of(v, key, diags)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }
        };
        stage.eval_set = Some(ResolvedEvalSet {
            dev: one(&es.dev, "agent.eval_set.dev", &mut diags),
            hidden: one(&es.hidden, "agent.eval_set.hidden", &mut diags),
        });
    }
    let mut mcp_out = Vec::new();
    for (i, (server, (_, caps))) in p.mcp_servers.iter().zip(mcp).enumerate() {
        let mut caps = caps.clone();
        let concrete = expand_atoms(layered, ex, &mut caps, &mut diags);
        let mut command = Vec::new();
        for (j, arg) in server.command.iter().enumerate() {
            match ex.expand_raw(arg) {
                Ok(a) => command.push(a),
                Err(e) => diags.push(atom_diag(
                    layered,
                    &idx(&format!("{}.command", idx("mcp_servers", i)), j),
                    &e,
                )),
            }
        }
        mcp_out.push(ResolvedMcpServer {
            server: server.clone(),
            command,
            capabilities: concrete,
        });
    }
    let mut sub_out = Vec::new();
    for (i, (sub, (_, caps))) in p.subagents.iter().zip(subs).enumerate() {
        let mut caps = caps.clone();
        let ceiling = expand_atoms(layered, ex, &mut caps, &mut diags);
        let definition = sub.definition.as_deref().and_then(|d| {
            path_of(
                d,
                &format!("{}.definition", idx("subagents", i)),
                &mut diags,
            )
        });
        sub_out.push(ResolvedSubagent {
            subagent: sub.clone(),
            ceiling,
            definition,
        });
    }
    stage.mcp = mcp_out;
    stage.subagents = sub_out;
    stage.limits = SandboxLimits {
        timeout: Duration::from_secs(p.sandbox.timeout_s),
        scratch_tmpfs_mb: p.sandbox.scratch_tmpfs_mb,
        env_allowlist: p.sandbox.env_allow.iter().cloned().collect(),
        network: p.sandbox.network,
    };
    let t = &layered.table;
    stage.fine_tune = s_at(t, "fine_tune.trained_against_profile_hash")
        .and_then(|h| Hash::parse(&h).ok())
        .map(|h| (h, b_at(t, "fine_tune.warn_on_drift", true)));
    if !p.sandbox.network
        && stage
            .concrete_grants
            .iter()
            .any(|c| matches!(c, Capability::Net { .. }))
    {
        stage.warnings.push(Diagnostic::new(
            "W_NET_MASKED",
            layered.origin_of("sandbox.network"),
            "sandbox.network",
            "net: atoms are granted but sandbox.network = false masks them",
        ));
    }
    errs_of(diags)
}

// ---- step 8 ----------------------------------------------------------------------------------

fn narrowing_checks(ctx: &Ctx<'_>, stage: &mut Stage) -> Vec<Diagnostic> {
    let inputs = ctx.inputs;
    let layered = &stage.layered;
    let grants = &stage.concrete_grants;
    let sym = |c: &Capability| symbolize(c, inputs.workdir, inputs.home);
    let mut diags = Vec::new();

    // (a) tools.
    for (i, tool) in stage.profile.tools.iter().enumerate() {
        // Index in the file's list, not the sorted one.
        let file_index = list_at(&layered.table, "tools.allow")
            .iter()
            .position(|t| t == tool)
            .unwrap_or(i);
        if let Some(decl) = inputs.registry.tools.get(tool) {
            for cap in &decl.capabilities {
                if !cap.covered_by(grants) {
                    diags.push(Diagnostic::new(
                        "E_CAP_EXCEEDS_GRANTS",
                        layered.origin_of("tools.allow"),
                        idx("tools.allow", file_index),
                        format!("tool '{tool}' requires '{}'", sym(cap)),
                    ));
                }
            }
        }
    }
    // (b) MCP servers.
    for (i, server) in stage.mcp.iter().enumerate() {
        let p = idx("mcp_servers", i);
        // Report by original index: re-expand the symbolic list to pair indices with atoms.
        let file_caps: Vec<String> = tables_at(&layered.table, "mcp_servers")
            .get(i)
            .map(|s| list_at(s, "capabilities"))
            .unwrap_or_default();
        let refs: Vec<&str> = file_caps.iter().map(String::as_str).collect();
        if let Ok(indexed) = ctx.bundles.expand_indexed(&refs) {
            for (k, symbolic) in indexed {
                if let Ok(cap) = expander(ctx).expand_atom(&symbolic)
                    && !cap.covered_by(grants)
                {
                    diags.push(Diagnostic::new(
                        "E_CAP_EXCEEDS_GRANTS",
                        layered.origin_of("mcp_servers"),
                        idx(&format!("{p}.capabilities"), k),
                        format!("'{symbolic}'"),
                    ));
                }
            }
        }
        if server.server.transport == "http"
            && let Some(url) = &server.server.url
        {
            let host = url_host(url);
            let host_cap = Capability::Net {
                allow: kernel::NetAllow::Hosts(BTreeSet::from([host.clone()])),
            };
            if host.is_empty() || !host_cap.covered_by(&server.capabilities) {
                diags.push(Diagnostic::new(
                    "E_MCP_URL_HOST",
                    layered.origin_of("mcp_servers"),
                    format!("{p}.url"),
                    format!("'{host}' not covered"),
                ));
            }
        }
    }
    // (c) sub-agents.
    for (i, sub) in stage.subagents.iter().enumerate() {
        let p = idx("subagents", i);
        let file_caps: Vec<String> = tables_at(&layered.table, "subagents")
            .get(i)
            .map(|s| list_at(s, "capabilities"))
            .unwrap_or_default();
        let refs: Vec<&str> = file_caps.iter().map(String::as_str).collect();
        let mut ceiling_ok = true;
        if let Ok(indexed) = ctx.bundles.expand_indexed(&refs) {
            for (k, symbolic) in indexed {
                if let Ok(cap) = expander(ctx).expand_atom(&symbolic)
                    && !cap.covered_by(grants)
                {
                    ceiling_ok = false;
                    diags.push(Diagnostic::new(
                        "E_CAP_EXCEEDS_GRANTS",
                        layered.origin_of("subagents"),
                        idx(&format!("{p}.capabilities"), k),
                        format!("'{symbolic}'"),
                    ));
                }
            }
        }
        if let Some(def) = &sub.definition
            && !def.is_file()
        {
            diags.push(Diagnostic::new(
                "E_FILE_NOT_FOUND",
                layered.origin_of("subagents"),
                format!("{p}.definition"),
                format!("'{}'", def.display()),
            ));
        }
        if !ceiling_ok {
            continue;
        }
        match child_grants(ctx, &sub.subagent.catalog) {
            Ok(child) => {
                for cap in child {
                    if !cap.covered_by(&sub.ceiling) {
                        diags.push(Diagnostic::new(
                            "E_CAP_EXCEEDS_GRANTS",
                            layered.origin_of("subagents"),
                            format!("{p}.capabilities"),
                            format!(
                                "sub-agent '{}' holds '{}' outside its ceiling",
                                sub.subagent.catalog,
                                sym(&cap)
                            ),
                        ));
                    }
                }
            }
            Err(child_diags) => diags.extend(child_diags),
        }
    }
    // (d) notebook.
    let notebook_cap = Capability::Fs {
        path: stage.notebook.clone(),
        mode: FsMode::Rw,
    };
    if !stage.notebook.as_os_str().is_empty() && !notebook_cap.covered_by(grants) {
        diags.push(Diagnostic::new(
            "E_CAP_EXCEEDS_GRANTS",
            layered.origin_of("notebook.path"),
            "notebook.path",
            format!("requires 'fs.rw:{}'", stage.profile.notebook.path),
        ));
    }
    // (e) hidden eval set.
    if let Some(es) = &stage.eval_set
        && !es.hidden.contains("://")
    {
        let hidden = Path::new(&es.hidden);
        let reachable = hidden.starts_with(inputs.workdir)
            || Capability::Fs {
                path: hidden.to_path_buf(),
                mode: FsMode::Ro,
            }
            .covered_by(grants);
        if reachable {
            diags.push(Diagnostic::new(
                "E_HIDDEN_EVAL_REACHABLE",
                layered.origin_of("agent.eval_set.hidden"),
                "agent.eval_set.hidden",
                format!(
                    "'{}' is under the workdir or covered by an fs grant",
                    es.hidden
                ),
            ));
        }
    }
    // Skills directories (warning).
    for (i, dir) in stage.skills.iter().enumerate() {
        if !dir.is_dir() {
            stage.warnings.push(Diagnostic::new(
                "W_SKILL_PATH_MISSING",
                layered.origin_of("skills.paths"),
                idx("skills.paths", i),
                format!("'{}' does not exist", dir.display()),
            ));
        }
    }
    diags
}

fn expander<'a>(ctx: &'a Ctx<'_>) -> Expander<'a> {
    Expander {
        workdir: ctx.inputs.workdir,
        home: ctx.inputs.home,
        install: &ctx.bundles.install,
    }
}

/// Host (with port, if given) of a URL; empty when unparsable.
fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    host.to_ascii_lowercase()
}

/// The child's `grants_resolved` with layers 0–2, memoized on catalog name; a cycle yields an
/// empty set (the check terminates, D6 recursion).
fn child_grants(ctx: &Ctx<'_>, catalog_name: &str) -> Result<Vec<Capability>, Vec<Diagnostic>> {
    if let Some(state) = ctx.memo.borrow().get(catalog_name) {
        return match state {
            ChildMemo::InProgress => Ok(Vec::new()),
            ChildMemo::Done(r) => r.clone(),
        };
    }
    ctx.memo
        .borrow_mut()
        .insert(catalog_name.to_owned(), ChildMemo::InProgress);
    let entry = ctx.catalog.get(catalog_name).cloned();
    let result = match entry {
        None => Ok(Vec::new()),
        Some(entry) => resolve_entry(ctx, &entry, Layers::Base).and_then(|stage| {
            errs_of(stage.narrowing_errors.clone()).map(|()| stage.concrete_grants)
        }),
    };
    ctx.memo
        .borrow_mut()
        .insert(catalog_name.to_owned(), ChildMemo::Done(result.clone()));
    result
}

/// Layer-3 rules (§4.2): the full resolution may not be wider than the layers-0–2 one.
fn check_project_narrowing(full: &Stage, base: &Stage) -> Vec<Diagnostic> {
    let project = full.project_file.as_ref().map(|p| p.path.as_path());
    let mut diags = Vec::new();
    for g in &full.grants {
        if let Some(cap) = &g.cap
            && !cap.covered_by(&base.concrete_grants)
        {
            diags.push(Diagnostic::new(
                "E_OVERRIDE_WIDENS_GRANTS",
                project,
                g.path.clone(),
                format!("'{}'", g.sym),
            ));
        }
    }
    let f = &full.profile.sandbox;
    let b = &base.profile.sandbox;
    if f.timeout_s > b.timeout_s {
        diags.push(Diagnostic::new(
            "E_OVERRIDE_WIDENS_SANDBOX",
            project,
            "sandbox.timeout_s",
            format!("{} > {}", f.timeout_s, b.timeout_s),
        ));
    }
    if f.scratch_tmpfs_mb > b.scratch_tmpfs_mb {
        diags.push(Diagnostic::new(
            "E_OVERRIDE_WIDENS_SANDBOX",
            project,
            "sandbox.scratch_tmpfs_mb",
            format!("{} > {}", f.scratch_tmpfs_mb, b.scratch_tmpfs_mb),
        ));
    }
    for (i, name) in list_at(&full.layered.table, "sandbox.env_allow")
        .iter()
        .enumerate()
    {
        if !b.env_allow.contains(name) {
            diags.push(Diagnostic::new(
                "E_OVERRIDE_WIDENS_SANDBOX",
                project,
                idx("sandbox.env_allow", i),
                format!("'{name}'"),
            ));
        }
    }
    if f.network && !b.network {
        diags.push(Diagnostic::new(
            "E_OVERRIDE_WIDENS_SANDBOX",
            project,
            "sandbox.network",
            "true > false",
        ));
    }
    diags
}

// ---- step 10 ---------------------------------------------------------------------------------

fn assemble_prompt(
    stage: &Stage,
    inputs: &ResolveInputs<'_>,
    warnings: &mut Vec<Diagnostic>,
) -> Result<Vec<PromptBlock>, Vec<Diagnostic>> {
    let p = &stage.profile;
    let mut sources = PromptSources {
        model: p.prompt.clone().map(|t| (t, p.model.id.clone())),
        role: p
            .agent
            .role_prompt
            .clone()
            .map(|t| (t, p.agent.name.clone())),
        agents_md: None,
        notebook: None,
    };
    let agents_md = &stage.agents_md;
    if agents_md.is_file() {
        let text = std::fs::read_to_string(agents_md).map_err(|e| {
            vec![Diagnostic::new(
                "E_FILE_NOT_FOUND",
                stage.layered.origin_of("agent.agents_md"),
                "agent.agents_md",
                format!("'{}': {e}", agents_md.display()),
            )]
        })?;
        sources.agents_md = Some((text, agents_md.to_string_lossy().into_owned()));
    } else {
        warnings.push(Diagnostic::new(
            "W_AGENTS_MD_MISSING",
            stage.layered.origin_of("agent.agents_md"),
            "agent.agents_md",
            format!("'{}' not found; block omitted", agents_md.display()),
        ));
    }
    if inputs.resume && p.notebook.inject_on_resume && stage.notebook.is_file() {
        let text = std::fs::read_to_string(&stage.notebook).unwrap_or_default();
        sources.notebook = Some((text, stage.notebook.to_string_lossy().into_owned()));
    }
    Ok(prompt::assemble(&sources))
}
