//! The launcher (P1.9, ADR-0004 "What P1.9 implements"): resolve a catalog entry against the
//! compiled tool registry, turn `profiles::KernelInputs` into a `KernelConfig` + `SessionInit`,
//! and create or reopen the kernel. Every protocol server (`grist-kernel`, and through it
//! `grist-daemon`) goes through here; `crates/orchestrator/tests/launcher.rs` is the reference
//! shape this module grew from.
//!
//! What the launcher decides, and what it refuses:
//! - **Sandbox backend** from the profile's `sandbox_backend`: `bwrap` → [`sandbox::BwrapBackend`];
//!   `none` → [`sandbox::NoneBackend`] only when this crate is built with `dev-sandbox-none`
//!   (D14). Any other name, or `none` in a release build, is [`LaunchError::Backend`].
//!   `GRIST_SANDBOX_BACKEND` overrides the profile's choice for development
//!   ([`LaunchOptions::sandbox_override`]) and is subject to the same rule.
//! - **Endpoint URL** from `GRIST_ENDPOINT_<NAME>_URL` via [`host::endpoint_url_from_env`]; a
//!   missing variable is a fatal start error.
//! - **Provider** is the OpenAI-compatible client with the model profile's quirks; the bearer
//!   secret, if any, is a handle resolved by the host at request time (D10).
//! - **Tool descriptions** from the model profile are applied by wrapping the compiled tools'
//!   definitions ([`Described`]); nothing else about a tool changes.
//! - **`ask_user`** (D17) is the host's tool; it is registered when the agent profile allows it and
//!   its questions arrive on the [`PendingQuestion`] receiver the caller gets back.
//!
//! Session bookkeeping lives under [`LaunchOptions::state_dir`]: `sessions/<id>.jsonl` is the
//! event log and `sessions/<id>.toml` the [`SessionRecord`] (workdir, agent, overrides) that a
//! later `session/resume` needs to rebuild the same configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use host::{AskUserTool, ChannelPrompter, EnvSecretSource, NativeHost, PendingQuestion};
use kernel::log::FileEventLog;
use kernel::{
    Capability, EventLog, Host, Kernel, KernelConfig, KernelError, Message, MigrationRegistry,
    ModelDelta, NetAllow, NetPolicy, NoopArtifactStore, NoopMemory, Provider, Redactor,
    ResumeCause, RetryPolicy, SandboxBackend, SessionId, SessionInit, Tool, ToolContext,
    ToolDefinition, ToolError, ToolKind, ToolResult,
};
use profiles::{KernelInputs, Registry, ResolveInputs, Resolved, ToolDecl, resolve};
use providers::{EndpointConfig, OpenAiCompatProvider, Quirks, QuirksProfile};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

/// Where profiles and session state live, and what the launcher may substitute.
#[derive(Clone, Debug)]
pub struct LaunchOptions {
    /// The `profiles/` tree (`catalog.toml`, `bundles.toml`, `models/`, `agents/`).
    pub profiles_dir: PathBuf,
    /// Session logs and records: `<state_dir>/sessions/<id>.jsonl|.toml`.
    pub state_dir: PathBuf,
    /// `${home}` for profile placeholders.
    pub home: PathBuf,
    /// Development override of the profile's `sandbox_backend` (`GRIST_SANDBOX_BACKEND`).
    pub sandbox_override: Option<String>,
    /// Capacity of the `ask_user` question channel.
    pub question_capacity: usize,
    /// Capacity of the streaming-delta broadcast handed to the kernel as `delta_sink`.
    pub delta_capacity: usize,
}

impl LaunchOptions {
    /// Options from the environment: `GRIST_PROFILES_DIR` (else discovered upward from the
    /// current directory, else `<state_dir>/profiles`), `GRIST_STATE_DIR` (else `$HOME/.grist`),
    /// `GRIST_SANDBOX_BACKEND`.
    pub fn from_env() -> Result<LaunchOptions, LaunchError> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| LaunchError::Config("HOME is not set".into()))?;
        let state_dir = std::env::var_os("GRIST_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".grist"));
        let profiles_dir = match std::env::var_os("GRIST_PROFILES_DIR") {
            Some(p) => PathBuf::from(p),
            None => std::env::current_dir()
                .ok()
                .and_then(|cwd| profiles::discover_profiles_dir(&cwd))
                .unwrap_or_else(|| state_dir.join("profiles")),
        };
        Ok(LaunchOptions {
            profiles_dir,
            state_dir,
            home,
            sandbox_override: std::env::var("GRIST_SANDBOX_BACKEND")
                .ok()
                .filter(|s| !s.is_empty()),
            question_capacity: 8,
            delta_capacity: 1024,
        })
    }

    /// `<state_dir>/sessions`.
    pub fn sessions_dir(&self) -> PathBuf {
        self.state_dir.join("sessions")
    }

    /// The event log of `id`.
    pub fn log_path(&self, id: &SessionId) -> PathBuf {
        self.sessions_dir().join(format!("{}.jsonl", id.0))
    }

    /// The [`SessionRecord`] of `id`.
    pub fn record_path(&self, id: &SessionId) -> PathBuf {
        self.sessions_dir().join(format!("{}.toml", id.0))
    }
}

/// What a client asked for in `session/new` (and what `session/resume` reads back).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Our session id (also the ACP `sessionId`).
    pub session_id: String,
    /// The checkout the session works in (ACP `cwd`).
    pub workdir: PathBuf,
    /// Catalog entry name.
    pub agent: String,
    /// Layer-4 runtime overrides (`profile-schema.md` §4.4).
    #[serde(default)]
    pub overrides: toml::Table,
    /// RFC 3339 creation time.
    pub created_at: String,
}

impl SessionRecord {
    /// Read `<state_dir>/sessions/<id>.toml`.
    pub async fn load(opts: &LaunchOptions, id: &SessionId) -> Result<SessionRecord, LaunchError> {
        let path = opts.record_path(id);
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| LaunchError::UnknownSession(format!("{}: {e}", path.display())))?;
        toml::from_str(&text).map_err(|e| LaunchError::Config(format!("{}: {e}", path.display())))
    }

    /// Every record under `<state_dir>/sessions`, oldest first.
    pub async fn list(opts: &LaunchOptions) -> Result<Vec<SessionRecord>, LaunchError> {
        let dir = opts.sessions_dir();
        let mut out = Vec::new();
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(LaunchError::Io(format!("{}: {e}", dir.display()))),
        };
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| LaunchError::Io(e.to_string()))?
        {
            let path = entry.path();
            if path.extension().is_some_and(|x| x == "toml") {
                let text = tokio::fs::read_to_string(&path)
                    .await
                    .map_err(|e| LaunchError::Io(format!("{}: {e}", path.display())))?;
                if let Ok(rec) = toml::from_str::<SessionRecord>(&text) {
                    out.push(rec);
                }
            }
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }
}

/// A launched session: the kernel plus the channels the protocol server drives it with.
pub struct Launched {
    /// The kernel (owned by exactly one driver; see `acp::session`).
    pub kernel: Kernel,
    /// `ask_user` questions (D17) to answer through the client.
    pub questions: mpsc::Receiver<PendingQuestion>,
    /// Streaming deltas (`KernelConfig.delta_sink`).
    pub deltas: broadcast::Receiver<ModelDelta>,
    /// The resolved inputs, for diagnostics and `session_created` cross-checks.
    pub inputs: KernelInputs,
    /// Resolver warnings, one line each.
    pub warnings: Vec<String>,
    /// The log path.
    pub log_path: PathBuf,
}

/// Why a session could not be launched.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    /// Profile resolution failed; one line per diagnostic.
    #[error("profile resolution failed:\n{}", .0.join("\n"))]
    Profile(Vec<String>),
    /// The profile (or the override) names a backend this build cannot provide.
    #[error("sandbox backend `{0}` is not available in this build (D14)")]
    Backend(String),
    /// Endpoint URL, quirks, or another configuration problem.
    #[error("configuration: {0}")]
    Config(String),
    /// No such session record.
    #[error("unknown session: {0}")]
    UnknownSession(String),
    /// Filesystem.
    #[error("io: {0}")]
    Io(String),
    /// The kernel refused the configuration or the log.
    #[error(transparent)]
    Kernel(#[from] KernelError),
}

/// The `ask_user` tool's declaration for the registry (D17: capabilities `[]`).
fn ask_user_decl() -> (String, ToolDecl) {
    let t = AskUserTool::new();
    (
        t.name().to_owned(),
        ToolDecl {
            kind: t.kind(),
            capabilities: t.capabilities(),
        },
    )
}

/// The registry of what this build compiled in: the six base tools plus `ask_user`, no middleware
/// or parsers yet, the `none` memory module, and the backends this build can provide.
pub fn registry(workdir: &Path) -> Registry {
    let mut tools: BTreeMap<String, ToolDecl> = sandbox::tool_decls(workdir)
        .into_iter()
        .map(|(name, kind, capabilities)| (name, ToolDecl { kind, capabilities }))
        .collect();
    let (name, decl) = ask_user_decl();
    tools.insert(name, decl);
    let mut backends = BTreeSet::from(["bwrap".to_owned()]);
    if cfg!(feature = "dev-sandbox-none") {
        backends.insert("none".to_owned());
    }
    Registry {
        tools,
        middleware: BTreeSet::new(),
        parsers: BTreeSet::new(),
        memory_modules: BTreeSet::from(["none".to_owned()]),
        sandbox_backends: backends,
        dev_build: cfg!(feature = "dev-sandbox-none"),
    }
}

/// A compiled tool with the model profile's description substituted (`tool_descriptions`).
pub struct Described {
    inner: Arc<dyn Tool>,
    description: String,
}

#[async_trait]
impl Tool for Described {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn schema(&self) -> Value {
        self.inner.schema()
    }
    fn kind(&self) -> ToolKind {
        self.inner.kind()
    }
    fn capabilities(&self) -> Vec<Capability> {
        self.inner.capabilities()
    }
    fn session_command(&self) -> Option<kernel::Command> {
        self.inner.session_command()
    }
    async fn invoke(&self, ctx: &ToolContext<'_>, input: Value) -> Result<ToolResult, ToolError> {
        self.inner.invoke(ctx, input).await
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_owned(),
            description: self.description.clone(),
            input_schema: self.schema(),
        }
    }
}

/// Resolve `agent` for `workdir` with `overrides` (`resume` per `ResolveInputs`).
fn resolve_profile(
    opts: &LaunchOptions,
    workdir: &Path,
    agent: &str,
    overrides: &toml::Table,
    resume: bool,
) -> Result<Resolved, LaunchError> {
    let reg = registry(workdir);
    resolve(&ResolveInputs {
        profiles_dir: &opts.profiles_dir,
        workdir,
        home: &opts.home,
        agent,
        runtime_overrides: overrides,
        registry: &reg,
        resume,
    })
    .map_err(|diags| LaunchError::Profile(diags.iter().map(|d| d.to_string()).collect()))
}

/// The host name of `url` for the provider's network allowlist.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let authority = rest.split('/').next()?;
    let host = authority.rsplit('@').next()?;
    let host = host
        .strip_prefix('[')
        .map_or(host, |h| h.split(']').next().unwrap_or(h));
    let host = host.split(':').next().unwrap_or(host);
    (!host.is_empty()).then(|| host.to_owned())
}

/// Assembled pieces shared by create and resume.
struct Assembled {
    config: KernelConfig,
    questions: mpsc::Receiver<PendingQuestion>,
    deltas: broadcast::Receiver<ModelDelta>,
}

/// `KernelConfig` from `KernelInputs` (ADR-0004 "What P1.9 implements"). `redactor` is shared
/// with the log (D10).
fn assemble(
    opts: &LaunchOptions,
    inputs: &KernelInputs,
    workdir: &Path,
    redactor: Arc<Redactor>,
    log: Arc<dyn EventLog>,
    provider_override: Option<Arc<dyn Provider>>,
) -> Result<Assembled, LaunchError> {
    let (prompter, questions) = ChannelPrompter::new(opts.question_capacity);
    let host = Arc::new(
        NativeHost::new(redactor.clone(), Arc::new(EnvSecretSource::from_env()))
            .with_prompter(Arc::new(prompter)),
    );

    // Backend (D14).
    let backend_name = opts
        .sandbox_override
        .clone()
        .unwrap_or_else(|| inputs.sandbox_backend.clone());
    let sandbox: Arc<dyn SandboxBackend> = match backend_name.as_str() {
        "bwrap" => Arc::new(sandbox::BwrapBackend::new(host.clone())),
        #[cfg(feature = "dev-sandbox-none")]
        "none" => Arc::new(sandbox::NoneBackend::new(host.clone())),
        other => return Err(LaunchError::Backend(other.to_owned())),
    };

    // Tools: the compiled set filtered by the profile, descriptions from the model profile.
    let mut all: Vec<Arc<dyn Tool>> = sandbox::base_tools(workdir, sandbox.clone());
    all.push(Arc::new(AskUserTool::new()));
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for name in &inputs.tools {
        let Some(t) = all.iter().find(|t| t.name() == name) else {
            return Err(LaunchError::Config(format!(
                "profile allows tool `{name}` but this build has no such tool"
            )));
        };
        tools.push(match inputs.tool_descriptions.get(name) {
            Some(d) => Arc::new(Described {
                inner: t.clone(),
                description: d.clone(),
            }),
            None => t.clone(),
        });
    }
    if let Some(m) = inputs.middleware.iter().find(|m| m.name != "recorder") {
        return Err(LaunchError::Config(format!(
            "profile names middleware `{}` but this build compiles none",
            m.name
        )));
    }

    // Provider: endpoint URL from the environment, quirks from the model profile.
    let provider: Arc<dyn Provider> = match provider_override {
        Some(p) => p,
        None => {
            let url = host::endpoint_url_from_env(&inputs.endpoint)
                .map_err(|e| LaunchError::Config(e.to_string()))?;
            let quirks = Quirks::from_profile(&QuirksProfile {
                reasoning_field: inputs.quirks.reasoning_field.clone(),
                supports_structured_output: inputs.quirks.supports_structured_output,
                supports_stream_usage: inputs.quirks.supports_stream_usage,
                strict_tool_schema: inputs.quirks.strict_tool_schema,
                auth: inputs.quirks.auth.clone(),
                tool_format: inputs.tool_format.clone(),
            })
            .map_err(|e| LaunchError::Config(format!("model profile quirks: {e}")))?;
            let allow = match url_host(&url) {
                Some(h) => NetAllow::Hosts(BTreeSet::from([h])),
                None => NetAllow::Any,
            };
            let net = host
                .network(&NetPolicy {
                    enabled: true,
                    allow,
                })
                .map_err(|e| LaunchError::Config(format!("provider network: {e}")))?;
            Arc::new(OpenAiCompatProvider::new(
                inputs.endpoint.clone(),
                EndpointConfig::new(url),
                quirks,
                net,
                host.clone(),
                Arc::new(NoopArtifactStore),
            ))
        }
    };

    let (delta_tx, deltas) = broadcast::channel(opts.delta_capacity.max(1));
    let config = KernelConfig {
        tools,
        middleware: Vec::new(),
        provider,
        host,
        artifact_store: Arc::new(NoopArtifactStore),
        memory: Arc::new(NoopMemory),
        sandbox,
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
        delta_sink: Some(delta_tx),
        event_channel_capacity: 1024,
    };
    Ok(Assembled {
        config,
        questions,
        deltas,
    })
}

/// `SessionInit` from a resolution.
fn session_init(id: &SessionId, resolved: &Resolved, overrides: &toml::Table) -> SessionInit {
    let inputs = &resolved.kernel_inputs;
    SessionInit {
        session_id: id.clone(),
        profiles: resolved.active_profiles.clone(),
        profile_loads: resolved.profile_loads.clone(),
        runtime_overrides: (!overrides.is_empty())
            .then(|| profiles::merge::toml_to_json(&toml::Value::Table(overrides.clone()))),
        notebook_path: inputs.notebook_path.clone(),
        memory: None,
    }
}

fn now_rfc3339() -> String {
    kernel::time::now_rfc3339_ms()
}

/// Create a new session: resolve the profile, write the record, create the log, create the kernel.
/// `provider_override` replaces the OpenAI-compatible client (tests; the replay driver).
pub async fn create_session(
    opts: &LaunchOptions,
    id: SessionId,
    workdir: &Path,
    agent: &str,
    overrides: toml::Table,
    provider_override: Option<Arc<dyn Provider>>,
) -> Result<Launched, LaunchError> {
    let workdir = workdir
        .canonicalize()
        .map_err(|e| LaunchError::Config(format!("workdir {}: {e}", workdir.display())))?;
    let resolved = resolve_profile(opts, &workdir, agent, &overrides, false)?;
    tokio::fs::create_dir_all(opts.sessions_dir())
        .await
        .map_err(|e| LaunchError::Io(e.to_string()))?;
    let record = SessionRecord {
        session_id: id.0.clone(),
        workdir: workdir.clone(),
        agent: agent.to_owned(),
        overrides: overrides.clone(),
        created_at: now_rfc3339(),
    };
    let text = toml::to_string(&record).map_err(|e| LaunchError::Config(e.to_string()))?;
    tokio::fs::write(opts.record_path(&id), text)
        .await
        .map_err(|e| LaunchError::Io(e.to_string()))?;
    let log_path = opts.log_path(&id);
    let redactor = Arc::new(Redactor::new());
    let log = Arc::new(
        FileEventLog::create(&log_path, id.clone(), redactor.clone())
            .await
            .map_err(KernelError::from)?,
    );
    let inputs = resolved.kernel_inputs.clone();
    let Assembled {
        config,
        questions,
        deltas,
    } = assemble(opts, &inputs, &workdir, redactor, log, provider_override)?;
    let init = session_init(&id, &resolved, &overrides);
    let kernel = Kernel::create(config, init).await?;
    Ok(Launched {
        kernel,
        questions,
        deltas,
        inputs,
        warnings: resolved.warnings.iter().map(|w| w.to_string()).collect(),
        log_path,
    })
}

/// Reopen an existing session from its record and log (`Kernel::open`).
pub async fn resume_session(
    opts: &LaunchOptions,
    id: SessionId,
    cause: ResumeCause,
    provider_override: Option<Arc<dyn Provider>>,
) -> Result<Launched, LaunchError> {
    let record = SessionRecord::load(opts, &id).await?;
    let resolved = resolve_profile(
        opts,
        &record.workdir,
        &record.agent,
        &record.overrides,
        true,
    )?;
    let log_path = opts.log_path(&id);
    let inputs = resolved.kernel_inputs.clone();
    let redactor = Arc::new(Redactor::new());
    let log = Arc::new(
        FileEventLog::open_for(&log_path, id.clone(), redactor.clone())
            .await
            .map_err(KernelError::from)?,
    );
    let Assembled {
        config,
        questions,
        deltas,
    } = assemble(
        opts,
        &inputs,
        &record.workdir,
        redactor,
        log,
        provider_override,
    )?;
    let kernel = Kernel::open(config, cause).await?;
    Ok(Launched {
        kernel,
        questions,
        deltas,
        inputs,
        warnings: resolved.warnings.iter().map(|w| w.to_string()).collect(),
        log_path,
    })
}

/// A user message from prompt text.
pub fn user_message(text: impl Into<String>) -> Message {
    Message::user_text(text)
}
