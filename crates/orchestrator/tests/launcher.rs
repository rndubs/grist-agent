//! The launcher path the P1.9 daemon will run for every session, exercised end to end on the
//! shipped `profiles/` tree: resolve the `default` catalog entry against the real base tools'
//! declarations, turn `profiles::KernelInputs` into a `kernel::KernelConfig` + `SessionInit`,
//! create the kernel, and run a turn. This is what proves the in-repo default agent, the
//! validator, and the tools agree on names and capabilities.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ::host::{MapSecretSource, NativeHost};
use ::profiles::{KernelInputs, Registry, ResolveInputs, ToolDecl, resolve};
use ::sandbox::{NoneBackend, base_tools, tool_decls};
use async_trait::async_trait;
use kernel::log::FileEventLog;
use kernel::*;
use serde_json::json;

fn profiles_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../profiles")
        .canonicalize()
        .unwrap()
}

/// The registry the real launcher builds from the compiled tools and middleware of this build.
fn registry(workdir: &Path) -> Registry {
    Registry {
        tools: tool_decls(workdir)
            .into_iter()
            .map(|(name, kind, capabilities)| (name, ToolDecl { kind, capabilities }))
            .collect(),
        middleware: BTreeSet::new(),
        parsers: BTreeSet::new(),
        memory_modules: BTreeSet::from(["none".to_owned()]),
        sandbox_backends: BTreeSet::from(["bwrap".to_owned(), "none".to_owned()]),
        dev_build: true,
    }
}

struct OneShot(Mutex<Vec<ModelResponse>>);

#[async_trait]
impl Provider for OneShot {
    fn name(&self) -> &str {
        "scripted"
    }
    async fn complete(&self, _req: ModelRequest) -> Result<ModelResponse, ProviderError> {
        Ok(self.0.lock().unwrap().remove(0))
    }
}

fn response(content: Vec<ContentBlock>, stop: StopReason) -> ModelResponse {
    let raw = serde_json::to_vec(&content).unwrap();
    ModelResponse {
        content,
        stop_reason: stop,
        usage: Usage::default(),
        model_id: "stand-in/default".into(),
        raw_response_hash: Hash::of_bytes(&raw),
        response_id: None,
    }
}

/// What the P1.9 launcher does with `KernelInputs`. Kept here as the reference shape.
fn kernel_config_from(
    inputs: &KernelInputs,
    workdir: &Path,
    log: Arc<dyn EventLog>,
    redactor: Arc<Redactor>,
    provider: Arc<dyn Provider>,
) -> (KernelConfig, Arc<NoneBackend>) {
    let host = Arc::new(NativeHost::new(
        redactor.clone(),
        Arc::new(MapSecretSource::empty()),
    ));
    // The profile selects `bwrap`; this CI host has no bwrap, so the launcher substitutes the
    // development backend (D14). A production launcher refuses the substitution.
    let sandbox = Arc::new(NoneBackend::new(host.clone()));
    let all = base_tools(workdir, sandbox.clone());
    let tools: Vec<Arc<dyn Tool>> = all
        .into_iter()
        .filter(|t| inputs.tools.iter().any(|n| n == t.name()))
        .collect();
    assert_eq!(tools.len(), inputs.tools.len(), "every allowed tool exists");
    let middleware = inputs
        .middleware
        .iter()
        .filter(|m| m.name != "recorder")
        .map(|m| panic!("no compiled middleware named {}", m.name))
        .collect();
    let config = KernelConfig {
        tools,
        middleware,
        provider,
        host,
        artifact_store: Arc::new(NoopArtifactStore),
        memory: Arc::new(NoopMemory),
        sandbox: sandbox.clone(),
        event_log: log,
        redactor,
        spill: inputs.spill.clone(),
        retry: RetryPolicy::default(),
        sandbox_limits: inputs.sandbox_limits.clone(),
        migrations: MigrationRegistry::new(),
        model_id: inputs.model_id.clone(),
        model_params: inputs.model_params.clone(),
        system_prompt: inputs.system_prompt.clone(),
        grants: inputs.grants.clone(),
        delta_sink: None,
        event_channel_capacity: 256,
    };
    (config, sandbox)
}

#[tokio::test]
async fn shipped_default_agent_resolves_against_the_real_tools_and_runs() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path().join("repo");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(workdir.join("AGENTS.md"), "Keep functions short.\n").unwrap();
    std::fs::create_dir_all(workdir.join(".grist/skills")).unwrap();
    std::fs::write(workdir.join("hello.txt"), "hello from the checkout\n").unwrap();
    let workdir = workdir.canonicalize().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    // ---- resolve the shipped profile (what the launcher does first) ----
    let reg = registry(&workdir);
    let overrides = toml::Table::new();
    let resolved = resolve(&ResolveInputs {
        profiles_dir: &profiles_dir(),
        workdir: &workdir,
        home: &home,
        agent: "default",
        runtime_overrides: &overrides,
        registry: &reg,
        resume: false,
    })
    .unwrap_or_else(|diags| panic!("{diags:?}"));
    let inputs = &resolved.kernel_inputs;
    assert_eq!(inputs.model_id, "stand-in/default");
    assert_eq!(inputs.endpoint, "litellm-ci");
    assert_eq!(inputs.sandbox_backend, "bwrap");
    assert_eq!(
        inputs.tools,
        ["bash", "edit", "python", "read", "run_script", "write"]
    );
    let grants: Vec<String> = inputs.grants.iter().map(|c| c.to_string()).collect();
    assert!(grants.contains(&format!("fs.rw:{}", workdir.display())));
    assert!(grants.contains(&"proc:bash".to_owned()));
    assert!(grants.contains(&"tool:run_script".to_owned()));
    assert!(!inputs.envelope_policy.net.enabled, "no network by default");
    // Three prompt blocks in D7 order: model, role, AGENTS.md.
    let kinds: Vec<PromptBlockKind> = inputs.system_prompt.iter().map(|b| b.kind).collect();
    assert_eq!(
        kinds,
        [
            PromptBlockKind::Model,
            PromptBlockKind::Role,
            PromptBlockKind::AgentsMd
        ]
    );
    assert!(
        inputs.system_prompt[2]
            .text
            .contains("Keep functions short.")
    );
    assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
    assert_eq!(
        resolved.profile_loads.len(),
        3,
        "bundles, model, agent; no project override file"
    );

    // ---- assemble and run the kernel ----
    let redactor = Arc::new(Redactor::new());
    let session_id = SessionId("s_launch".into());
    let log = Arc::new(
        FileEventLog::create(
            &dir.path().join("s.jsonl"),
            session_id.clone(),
            redactor.clone(),
        )
        .await
        .unwrap(),
    );
    let provider = Arc::new(OneShot(Mutex::new(vec![
        response(
            vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "read".into(),
                input: json!({"path": "hello.txt"}),
            }],
            StopReason::ToolUse,
        ),
        response(
            vec![ContentBlock::Text {
                text: "It says hello.".into(),
            }],
            StopReason::EndTurn,
        ),
    ])));
    let (config, _sandbox) = kernel_config_from(inputs, &workdir, log.clone(), redactor, provider);
    let init = SessionInit {
        session_id,
        profiles: resolved.active_profiles.clone(),
        profile_loads: resolved.profile_loads.clone(),
        runtime_overrides: None,
        notebook_path: inputs.notebook_path.clone(),
        memory: None,
    };
    let mut k = Kernel::create(config, init).await.unwrap();
    k.handle()
        .enqueue_user_message(Message::user_text("What does hello.txt say?"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);

    // The kernel recorded exactly what profiles resolved.
    let events = log.events();
    let created = events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::SessionCreated(p) => Some(p),
            _ => None,
        })
        .unwrap();
    assert_eq!(created.profiles, resolved.active_profiles);
    assert_eq!(created.tools, inputs.tools);
    assert_eq!(created.grants, inputs.grants);
    assert_eq!(created.model_id, inputs.model_id);
    assert_eq!(created.sandbox_limits, inputs.sandbox_limits);
    assert_eq!(created.spill, inputs.spill);
    assert_eq!(
        created.sandbox_policy_hash, inputs.sandbox_policy_hash,
        "the kernel derives the same envelope as profiles"
    );
    assert_eq!(k.state().sandbox_policy_hash, inputs.sandbox_policy_hash);
    assert_eq!(k.state().notebook_path, inputs.notebook_path);
    let loads = events
        .iter()
        .filter(|e| matches!(e.body, EventBody::ProfileLoad(_)))
        .count();
    assert_eq!(loads, resolved.profile_loads.len());
    // The read tool really read the checkout under the profile's grants.
    let result = events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ToolResult(p) => Some(p),
            _ => None,
        })
        .unwrap();
    assert!(!result.is_error);
    let ToolResultContent::Json(v) = &result.content else {
        panic!()
    };
    assert!(
        v["content"]
            .as_str()
            .unwrap()
            .contains("hello from the checkout")
    );
    let req = events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ModelRequest(p) => Some(p),
            _ => None,
        })
        .unwrap();
    assert_eq!(req.profiles, resolved.active_profiles);
    assert_eq!(req.prompt_blocks.len(), 3);
    assert_eq!(k.state().turn, 2);
    assert_eq!(
        inputs.tool_descriptions.get("bash").map(String::as_str),
        Some(
            "Run one shell command in a fresh sandbox. Shell state does not persist between calls; the filesystem does."
        ),
        "the model profile's tool-description override reaches the launcher"
    );
}
