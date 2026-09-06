//! Resolves the shipped `profiles/` tree (§10.2) end to end with the P1.7 registry.

mod common;

use std::path::{Path, PathBuf};

use common::*;
use kernel::{Capability, FsMode, MiddlewareSource, PromptBlockKind, RECORDER_PRIORITY};
use profiles::{ResolveInputs, discover_profiles_dir, resolve};
use tempfile::TempDir;

fn shipped_dir() -> PathBuf {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    discover_profiles_dir(here).expect("profiles/catalog.toml above the crate")
}

fn run(workdir: &Path, home: &Path, resume: bool) -> profiles::Resolved {
    resolve(&ResolveInputs {
        profiles_dir: &shipped_dir(),
        workdir,
        home,
        agent: "default",
        runtime_overrides: &toml::Table::new(),
        registry: &registry_for(workdir),
        resume,
    })
    .unwrap_or_else(|d| panic!("{:#?}", d))
}

fn cap(s: &str) -> Capability {
    s.parse().unwrap()
}

#[test]
fn discover_profiles_dir_walks_up() {
    let d = shipped_dir();
    assert!(d.join("catalog.toml").is_file());
    assert!(d.join("bundles.toml").is_file());
    assert!(d.join("models/stand-in.toml").is_file());
    assert!(d.join("agents/default.toml").is_file());
    assert!(d.join("agents/default.role.md").is_file());
    let tmp = TempDir::new().unwrap();
    assert_eq!(discover_profiles_dir(tmp.path()), None);
}

#[test]
fn shipped_default_agent_resolves() {
    let tmp = TempDir::new().unwrap();
    let workdir = tmp.path().join("repo");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let r = run(&workdir, &home, false);
    let w = workdir.to_string_lossy();
    let k = &r.kernel_inputs;

    // Grants: the three declared atoms plus the seven derived tool atoms, sorted.
    let expected: Vec<Capability> = [
        format!("fs.rw:{w}"),
        "proc:bash".into(),
        "proc:python3".into(),
        "tool:ask_user".into(),
        "tool:bash".into(),
        "tool:edit".into(),
        "tool:python".into(),
        "tool:read".into(),
        "tool:run_script".into(),
        "tool:write".into(),
    ]
    .iter()
    .map(|s| cap(s))
    .collect::<std::collections::BTreeSet<_>>()
    .into_iter()
    .collect();
    assert_eq!(k.grants, expected);
    assert_eq!(
        r.resolved_profile.grants,
        [
            "fs.rw:${workdir}",
            "proc:bash",
            "proc:python3",
            "tool:ask_user",
            "tool:bash",
            "tool:edit",
            "tool:python",
            "tool:read",
            "tool:run_script",
            "tool:write"
        ]
    );
    assert_eq!(
        k.tools,
        [
            "ask_user",
            "bash",
            "edit",
            "python",
            "read",
            "run_script",
            "write"
        ]
    );

    // Chain: only the kernel's recorder.
    assert_eq!(k.middleware.len(), 1);
    assert_eq!(k.middleware[0].name, "recorder");
    assert_eq!(k.middleware[0].priority, RECORDER_PRIORITY);
    assert_eq!(k.middleware[0].source, MiddlewareSource::Kernel);
    assert_eq!(k.middleware[0].config_hash, None);

    // No network; the envelope mounts the workdir rw.
    assert!(!k.envelope_policy.net.enabled);
    assert!(!k.sandbox_limits.network);
    assert_eq!(k.envelope_policy.mounts.len(), 1);
    assert_eq!(k.envelope_policy.mounts[0].path, workdir);
    assert_eq!(k.envelope_policy.mounts[0].mode, FsMode::Rw);
    assert_eq!(
        k.envelope_policy.programs,
        ["bash", "python3"].into_iter().map(str::to_owned).collect()
    );
    assert_eq!(k.sandbox_backend, "bwrap");
    assert_eq!(k.sandbox_limits.timeout.as_secs(), 600);
    assert_eq!(k.sandbox_limits.scratch_tmpfs_mb, 256);
    assert_eq!(
        k.sandbox_limits.env_allowlist,
        ["PATH", "HOME", "LANG", "LC_ALL", "TERM", "TZ"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );

    // Model side.
    assert_eq!(k.model_id, "stand-in/default");
    assert_eq!(k.endpoint, "litellm-ci");
    assert_eq!(k.tool_format, "native");
    assert_eq!(k.context_length, 32768);
    assert_eq!(k.model_params.temperature, Some(0.0));
    assert_eq!(k.model_params.max_tokens, Some(2048));
    let thinking = k.model_params.thinking.clone().unwrap();
    assert!(!thinking.enabled);
    assert_eq!(thinking.budget_tokens, None);
    assert_eq!(k.quirks.auth, "bearer:LITELLM_CI_API_KEY");
    assert!(k.quirks.supports_stream_usage);
    assert_eq!(k.compaction.trigger_at, 0.80);
    assert_eq!(k.compaction.target, 0.45);
    assert_eq!(k.tool_descriptions.len(), 1);
    assert!(k.tool_descriptions["bash"].starts_with("Run one shell command"));

    // Agent side.
    assert_eq!(k.context_budget_tokens, 40000);
    assert_eq!(k.spill.cap_bytes, 16384);
    assert_eq!(k.notebook_path, Some(workdir.join(".grist/notebook.md")));
    assert_eq!(k.agents_md_path, workdir.join("AGENTS.md"));
    assert_eq!(k.skills_paths, [workdir.join(".grist/skills")]);
    assert_eq!(k.memory.module, "none");
    assert_eq!(k.catalog_entry, "default");

    // System prompt: model block + role block; AGENTS.md absent → warning, no block.
    let kinds: Vec<PromptBlockKind> = k.system_prompt.iter().map(|b| b.kind).collect();
    assert_eq!(kinds, [PromptBlockKind::Model, PromptBlockKind::Role]);
    assert!(
        k.system_prompt[0]
            .text
            .starts_with("You are a coding and scripting assistant")
    );
    assert!(
        k.system_prompt[1]
            .text
            .starts_with("You work in the repository mounted")
    );
    assert!(!k.system_prompt[1].text.ends_with('\n'));
    assert!(warning_codes(&r).contains(&"W_AGENTS_MD_MISSING"));
    assert!(warning_codes(&r).contains(&"W_SKILL_PATH_MISSING"));

    // Events and state.
    let kinds: Vec<_> = r.profile_loads.iter().map(|p| p.kind).collect();
    assert_eq!(
        kinds,
        [
            kernel::ProfileKind::Bundles,
            kernel::ProfileKind::Model,
            kernel::ProfileKind::Agent
        ]
    );
    assert!(r.profile_loads.iter().all(|p| !p.rejected && p.turn == 0));
    assert_eq!(
        r.profile_loads[1].hash,
        kernel::Hash::of_bytes(&std::fs::read(shipped_dir().join("models/stand-in.toml")).unwrap())
    );
    assert_eq!(
        r.active_profiles.model_profile_hash,
        r.profile_loads[1].hash
    );
    assert_eq!(
        r.active_profiles.agent_profile_hash,
        r.profile_loads[2].hash
    );
    assert_eq!(
        r.active_profiles.bundles_hash,
        Some(r.profile_loads[0].hash.clone())
    );
    assert_eq!(r.active_profiles.project_profile_hash, None);
    assert_eq!(
        r.active_profiles.resolved_profile_hash,
        r.resolved_profile_hash
    );
    assert_eq!(
        r.resolved_profile_hash,
        kernel::Hash::of_canonical_json(&r.resolved_profile).unwrap()
    );
    assert_eq!(
        r.kernel_inputs.sandbox_policy_hash,
        r.kernel_inputs.envelope_policy.hash().unwrap()
    );
    assert!(r.drift.is_none());
}

#[test]
fn shipped_with_agents_md_adds_the_headed_block() {
    let tmp = TempDir::new().unwrap();
    let workdir = tmp.path().join("repo");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(workdir.join("AGENTS.md"), "\nUse rustfmt.\n\n").unwrap();
    let r = run(&workdir, &home, false);
    let k = &r.kernel_inputs;
    let kinds: Vec<PromptBlockKind> = k.system_prompt.iter().map(|b| b.kind).collect();
    assert_eq!(
        kinds,
        [
            PromptBlockKind::Model,
            PromptBlockKind::Role,
            PromptBlockKind::AgentsMd
        ]
    );
    let b = &k.system_prompt[2];
    assert_eq!(b.text, "# Project instructions (AGENTS.md)\n\nUse rustfmt.");
    assert_eq!(b.hash, kernel::Hash::of_bytes(b.text.as_bytes()));
    assert_eq!(b.name, workdir.join("AGENTS.md").to_string_lossy());
    assert!(!warning_codes(&r).contains(&"W_AGENTS_MD_MISSING"));
    // The notebook block appears only on resume, when the file exists.
    std::fs::create_dir_all(workdir.join(".grist")).unwrap();
    std::fs::write(workdir.join(".grist/notebook.md"), "step 1 done\n").unwrap();
    let r = run(&workdir, &home, false);
    assert_eq!(r.kernel_inputs.system_prompt.len(), 3);
    let r = run(&workdir, &home, true);
    let b = r.kernel_inputs.system_prompt.last().unwrap();
    assert_eq!(b.kind, PromptBlockKind::Notebook);
    assert_eq!(b.text, "# Notebook\n\nstep 1 done");
}

#[test]
fn resolved_profile_hash_is_portable_while_sandbox_hash_is_not() {
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    let home = a.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let wa = a.path().join("repo-a");
    let wb = b.path().join("repo-b");
    std::fs::create_dir_all(&wa).unwrap();
    std::fs::create_dir_all(&wb).unwrap();
    let ra = run(&wa, &home, false);
    let rb = run(&wb, &home, false);
    assert_eq!(ra.resolved_profile_hash, rb.resolved_profile_hash);
    assert_eq!(ra.resolved_profile, rb.resolved_profile);
    assert_ne!(
        ra.kernel_inputs.sandbox_policy_hash,
        rb.kernel_inputs.sandbox_policy_hash
    );
    assert_ne!(ra.kernel_inputs.grants, rb.kernel_inputs.grants);
}
