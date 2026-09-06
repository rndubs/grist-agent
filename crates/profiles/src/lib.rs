//! `profiles` — loading, merging, and validating model profiles, agent profiles, and project
//! overrides; the catalog; the capability bundles file (`docs/specs/profile-schema.md`, D6, D7).
//!
//! Entry points: [`resolve`] runs the §7 algorithm; [`Catalog`] and [`Bundles`] load the two
//! supporting files; [`kernel_defaults`] is layer 0; [`discover_profiles_dir`] finds
//! `profiles/catalog.toml` by walking up from a start directory.

pub mod atom;
pub mod bundles;
pub mod catalog;
pub mod diagnostic;
pub mod merge;
pub mod prompt;
pub mod resolve;
pub mod resolved;
pub mod schema;

pub use bundles::{Bundle, Bundles};
pub use catalog::{Catalog, CatalogEntry, discover_profiles_dir};
pub use diagnostic::Diagnostic;
pub use resolve::{
    KernelInputs, ProfileDrift, Registry, ResolveInputs, Resolved, ResolvedEvalSet,
    ResolvedMcpServer, ResolvedMiddleware, ResolvedSubagent, ToolDecl, resolve,
};
pub use resolved::ResolvedProfile;
pub use schema::{FileKind, SCHEMA_VERSION, ValidatedFile, validate_file};

/// Layer 0 (`profile-schema.md` §1.4) as TOML text, verbatim.
pub const KERNEL_DEFAULTS_TOML: &str = r#"schema_version = 1

[model]
# id              (required, string)   full endpoint model string, opaque to the kernel
# endpoint        (required, string)   endpoint name, resolved by the launcher (§2.1)
# context_length  (required, integer)  tokens
tool_format = "native"
max_output_tokens = 4096
# temperature: absent means "do not send the parameter; the server decides"

[model.thinking]
enabled = false
budget_tokens = 0

[model.quirks]
reasoning_field = "none"
supports_structured_output = false
supports_stream_usage = false
strict_tool_schema = false
auth = "none"

[compaction]
trigger_at = 0.85
target = 0.50

[agent]
# name        (required, string)
context_budget_tokens = 40000       # D16 placeholder, to be tuned
spill_cap_bytes = 16384             # D12: spill is a kernel behaviour; only the cap is a value
agents_md = "${workdir}/AGENTS.md"

[capabilities]
grants = []

[tools]
allow = []

[skills]
paths = []

[memory]
module = "none"

[sandbox]
backend = "bwrap"
timeout_s = 600
scratch_tmpfs_mb = 256
env_allow = ["PATH", "HOME", "LANG", "LC_ALL", "TERM", "TZ"]
network = false

[notebook]
path = "${workdir}/.grist/notebook.md"
inject_on_resume = true
max_tokens = 4000
"#;

/// Layer 0: the kernel defaults of `profile-schema.md` §1.4 as a parsed table.
pub fn kernel_defaults() -> toml::Table {
    KERNEL_DEFAULTS_TOML
        .parse()
        .expect("KERNEL_DEFAULTS_TOML is valid TOML")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_defaults_match_section_1_4() {
        let t = kernel_defaults();
        assert_eq!(t["schema_version"].as_integer(), Some(1));
        assert_eq!(t["model"]["tool_format"].as_str(), Some("native"));
        assert_eq!(t["model"]["max_output_tokens"].as_integer(), Some(4096));
        assert!(t["model"].get("temperature").is_none());
        assert!(t["model"].get("id").is_none());
        assert_eq!(t["model"]["thinking"]["enabled"].as_bool(), Some(false));
        assert_eq!(
            t["model"]["thinking"]["budget_tokens"].as_integer(),
            Some(0)
        );
        assert_eq!(
            t["model"]["quirks"]["reasoning_field"].as_str(),
            Some("none")
        );
        assert_eq!(t["model"]["quirks"]["auth"].as_str(), Some("none"));
        assert_eq!(t["compaction"]["trigger_at"].as_float(), Some(0.85));
        assert_eq!(t["compaction"]["target"].as_float(), Some(0.50));
        assert_eq!(
            t["agent"]["context_budget_tokens"].as_integer(),
            Some(40000)
        );
        assert_eq!(t["agent"]["spill_cap_bytes"].as_integer(), Some(16384));
        assert_eq!(
            t["agent"]["agents_md"].as_str(),
            Some("${workdir}/AGENTS.md")
        );
        assert!(t["agent"].get("name").is_none());
        assert_eq!(t["capabilities"]["grants"].as_array().unwrap().len(), 0);
        assert_eq!(t["tools"]["allow"].as_array().unwrap().len(), 0);
        assert_eq!(t["skills"]["paths"].as_array().unwrap().len(), 0);
        assert_eq!(t["memory"]["module"].as_str(), Some("none"));
        assert_eq!(t["sandbox"]["backend"].as_str(), Some("bwrap"));
        assert_eq!(t["sandbox"]["timeout_s"].as_integer(), Some(600));
        assert_eq!(t["sandbox"]["scratch_tmpfs_mb"].as_integer(), Some(256));
        let env: Vec<&str> = t["sandbox"]["env_allow"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(env, ["PATH", "HOME", "LANG", "LC_ALL", "TERM", "TZ"]);
        assert_eq!(t["sandbox"]["network"].as_bool(), Some(false));
        assert_eq!(
            t["notebook"]["path"].as_str(),
            Some("${workdir}/.grist/notebook.md")
        );
        assert_eq!(t["notebook"]["inject_on_resume"].as_bool(), Some(true));
        assert_eq!(t["notebook"]["max_tokens"].as_integer(), Some(4000));
        // Layer 0 validates as an agent-shaped file except for the model keys it omits.
        assert_eq!(t.len(), 10);
    }
}
