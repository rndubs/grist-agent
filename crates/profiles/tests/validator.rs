//! `profile-schema.md` §9: one test per rejection case (`rejects_<nn>_<slug>`) plus the
//! warnings (`warns_<slug>`). Each asserts on the `Display` prefix of the first diagnostic.

mod common;

use common::*;
use kernel::Hash;

const MODEL_HEAD: &str = r#"schema_version = 1
[model]
id = "stand-in/default"
endpoint = "litellm-ci"
context_length = 32768
"#;

#[test]
fn rejects_01a_unknown_key_top_level() {
    let t = Tree::new();
    t.write(
        "models/stand-in.toml",
        &format!("{MODEL_HEAD}[modle]\ntemperature = 0.1\n"),
    );
    assert_first(t.resolve("default"), "E_UNKNOWN_KEY at stand-in.toml:modle");
}

#[test]
fn rejects_01b_unknown_key_table_level() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
contxt_budget_tokens = 40000
[tools]
allow = ["read"]
"#,
    );
    assert_first(
        t.resolve("default"),
        "E_UNKNOWN_KEY at default.toml:agent.contxt_budget_tokens",
    );
}

#[test]
fn rejects_01c_unknown_key_array_of_tables_level() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[[middleware]]
name = "compaction"
priority = 300
prio = 300
"#,
    );
    assert_first(
        t.resolve("default"),
        "E_UNKNOWN_KEY at default.toml:middleware[0].prio",
    );
}

#[test]
fn rejects_02_kernel_only_key() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[spill]
enabled = false
"#,
    );
    assert_first(
        t.resolve("default"),
        "E_KERNEL_ONLY_KEY at default.toml:spill",
    );
    // Kernel-only keys under [sandbox] too.
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[sandbox]
seccomp = "strict"
"#,
    );
    assert_first(
        t.resolve("default"),
        "E_KERNEL_ONLY_KEY at default.toml:sandbox.seccomp",
    );
}

#[test]
fn rejects_03_tool_capability_exceeds_grants() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.ro:${workdir}"]
[tools]
allow = ["write"]
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_CAP_EXCEEDS_GRANTS at default.toml:tools.allow[0]: tool 'write' requires 'fs.rw:${workdir}'",
    );
}

#[test]
fn rejects_04_mcp_server_capability_exceeds_grants() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}", "proc:npx"]
[tools]
allow = ["read"]
[[mcp_servers]]
name = "docs"
transport = "stdio"
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/srv/docs"]
capabilities = ["proc:npx", "fs.ro:/srv/docs"]
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_CAP_EXCEEDS_GRANTS at default.toml:mcp_servers[0].capabilities[1]: 'fs.ro:/srv/docs'",
    );
}

const DEBUGGER: &str = r#"schema_version = 1
[agent]
name = "debugger"
[capabilities]
grants = ["solver"]
[tools]
allow = []
"#;

#[test]
fn rejects_05_subagent_ceiling_exceeds_parent_grants() {
    let t = Tree::new();
    t.write("agents/debugger.toml", DEBUGGER);
    t.write(
        "agents/orchestrator.toml",
        r#"schema_version = 1
[agent]
name = "orchestrator"
[capabilities]
grants = ["fs.rw:${workdir}", "solver"]
[tools]
allow = ["read"]
[[subagents]]
name = "dbg"
catalog = "debugger"
capabilities = ["solver", "net:*"]
"#,
    );
    t.catalog_with(&["orchestrator", "debugger"]);
    assert_first_exact(
        t.resolve("orchestrator"),
        "E_CAP_EXCEEDS_GRANTS at orchestrator.toml:subagents[0].capabilities[1]: 'net:*'",
    );
}

#[test]
fn rejects_06_unknown_bundle_name() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}", "meshing", "thermal"]
[tools]
allow = ["read"]
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_UNKNOWN_BUNDLE at default.toml:capabilities.grants[2]: 'thermal'",
    );
}

#[test]
fn rejects_07_malformed_atom_string() {
    let t = Tree::new();
    for atom in ["fs.rwx:${workdir}", "net:", "proc:", "fs.ro:", "Fs.ro:/x"] {
        t.write(
            "agents/default.toml",
            &format!(
                "schema_version = 1\n[agent]\nname = \"default\"\n[capabilities]\ngrants = [\"{}\"]\n[tools]\nallow = [\"read\"]\n",
                atom.replace('"', "\\\"")
            ),
        );
        assert_first_exact(
            t.resolve("default"),
            &format!("E_MALFORMED_ATOM at default.toml:capabilities.grants[0]: '{atom}'"),
        );
    }
}

#[test]
fn rejects_08_path_not_absolute_after_expansion() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}", "fs.ro:data/meshes"]
[tools]
allow = ["read"]
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_PATH_NOT_ABSOLUTE at default.toml:capabilities.grants[1]: 'data/meshes'",
    );
}

#[test]
fn rejects_09_dotdot_in_path() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}/../shared"]
[tools]
allow = ["read"]
"#,
    );
    assert_first(
        t.resolve("default"),
        "E_PATH_DOTDOT at default.toml:capabilities.grants[0]",
    );
}

#[test]
fn rejects_10_duplicate_middleware_name_in_one_file() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[[middleware]]
name = "compaction"
priority = 300
[[middleware]]
name = "compaction"
priority = 310
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_DUP_MIDDLEWARE at default.toml:middleware[1].name: 'compaction'",
    );
}

#[test]
fn rejects_11a_priority_outside_range_agent_uses_model_slot() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[[middleware]]
name = "compaction"
priority = 150
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_PRIORITY_RANGE at default.toml:middleware[0].priority: 150 not in 200..=899",
    );
}

#[test]
fn rejects_11b_priority_outside_range_model_uses_agent_slot() {
    let t = Tree::new();
    t.write(
        "models/stand-in.toml",
        &format!("{MODEL_HEAD}[[middleware]]\nname = \"thinking_strip\"\npriority = 300\n"),
    );
    assert_first_exact(
        t.resolve("default"),
        "E_PRIORITY_RANGE at stand-in.toml:middleware[0].priority: 300 not in 100..=199",
    );
}

#[test]
fn rejects_11c_priority_outside_range_project_uses_kernel_slot() {
    let t = Tree::new();
    t.write_workdir(
        ".grist/agent.toml",
        r#"schema_version = 1
[[middleware]]
name = "budget"
priority = 950
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_PRIORITY_RANGE at agent.toml:middleware[0].priority: 950 not in 200..=899",
    );
}

const FT_MODEL: &str = r#"schema_version = 1
[model]
id = "ft/qwen3-32b-2026-08"
endpoint = "vllm-ft"
context_length = 65536
tool_format = "parsed:hermes"
"#;

fn ft_catalog(t: &Tree) {
    t.write(
        "catalog.toml",
        r#"schema_version = 1
[[agents]]
name = "default"
model_profile = "models/ft-qwen3.toml"
agent_profile = "agents/default.toml"
"#,
    );
}

#[test]
fn rejects_12_parsed_syntax_without_parser() {
    let t = Tree::new();
    t.write("models/ft-qwen3.toml", FT_MODEL);
    ft_catalog(&t);
    assert_first_exact(
        t.resolve("default"),
        "E_PARSER_UNAVAILABLE at ft-qwen3.toml:model.tool_format: no parser for 'hermes'",
    );
}

#[test]
fn rejects_13_value_ranges() {
    let t = Tree::new();
    let agent = |extra: &str| {
        format!(
            "schema_version = 1\n[agent]\nname = \"default\"\n{extra}\n[tools]\nallow = [\"read\"]\n"
        )
    };
    t.write("agents/default.toml", &agent("context_budget_tokens = 0"));
    assert_first_exact(
        t.resolve("default"),
        "E_VALUE_RANGE at default.toml:agent.context_budget_tokens: must be > 0",
    );
    t.write("agents/default.toml", &agent("spill_cap_bytes = 0"));
    assert_first(
        t.resolve("default"),
        "E_VALUE_RANGE at default.toml:agent.spill_cap_bytes",
    );
    t.write("agents/default.toml", &agent("[sandbox]\ntimeout_s = 0"));
    assert_first(
        t.resolve("default"),
        "E_VALUE_RANGE at default.toml:sandbox.timeout_s: must be > 0",
    );
    t.write("agents/default.toml", MIN_AGENT);
    t.write(
        "models/stand-in.toml",
        r#"schema_version = 1
[model]
id = "stand-in/default"
endpoint = "litellm-ci"
context_length = 0
"#,
    );
    assert_first(
        t.resolve("default"),
        "E_VALUE_RANGE at stand-in.toml:model.context_length: must be > 0",
    );
    t.write(
        "models/stand-in.toml",
        &format!("{MODEL_HEAD}[compaction]\ntrigger_at = 0.8\ntarget = 0.9\n"),
    );
    assert_first(
        t.resolve("default"),
        "E_VALUE_RANGE at stand-in.toml:compaction.target",
    );
    t.write(
        "models/stand-in.toml",
        &format!("{MODEL_HEAD}temperature = 3.0\n"),
    );
    assert_first(
        t.resolve("default"),
        "E_VALUE_RANGE at stand-in.toml:model.temperature: must be in [0, 2]",
    );
}

#[test]
fn rejects_14_hidden_eval_set_reachable() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[agent.eval_set]
dev = "evals/dev"
hidden = "${workdir}/evals/hidden"
[capabilities]
grants = ["fs.rw:${workdir}"]
[tools]
allow = ["read"]
"#,
    );
    assert_first(
        t.resolve("default"),
        "E_HIDDEN_EVAL_REACHABLE at default.toml:agent.eval_set.hidden",
    );
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[agent.eval_set]
dev = "evals/dev"
hidden = "/data/evals/hidden"
[capabilities]
grants = ["fs.rw:${workdir}", "fs.ro:/data"]
[tools]
allow = ["read"]
"#,
    );
    assert_first(
        t.resolve("default"),
        "E_HIDDEN_EVAL_REACHABLE at default.toml:agent.eval_set.hidden",
    );
    // Not reachable: outside the workdir and not covered.
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[agent.eval_set]
dev = "evals/dev"
hidden = "/data/evals/hidden"
[capabilities]
grants = ["fs.rw:${workdir}"]
[tools]
allow = ["read"]
"#,
    );
    assert!(t.resolve("default").is_ok());
}

#[test]
fn rejects_15_project_override_widening_grants() {
    let t = Tree::new();
    t.write_workdir(
        ".grist/agent.toml",
        r#"schema_version = 1
[capabilities]
grants = ["fs.rw:${workdir}", "net:*"]
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_OVERRIDE_WIDENS_GRANTS at agent.toml:capabilities.grants[1]: 'net:*'",
    );
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}", "proc:python3"]
[tools]
allow = ["read"]
"#,
    );
    t.write_workdir(
        ".grist/agent.toml",
        r#"schema_version = 1
[tools]
allow = ["read", "python"]
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_OVERRIDE_WIDENS_GRANTS at agent.toml:tools.allow[1]: 'tool:python'",
    );
}

#[test]
fn rejects_16_sandbox_none_in_non_dev_build() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[sandbox]
backend = "none"
"#,
    );
    assert_first(
        t.resolve("default"),
        "E_SANDBOX_NONE_FORBIDDEN at default.toml:sandbox.backend",
    );
    // Accepted in a dev build (the missing grants then surface as the next step's error).
    let mut reg = registry_for(&t.workdir);
    reg.dev_build = true;
    assert_first(
        t.resolve_with("default", &reg, &toml::Table::new(), false),
        "E_CAP_EXCEEDS_GRANTS at default.toml:tools.allow[0]",
    );
}

#[test]
fn rejects_17_project_override_forbidden_key() {
    let t = Tree::new();
    t.write_workdir(
        ".grist/agent.toml",
        "schema_version = 1\n[agent]\nname = \"something-else\"\n",
    );
    assert_first(
        t.resolve("default"),
        "E_OVERRIDE_FORBIDDEN_KEY at agent.toml:agent.name",
    );
    for (snippet, path) in [
        ("[model]\nid = \"x\"\n", "model.id"),
        ("[agent.eval_set]\nhidden = \"/x\"\n", "agent.eval_set"),
        ("[sandbox]\nbackend = \"bwrap\"\n", "sandbox.backend"),
        (
            "[fine_tune]\ntrained_against_profile_hash = \"b3:0000000000000000000000000000000000000000000000000000000000000000\"\n",
            "fine_tune",
        ),
    ] {
        t.write_workdir(
            ".grist/agent.toml",
            &format!("schema_version = 1\n{snippet}"),
        );
        assert_first(
            t.resolve("default"),
            &format!("E_OVERRIDE_FORBIDDEN_KEY at agent.toml:{path}"),
        );
    }
}

#[test]
fn rejects_18_project_override_widening_sandbox() {
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
[sandbox]
timeout_s = 600
network = false
env_allow = ["PATH"]
"#,
    );
    t.write_workdir(
        ".grist/agent.toml",
        "schema_version = 1\n[sandbox]\ntimeout_s = 3600\n",
    );
    assert_first_exact(
        t.resolve("default"),
        "E_OVERRIDE_WIDENS_SANDBOX at agent.toml:sandbox.timeout_s: 3600 > 600",
    );
    t.write_workdir(
        ".grist/agent.toml",
        "schema_version = 1\n[sandbox]\nnetwork = true\n",
    );
    assert_first(
        t.resolve("default"),
        "E_OVERRIDE_WIDENS_SANDBOX at agent.toml:sandbox.network",
    );
    t.write_workdir(
        ".grist/agent.toml",
        "schema_version = 1\n[sandbox]\nenv_allow = [\"PATH\", \"HOME\"]\n",
    );
    assert_first(
        t.resolve("default"),
        "E_OVERRIDE_WIDENS_SANDBOX at agent.toml:sandbox.env_allow[1]: 'HOME'",
    );
    t.write_workdir(
        ".grist/agent.toml",
        "schema_version = 1\n[sandbox]\nscratch_tmpfs_mb = 4096\n",
    );
    assert_first(
        t.resolve("default"),
        "E_OVERRIDE_WIDENS_SANDBOX at agent.toml:sandbox.scratch_tmpfs_mb: 4096 > 256",
    );
    // Narrowing is fine.
    t.write_workdir(
        ".grist/agent.toml",
        "schema_version = 1\n[sandbox]\ntimeout_s = 120\nenv_allow = []\nscratch_tmpfs_mb = 64\n",
    );
    let r = t.resolve("default").unwrap();
    assert_eq!(r.resolved_profile.sandbox.timeout_s, 120);
}

#[test]
fn rejects_19_unknown_middleware_name() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[[middleware]]
name = "foo"
priority = 300
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_MIDDLEWARE_UNKNOWN at default.toml:middleware[0].name: 'foo'",
    );
}

#[test]
fn rejects_20_unknown_tool() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read", "browse"]
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_TOOL_UNKNOWN at default.toml:tools.allow[1]: 'browse'",
    );
    t.write("agents/default.toml", MIN_AGENT);
    t.write(
        "models/stand-in.toml",
        &format!("{MODEL_HEAD}[tool_descriptions]\nbrowse = \"Browse the web.\"\n"),
    );
    assert_first(
        t.resolve("default"),
        "E_TOOL_UNKNOWN at stand-in.toml:tool_descriptions.browse",
    );
}

#[test]
fn rejects_21_env_allow_secret_pattern() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[sandbox]
env_allow = ["PATH", "OPENAI_API_KEY"]
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_ENV_ALLOW_SECRET_PATTERN at default.toml:sandbox.env_allow[1]: 'OPENAI_API_KEY'",
    );
}

#[test]
fn rejects_22_schema_version_missing_or_newer() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        "schema_version = 2\n[agent]\nname = \"default\"\n[tools]\nallow = [\"read\"]\n",
    );
    assert_first_exact(
        t.resolve("default"),
        "E_SCHEMA_VERSION at default.toml:schema_version: 2 unsupported (max 1)",
    );
    t.write(
        "agents/default.toml",
        "[agent]\nname = \"default\"\n[tools]\nallow = [\"read\"]\n",
    );
    assert_first_exact(
        t.resolve("default"),
        "E_SCHEMA_VERSION at default.toml:schema_version: missing",
    );
    t.write(
        "agents/default.toml",
        "schema_version = \"1\"\n[agent]\nname = \"default\"\n[tools]\nallow = [\"read\"]\n",
    );
    assert_first_exact(
        t.resolve("default"),
        "E_SCHEMA_VERSION at default.toml:schema_version: must be an integer",
    );
}

#[test]
fn rejects_23_text_source_both_or_neither() {
    let t = Tree::new();
    t.write(
        "models/stand-in.toml",
        &format!("{MODEL_HEAD}[prompt]\ntext = \"inline\"\nfile = \"prompt.md\"\n"),
    );
    assert_first_exact(
        t.resolve("default"),
        "E_TEXT_SOURCE at stand-in.toml:prompt: exactly one of 'text' or 'file'",
    );
    t.write(
        "models/stand-in.toml",
        &format!("{MODEL_HEAD}[prompt]\nother = \"x\"\n"),
    );
    assert_first(
        t.resolve("default"),
        "E_TEXT_SOURCE at stand-in.toml:prompt",
    );
}

#[test]
fn rejects_24_required_key_missing_after_merge() {
    let t = Tree::new();
    t.write(
        "models/stand-in.toml",
        "schema_version = 1\n[model]\nid = \"stand-in/default\"\nendpoint = \"litellm-ci\"\n",
    );
    assert_first(
        t.resolve("default"),
        "E_MISSING_KEY at stand-in.toml:model.context_length",
    );
}

#[test]
fn rejects_25_unknown_placeholder() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${repo}"]
[tools]
allow = ["read"]
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_UNKNOWN_PLACEHOLDER at default.toml:capabilities.grants[0]: '${repo}'",
    );
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}", "fs.ro:${install:thermal}"]
[tools]
allow = ["read"]
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_UNKNOWN_PLACEHOLDER at default.toml:capabilities.grants[1]: '${install:thermal}'",
    );
}

#[test]
fn rejects_26_parser_slot_violated() {
    let t = Tree::new();
    t.write(
        "models/ft-qwen3.toml",
        &format!("{FT_MODEL}[[middleware]]\nname = \"tool_call_parser\"\npriority = 120\n"),
    );
    ft_catalog(&t);
    assert_first_exact(
        t.resolve("default"),
        "E_PARSER_SLOT at ft-qwen3.toml:middleware[0].priority: tool_call_parser must be 100",
    );
    t.write(
        "models/ft-qwen3.toml",
        &format!(
            "{}[[middleware]]\nname = \"tool_call_parser\"\npriority = 100\n",
            FT_MODEL.replace("parsed:hermes", "native")
        ),
    );
    assert_first_exact(
        t.resolve("default"),
        "E_PARSER_SLOT at ft-qwen3.toml:middleware[0].name: no parser without parsed tool_format",
    );
}

#[test]
fn rejects_27_catalog_reference_errors() {
    let t = Tree::new();
    t.write(
        "catalog.toml",
        r#"schema_version = 1
[[agents]]
name = "default"
model_profile = "models/stand-in.toml"
agent_profile = "agents/default.toml"
[[agents]]
name = "default"
model_profile = "models/stand-in.toml"
agent_profile = "agents/other.toml"
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_DUP_NAME at catalog.toml:agents[1].name: 'default'",
    );
    // Sub-agent naming an absent entry.
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
name = "x"
catalog = "nope"
capabilities = []
"#,
    );
    t.catalog_with(&["orchestrator"]);
    assert_first_exact(
        t.resolve("orchestrator"),
        "E_CATALOG_REF at orchestrator.toml:subagents[0].catalog: 'nope'",
    );
    // Agent file whose [agent].name differs from its entry.
    t.write("catalog.toml", CATALOG);
    t.write(
        "agents/default.toml",
        &MIN_AGENT.replace("\"default\"", "\"dflt\""),
    );
    assert_first_exact(
        t.resolve("default"),
        "E_CATALOG_REF at default.toml:agent.name: 'dflt' != catalog entry 'default'",
    );
    // Missing file.
    t.write("agents/default.toml", MIN_AGENT);
    t.write(
        "catalog.toml",
        &CATALOG.replace("agents/default.toml", "agents/missing.toml"),
    );
    assert_first(
        t.resolve("default"),
        "E_FILE_NOT_FOUND at catalog.toml:agents[0].agent_profile",
    );
    // No `default` entry.
    t.write(
        "catalog.toml",
        &CATALOG.replace("name = \"default\"", "name = \"other\""),
    );
    t.write(
        "agents/default.toml",
        &MIN_AGENT.replace("\"default\"", "\"other\""),
    );
    assert_first(t.resolve("other"), "E_CATALOG_REF at catalog.toml:agents");
}

#[test]
fn rejects_28_derived_atom_in_grants() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}", "tool:python"]
[tools]
allow = ["read"]
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_DERIVED_ATOM_IN_GRANTS at default.toml:capabilities.grants[1]: use [tools].allow",
    );
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}", "spawn:debugger"]
[tools]
allow = ["read"]
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_DERIVED_ATOM_IN_GRANTS at default.toml:capabilities.grants[1]: use [[subagents]]",
    );
}

#[test]
fn rejects_29_kernel_reserved_middleware_name() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[[middleware]]
name = "recorder"
priority = 300
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_RESERVED_MIDDLEWARE_NAME at default.toml:middleware[0].name: 'recorder'",
    );
}

#[test]
fn rejects_30_http_mcp_url_host_not_in_net_atoms() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}", "net:mcp.internal.example"]
[tools]
allow = ["read"]
[[mcp_servers]]
name = "search"
transport = "http"
url = "https://search.internal.example/mcp"
capabilities = ["net:mcp.internal.example"]
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_MCP_URL_HOST at default.toml:mcp_servers[0].url: 'search.internal.example' not covered",
    );
}

#[test]
fn rejects_31_referenced_file_missing() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[agent.role_prompt]
file = "missing.md"
[tools]
allow = ["read"]
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_FILE_NOT_FOUND at default.toml:agent.role_prompt.file: 'missing.md'",
    );
}

#[test]
fn rejects_32_bundle_nests_a_bundle() {
    let t = Tree::new();
    t.write(
        "bundles.toml",
        r#"schema_version = 1
[install]
solver = "/opt/inhouse/solver"
[bundles.solver]
atoms = ["fs.ro:${install:solver}", "proc:sbatch"]
[bundles.all]
atoms = ["solver", "fs.rw:${workdir}"]
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_MALFORMED_ATOM at bundles.toml:bundles.all.atoms[0]: 'solver' (bundles may not nest)",
    );
}

#[test]
fn rejects_33_notebook_path_not_writable() {
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
[notebook]
path = "${home}/notes.md"
"#,
    );
    assert_first_exact(
        t.resolve("default"),
        "E_CAP_EXCEEDS_GRANTS at default.toml:notebook.path: requires 'fs.rw:${home}/notes.md'",
    );
}

// ---- warnings --------------------------------------------------------------------------------

#[test]
fn warns_agents_md_missing() {
    let t = Tree::new();
    let r = t.resolve("default").unwrap();
    assert!(warning_codes(&r).contains(&"W_AGENTS_MD_MISSING"));
    let w = r
        .warnings
        .iter()
        .find(|w| w.code == "W_AGENTS_MD_MISSING")
        .unwrap();
    assert!(
        w.to_string()
            .starts_with("W_AGENTS_MD_MISSING at agent.agents_md"),
        "{w}"
    );
    t.write_workdir("AGENTS.md", "Follow the style guide.\n");
    let r = t.resolve("default").unwrap();
    assert!(!warning_codes(&r).contains(&"W_AGENTS_MD_MISSING"));
}

#[test]
fn warns_net_masked() {
    let t = Tree::new();
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}", "net:*"]
[tools]
allow = ["read"]
"#,
    );
    let r = t.resolve("default").unwrap();
    let w = r
        .warnings
        .iter()
        .find(|w| w.code == "W_NET_MASKED")
        .unwrap();
    assert!(w.to_string().starts_with("W_NET_MASKED at "), "{w}");
    assert!(!r.kernel_inputs.envelope_policy.net.enabled);
    assert!(r.resolved_profile.grants.contains(&"net:*".to_owned()));
    // With the master switch on, the atom decides.
    t.write(
        "agents/default.toml",
        r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}", "net:*"]
[tools]
allow = ["read"]
[sandbox]
network = true
"#,
    );
    let r = t.resolve("default").unwrap();
    assert!(!warning_codes(&r).contains(&"W_NET_MASKED"));
    assert!(r.kernel_inputs.envelope_policy.net.enabled);
}

#[test]
fn warns_skill_path_missing() {
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
[skills]
paths = ["${workdir}/.grist/skills", "skills"]
"#,
    );
    let r = t.resolve("default").unwrap();
    let paths: Vec<String> = r
        .warnings
        .iter()
        .filter(|w| w.code == "W_SKILL_PATH_MISSING")
        .map(|w| w.toml_path.clone())
        .collect();
    assert_eq!(paths, ["skills.paths[0]", "skills.paths[1]"]);
    assert_eq!(
        r.kernel_inputs.skills_paths[1],
        t.profiles.join("agents").join("skills")
    );
    std::fs::create_dir_all(t.workdir.join(".grist/skills")).unwrap();
    let r = t.resolve("default").unwrap();
    let paths: Vec<String> = r
        .warnings
        .iter()
        .filter(|w| w.code == "W_SKILL_PATH_MISSING")
        .map(|w| w.toml_path.clone())
        .collect();
    assert_eq!(paths, ["skills.paths[1]"]);
}

#[test]
fn warns_profile_drift() {
    let t = Tree::new();
    let zero = "b3:0000000000000000000000000000000000000000000000000000000000000000";
    t.write(
        "models/stand-in.toml",
        &format!("{MODEL_HEAD}[fine_tune]\ntrained_against_profile_hash = \"{zero}\"\n"),
    );
    let r = t.resolve("default").unwrap();
    let w = r
        .warnings
        .iter()
        .find(|w| w.code == "W_PROFILE_DRIFT")
        .unwrap();
    assert!(
        w.to_string()
            .starts_with("W_PROFILE_DRIFT at stand-in.toml:fine_tune.trained_against_profile_hash"),
        "{w}"
    );
    let drift = r.drift.clone().unwrap();
    assert_eq!(drift.expected, Hash::parse(zero).unwrap());
    assert_eq!(drift.actual, r.resolved_profile_hash);
    assert!(drift.warn);
    // Pinning the real hash silences it, and `fine_tune` is outside the hash.
    let actual = r.resolved_profile_hash.clone();
    t.write(
        "models/stand-in.toml",
        &format!("{MODEL_HEAD}[fine_tune]\ntrained_against_profile_hash = \"{actual}\"\n"),
    );
    let r = t.resolve("default").unwrap();
    assert!(r.drift.is_none());
    assert_eq!(r.resolved_profile_hash, actual);
    // `warn_on_drift = false`: recorded, not warned.
    t.write(
        "models/stand-in.toml",
        &format!("{MODEL_HEAD}[fine_tune]\ntrained_against_profile_hash = \"{zero}\"\nwarn_on_drift = false\n"),
    );
    let r = t.resolve("default").unwrap();
    assert!(!warning_codes(&r).contains(&"W_PROFILE_DRIFT"));
    assert!(!r.drift.unwrap().warn);
}
