//! Resolution behaviours beyond §9: the §6.2 worked example, the parser slot, runtime
//! overrides, sub-agent recursion, `Diagnostic` display, and the bundles expansion.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use common::*;
use kernel::{Hash, MiddlewareSource, TOOL_CALL_PARSER_PRIORITY};
use profiles::{Bundles, Diagnostic};

#[test]
fn diagnostic_display_is_code_at_basename_colon_path_colon_message() {
    let d = Diagnostic::new(
        "E_UNKNOWN_KEY",
        Some(Path::new("/x/y/profiles/agents/default.toml")),
        "agent.contxt_budget_tokens",
        "unknown key",
    );
    assert_eq!(
        d.to_string(),
        "E_UNKNOWN_KEY at default.toml:agent.contxt_budget_tokens: unknown key"
    );
    assert_eq!(
        d.file,
        Some(PathBuf::from("/x/y/profiles/agents/default.toml"))
    );
    assert!(d.is_error());
    assert!(!d.is_warning());
    let w = Diagnostic::new("W_NET_MASKED", None, "sandbox.network", "masked");
    assert_eq!(w.to_string(), "W_NET_MASKED at sandbox.network: masked");
    assert!(w.is_warning());
}

fn registry_with_middleware(t: &Tree, names: &[&str]) -> profiles::Registry {
    let mut reg = registry_for(&t.workdir);
    reg.middleware = names.iter().map(|s| (*s).to_owned()).collect();
    reg
}

#[test]
fn merge_worked_example_of_section_6_2() {
    let t = Tree::new();
    t.write(
        "models/stand-in.toml",
        r#"schema_version = 1
[model]
id = "stand-in/default"
endpoint = "litellm-ci"
context_length = 32768
temperature = 0.0
max_output_tokens = 2048
[compaction]
trigger_at = 0.80
target = 0.45
[[middleware]]
name = "thinking_strip"
priority = 150
"#,
    );
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}"]
[tools]
allow = ["read"]
[sandbox]
timeout_s = 600
env_allow = ["PATH", "HOME", "LANG", "LC_ALL", "TERM", "TZ"]
[[middleware]]
name = "compaction"
priority = 300
config = { style = "paragraph", keep_last = 6 }
[[middleware]]
name = "budget"
priority = 800
"#,
    );
    t.write_workdir(
        ".grist/agent.toml",
        r#"schema_version = 1
[model]
temperature = 0.3
[sandbox]
timeout_s = 120
env_allow = ["PATH", "LANG"]
[[middleware]]
name = "compaction"
priority = 250
config = { style = "notebook" }
"#,
    );
    let reg = registry_with_middleware(&t, &["thinking_strip", "compaction", "budget"]);
    let r = t
        .resolve_with("default", &reg, &toml::Table::new(), false)
        .unwrap();
    let p = &r.resolved_profile;
    assert_eq!(p.model.temperature, Some(0.3));
    assert_eq!(p.model.max_output_tokens, 2048);
    assert_eq!(p.compaction.trigger_at, 0.80);
    assert_eq!(p.compaction.target, 0.45);
    assert_eq!(p.sandbox.timeout_s, 120);
    assert_eq!(p.sandbox.env_allow, ["LANG", "PATH"]);
    assert_eq!(p.sandbox.scratch_tmpfs_mb, 256);
    assert!(!p.sandbox.network);
    let chain: Vec<(String, i32, MiddlewareSource)> = r
        .kernel_inputs
        .middleware
        .iter()
        .map(|m| (m.name.clone(), m.priority, m.source))
        .collect();
    assert_eq!(
        chain,
        [
            ("thinking_strip".to_owned(), 150, MiddlewareSource::Model),
            ("compaction".to_owned(), 250, MiddlewareSource::Project),
            ("budget".to_owned(), 800, MiddlewareSource::Agent),
            ("recorder".to_owned(), 990, MiddlewareSource::Kernel),
        ]
    );
    let compaction = &r.kernel_inputs.middleware[1];
    assert_eq!(
        compaction.config,
        serde_json::json!({ "style": "notebook" })
    );
    assert_eq!(
        compaction.config_hash,
        Some(Hash::of_canonical_json(&serde_json::json!({ "style": "notebook" })).unwrap())
    );
    assert_eq!(r.kernel_inputs.middleware[0].config_hash, None);
    assert_eq!(p.middleware.len(), 4);
    assert_eq!(
        p.middleware[1].config,
        serde_json::json!({ "style": "notebook" })
    );
    assert_eq!(
        r.profile_loads.last().unwrap().kind,
        kernel::ProfileKind::Project
    );
    assert!(r.active_profiles.project_profile_hash.is_some());
    assert_eq!(r.kernel_inputs.model_params.temperature, Some(0.3));
    assert_eq!(r.kernel_inputs.sandbox_limits.timeout.as_secs(), 120);
}

#[test]
fn project_cannot_override_a_model_slot_entry() {
    let t = Tree::new();
    t.write(
        "models/stand-in.toml",
        r#"schema_version = 1
[model]
id = "stand-in/default"
endpoint = "litellm-ci"
context_length = 32768
[[middleware]]
name = "thinking_strip"
priority = 150
"#,
    );
    t.write_workdir(
        ".grist/agent.toml",
        "schema_version = 1\n[[middleware]]\nname = \"thinking_strip\"\npriority = 300\n",
    );
    let reg = registry_with_middleware(&t, &["thinking_strip"]);
    assert_first(
        t.resolve_with("default", &reg, &toml::Table::new(), false),
        "E_PRIORITY_RANGE at agent.toml:middleware[0].name: 'thinking_strip'",
    );
}

#[test]
fn stable_sort_keeps_declaration_order_on_ties() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}"]
[tools]
allow = ["read"]
[[middleware]]
name = "b_first"
priority = 300
[[middleware]]
name = "a_second"
priority = 300
"#,
    );
    let reg = registry_with_middleware(&t, &["b_first", "a_second"]);
    let r = t
        .resolve_with("default", &reg, &toml::Table::new(), false)
        .unwrap();
    let names: Vec<&str> = r
        .kernel_inputs
        .middleware
        .iter()
        .map(|m| m.name.as_str())
        .collect();
    assert_eq!(names, ["b_first", "a_second", "recorder"]);
}

#[test]
fn parser_slot_is_synthesized_for_parsed_tool_format() {
    let t = Tree::new();
    t.write(
        "models/stand-in.toml",
        r#"schema_version = 1
[model]
id = "ft/qwen3"
endpoint = "vllm-ft"
context_length = 65536
tool_format = "parsed:hermes"
"#,
    );
    let mut reg = registry_for(&t.workdir);
    reg.parsers = BTreeSet::from(["hermes".to_owned()]);
    let r = t
        .resolve_with("default", &reg, &toml::Table::new(), false)
        .unwrap();
    let m = &r.kernel_inputs.middleware;
    assert_eq!(m.len(), 2);
    assert_eq!(m[0].name, "tool_call_parser");
    assert_eq!(m[0].priority, TOOL_CALL_PARSER_PRIORITY);
    assert_eq!(m[0].source, MiddlewareSource::Model);
    assert_eq!(m[0].config, serde_json::json!({ "syntax": "hermes" }));
    assert!(m[0].config_hash.is_some());
    assert_eq!(m[1].name, "recorder");
    assert_eq!(r.kernel_inputs.tool_format, "parsed:hermes");
    // A declared entry at 100 is used as-is.
    t.write(
        "models/stand-in.toml",
        r#"schema_version = 1
[model]
id = "ft/qwen3"
endpoint = "vllm-ft"
context_length = 65536
tool_format = "parsed:hermes"
[[middleware]]
name = "tool_call_parser"
priority = 100
config = { syntax = "hermes", strict = true }
"#,
    );
    let r = t
        .resolve_with("default", &reg, &toml::Table::new(), false)
        .unwrap();
    assert_eq!(
        r.kernel_inputs.middleware[0].config,
        serde_json::json!({ "syntax": "hermes", "strict": true })
    );
}

#[test]
fn runtime_overrides_are_whitelisted_typed_and_hashed() {
    let t = Tree::new();
    let mut ov = toml::Table::new();
    ov.insert("model.temperature".into(), toml::Value::Float(0.7));
    ov.insert(
        "agent.context_budget_tokens".into(),
        toml::Value::Integer(30000),
    );
    ov.insert("model.thinking.enabled".into(), toml::Value::Boolean(true));
    ov.insert(
        "model.thinking.budget_tokens".into(),
        toml::Value::Integer(512),
    );
    ov.insert(
        "notebook.path".into(),
        toml::Value::String("${workdir}/.grist/nb.md".into()),
    );
    let base = t.resolve("default").unwrap();
    let r = t
        .resolve_with("default", &registry_for(&t.workdir), &ov, false)
        .unwrap();
    assert_eq!(r.kernel_inputs.model_params.temperature, Some(0.7));
    assert_eq!(r.kernel_inputs.context_budget_tokens, 30000);
    let th = r.kernel_inputs.model_params.thinking.clone().unwrap();
    assert!(th.enabled);
    assert_eq!(th.budget_tokens, Some(512));
    assert_eq!(
        r.kernel_inputs.notebook_path,
        Some(t.workdir.join(".grist/nb.md"))
    );
    assert_eq!(r.resolved_profile.runtime_overrides.len(), 5);
    assert_ne!(r.resolved_profile_hash, base.resolved_profile_hash);
    assert_eq!(r.profile_loads.len(), 3);

    let mut bad = toml::Table::new();
    bad.insert("model.id".into(), toml::Value::String("x".into()));
    assert_first(
        t.resolve_with("default", &registry_for(&t.workdir), &bad, false),
        "E_OVERRIDE_FORBIDDEN_KEY at model.id",
    );
    let mut bad = toml::Table::new();
    bad.insert("model.temperature".into(), toml::Value::Float(5.0));
    assert_first(
        t.resolve_with("default", &registry_for(&t.workdir), &bad, false),
        "E_VALUE_RANGE at model.temperature",
    );
}

#[test]
fn subagent_child_grants_are_checked_against_the_ceiling() {
    let t = Tree::new();
    t.write(
        "agents/debugger.toml",
        r#"schema_version = 1
[agent]
name = "debugger"
[capabilities]
grants = ["fs.rw:${workdir}", "proc:python3"]
[tools]
allow = []
"#,
    );
    t.write(
        "agents/orchestrator.toml",
        r#"schema_version = 1
[agent]
name = "orchestrator"
[capabilities]
grants = ["fs.rw:${workdir}", "proc:python3"]
[tools]
allow = ["read"]
[[subagents]]
name = "dbg"
catalog = "debugger"
capabilities = ["fs.rw:${workdir}"]
"#,
    );
    t.catalog_with(&["orchestrator", "debugger"]);
    assert_first(
        t.resolve("orchestrator"),
        "E_CAP_EXCEEDS_GRANTS at orchestrator.toml:subagents[0].capabilities: sub-agent 'debugger' holds 'proc:python3'",
    );
    // Widen the ceiling: fine, and the spawn atom is derived.
    t.write(
        "agents/orchestrator.toml",
        r#"schema_version = 1
[agent]
name = "orchestrator"
[capabilities]
grants = ["fs.rw:${workdir}", "proc:python3"]
[tools]
allow = ["read"]
[[subagents]]
name = "dbg"
catalog = "debugger"
capabilities = ["fs.rw:${workdir}", "proc:python3"]
"#,
    );
    let r = t.resolve("orchestrator").unwrap();
    assert!(
        r.resolved_profile
            .grants
            .contains(&"spawn:debugger".to_owned())
    );
    assert_eq!(r.kernel_inputs.subagents.len(), 1);
    assert_eq!(r.kernel_inputs.subagents[0].ceiling.len(), 2);
}

#[test]
fn subagent_recursion_terminates() {
    let t = Tree::new();
    t.write(
        "agents/orchestrator.toml",
        r#"schema_version = 1
[agent]
name = "orchestrator"
[capabilities]
grants = ["fs.rw:${workdir}"]
[tools]
allow = ["read"]
[[subagents]]
name = "self"
catalog = "orchestrator"
capabilities = ["fs.rw:${workdir}"]
"#,
    );
    t.catalog_with(&["orchestrator"]);
    // The child's derived `tool:read` and `spawn:orchestrator` are outside the ceiling.
    let err = t.resolve("orchestrator").unwrap_err();
    assert!(
        err.iter().all(|d| d.code == "E_CAP_EXCEEDS_GRANTS"),
        "{err:?}"
    );
    t.write(
        "agents/orchestrator.toml",
        r#"schema_version = 1
[agent]
name = "orchestrator"
[capabilities]
grants = ["fs.rw:${workdir}"]
[tools]
allow = ["read"]
[[subagents]]
name = "self"
catalog = "orchestrator"
capabilities = ["fs.rw:${workdir}", "tool:read", "spawn:orchestrator"]
"#,
    );
    assert!(t.resolve("orchestrator").is_ok());
}

#[test]
fn bundles_expand_dedupe_and_sort() {
    let t = Tree::new();
    let b = Bundles::load(&t.profiles.join("bundles.toml")).unwrap();
    assert_eq!(b.install["solver"], "/opt/inhouse/solver");
    let atoms = b
        .expand(&["solver", "fs.rw:${workdir}", "post", "proc:bash"])
        .unwrap();
    assert_eq!(
        atoms,
        [
            "fs.ro:${install:post}",
            "fs.ro:${install:solver}",
            "fs.rw:${workdir}",
            "proc:bash",
            "proc:sbatch",
            "proc:scancel",
            "proc:squeue",
        ]
    );
    let err = b.expand(&["thermal"]).unwrap_err();
    assert_eq!(err[0].code, "E_UNKNOWN_BUNDLE");
    // Bundles in an agent profile expand with the install placeholder bound.
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["solver"]
[tools]
allow = ["read"]
"#,
    );
    let r = t.resolve("default").unwrap();
    let mounts: Vec<String> = r
        .kernel_inputs
        .envelope_policy
        .mounts
        .iter()
        .map(|m| format!("{}:{:?}", m.path.display(), m.mode))
        .collect();
    assert!(mounts.contains(&"/opt/inhouse/solver:Ro".to_owned()));
    assert!(r.kernel_inputs.envelope_policy.programs.contains("sbatch"));
}

#[test]
fn text_source_file_is_read_relative_to_the_profile_and_hashed_by_content() {
    let t = Tree::new();
    t.write("agents/default.role.md", "Be terse.\n");
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[agent.role_prompt]
file = "default.role.md"
[capabilities]
grants = ["fs.rw:${workdir}"]
[tools]
allow = ["read"]
"#,
    );
    let a = t.resolve("default").unwrap();
    assert_eq!(
        a.resolved_profile.agent.role_prompt.as_deref(),
        Some("Be terse.\n")
    );
    t.write("agents/default.role.md", "Be verbose.\n");
    let b = t.resolve("default").unwrap();
    assert_ne!(a.resolved_profile_hash, b.resolved_profile_hash);
    // The agent file itself did not change, so its source hash did not either.
    assert_eq!(
        a.active_profiles.agent_profile_hash,
        b.active_profiles.agent_profile_hash
    );
}

#[test]
fn catalog_lists_entries_and_missing_entry_is_a_catalog_ref_error() {
    let t = Tree::new();
    let c = profiles::Catalog::load(&t.profiles.join("catalog.toml")).unwrap();
    assert_eq!(c.entries().len(), 1);
    assert_eq!(
        c.get("default").unwrap().agent_profile,
        t.profiles.join("agents/default.toml")
    );
    assert!(c.get("nope").is_none());
    assert_first(
        t.resolve("nope"),
        "E_CATALOG_REF at catalog.toml:agents: no entry named 'nope'",
    );
}
