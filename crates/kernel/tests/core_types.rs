//! P1.1: serde round-trips and hash stability of every core type; `State` volatile-field
//! behavior; migrations; `derive_policy`; the event catalog (including the spec's JSON examples).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use kernel::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn h(byte: &str) -> Hash {
    Hash::parse(&format!("b3:{}", byte.repeat(32))).unwrap()
}

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(v: &T) -> Value {
    let json = serde_json::to_value(v).unwrap();
    let back: T = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(&back, v, "round trip changed the value");
    let text = serde_json::to_string(v).unwrap();
    let back2: T = serde_json::from_str(&text).unwrap();
    assert_eq!(&back2, v);
    json
}

fn profiles() -> ActiveProfiles {
    ActiveProfiles {
        model_profile_hash: h("1a"),
        agent_profile_hash: h("2b"),
        resolved_profile_hash: h("3c"),
        project_profile_hash: None,
        bundles_hash: Some(h("bd")),
    }
}

fn sample_state() -> State {
    let mut pending = BTreeMap::new();
    pending.insert(
        TaskId("t1-call_8f2a".into()),
        Task {
            id: TaskId("t1-call_8f2a".into()),
            tool_use_id: "call_8f2a".into(),
            tool_name: "run_script".into(),
            status: TaskStatus::Running,
            started_turn: 1,
            completed_turn: None,
            eta: Some(Duration::from_secs(600)),
            check_hint: Some(json!({"pid": 41022})),
            description: Some("mesh.sh --size 2mm (pid 41022)".into()),
            outcome: None,
            in_process_waker: true,
        },
    );
    State {
        schema_version: STATE_SCHEMA_VERSION,
        session_id: SessionId("s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4".into()),
        created_at: "2026-09-06T12:00:00.001Z".into(),
        turn: 1,
        session_status: SessionStatus::Running,
        messages: vec![
            Message::user_text("Mesh the bracket at 2 mm and run the static case."),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "I'll start the meshing job.".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "call_8f2a".into(),
                        name: "run_script".into(),
                        input: json!({"path": "/work/bracket/mesh.sh", "args": ["--size", "2mm"]}),
                    },
                ],
            },
            Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_8f2a".into(),
                    content: ToolResultContent::Json(json!({
                        "task_id": "t1-call_8f2a", "status": "running",
                        "eta_secs": 600, "description": "mesh.sh --size 2mm (pid 41022)"
                    })),
                    is_error: false,
                }],
            },
        ],
        pending_tasks: pending,
        profiles: profiles(),
        memory: None,
        notebook_path: Some(PathBuf::from("/work/bracket/.grist/notebook.md")),
        sandbox_policy_hash: h("4d"),
        sandbox_backend: "bwrap".into(),
    }
}

// ---- content blocks and messages ---------------------------------------------------------------

#[test]
fn content_blocks_round_trip_with_type_tags() {
    let blocks = vec![
        ContentBlock::Text { text: "hi".into() },
        ContentBlock::Thinking {
            text: "hmm".into(),
            signature: Some("sig".into()),
        },
        ContentBlock::Thinking {
            text: "hmm".into(),
            signature: None,
        },
        ContentBlock::ToolUse {
            id: "c1".into(),
            name: "read".into(),
            input: json!({"path": "/x"}),
        },
        ContentBlock::ToolResult {
            tool_use_id: "c1".into(),
            content: ToolResultContent::Blocks(vec![ContentBlock::Text { text: "ok".into() }]),
            is_error: false,
        },
        ContentBlock::Image {
            artifact_handle: ArtifactHandle(h("ab")),
            mime: "image/png".into(),
        },
        ContentBlock::TaskResult {
            task_id: TaskId("t1-c1".into()),
            tool_use_id: "c1".into(),
            status: TaskStatus::Succeeded,
            content: ToolResultContent::Json(json!({"exit_code": 0})),
            is_error: false,
        },
    ];
    let json = round_trip(&blocks);
    let types: Vec<&str> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "text",
            "thinking",
            "thinking",
            "tool_use",
            "tool_result",
            "image",
            "task_result"
        ]
    );
    assert_eq!(
        json[4]["content"],
        json!({"blocks": [{"type": "text", "text": "ok"}]})
    );
    assert_eq!(json[6]["content"], json!({"json": {"exit_code": 0}}));
    round_trip(&Message::user_text("x"));
    let pb = PromptBlock::new(PromptBlockKind::Role, "default", "You work in the repo.");
    assert_eq!(pb.hash, Hash::of_bytes(b"You work in the repo."));
    let j = round_trip(&pb);
    assert_eq!(j["kind"], "role");
}

#[test]
fn tool_result_content_validation() {
    assert!(ToolResultContent::Json(json!(1)).is_valid());
    assert!(ToolResultContent::Blocks(vec![ContentBlock::Text { text: "t".into() }]).is_valid());
    assert!(
        !ToolResultContent::Blocks(vec![ContentBlock::ToolUse {
            id: "x".into(),
            name: "y".into(),
            input: Value::Null
        }])
        .is_valid()
    );
}

// ---- state --------------------------------------------------------------------------------------

#[test]
fn state_round_trips_and_schema_version_is_first() {
    let state = sample_state();
    let json = round_trip(&state);
    let text = serde_json::to_string(&state).unwrap();
    assert!(text.starts_with("{\"schema_version\":1,"), "{text}");
    assert_eq!(json["pending_tasks"]["t1-call_8f2a"]["eta_secs"], 600);
    assert_eq!(state.open_tasks().count(), 1);
}

#[test]
fn state_hash_ignores_exactly_the_volatile_fields() {
    let base = sample_state();
    let base_hash = base.state_hash().unwrap();
    assert_eq!(
        State::VOLATILE_FIELDS,
        &[
            "session_id",
            "created_at",
            "sandbox_policy_hash",
            "sandbox_backend"
        ]
    );

    // Changing each volatile field leaves the hash unchanged.
    let mut s = base.clone();
    s.session_id = SessionId("other".into());
    assert_eq!(s.state_hash().unwrap(), base_hash);
    let mut s = base.clone();
    s.created_at = "1999-01-01T00:00:00.000Z".into();
    assert_eq!(s.state_hash().unwrap(), base_hash);
    let mut s = base.clone();
    s.sandbox_policy_hash = h("ee");
    assert_eq!(s.state_hash().unwrap(), base_hash);
    let mut s = base.clone();
    s.sandbox_backend = "none".into();
    assert_eq!(s.state_hash().unwrap(), base_hash);

    // Changing any other field changes it.
    let raw = serde_json::to_value(&base).unwrap();
    for (field, _) in raw.as_object().unwrap() {
        if State::VOLATILE_FIELDS.contains(&field.as_str()) {
            continue;
        }
        let mut changed = base.clone();
        match field.as_str() {
            "schema_version" => changed.schema_version += 1,
            "turn" => changed.turn += 1,
            "session_status" => changed.session_status = SessionStatus::Idle,
            "messages" => changed.messages.push(Message::user_text("more")),
            "pending_tasks" => changed.pending_tasks.clear(),
            "profiles" => changed.profiles.model_profile_hash = h("ff"),
            "memory" => changed.memory = Some(MemoryPointer::empty()),
            "notebook_path" => changed.notebook_path = None,
            other => panic!("unexpected State field `{other}`: update this test and the spec"),
        }
        // `schema_version` bump does not deserialize as a valid current state, but hashing is
        // over raw JSON so it still works here.
        assert_ne!(changed.state_hash().unwrap(), base_hash, "field {field}");
    }
}

#[test]
fn state_hash_is_stable_across_builds() {
    // Pinned so that a serialization change is caught: if this fails, `STATE_SCHEMA_VERSION`
    // and `event-schema.md` need a look.
    let canon = String::from_utf8(canonical_json(&sample_state()).unwrap()).unwrap();
    assert!(canon.starts_with("{\"created_at\":\"2026-09-06T12:00:00.001Z\",\"memory\":null,\"messages\":[{\"content\":[{\"text\":\"Mesh the bracket"));
    assert_eq!(
        sample_state().state_hash().unwrap().as_str(),
        "b3:5b8444ebeac9f81decccbd7c12c35b463e38a858c628a9534283ca23a4ec42e3"
    );
}

// ---- migrations -----------------------------------------------------------------------------

struct V0ToV1;

impl StateMigration for V0ToV1 {
    fn from_version(&self) -> u32 {
        0
    }
    fn migrate(&self, mut raw: Value) -> Result<Value, MigrationError> {
        let obj = raw.as_object_mut().unwrap();
        // v0 had no `memory` field and called `pending_tasks` `tasks`.
        if let Some(tasks) = obj.remove("tasks") {
            obj.insert("pending_tasks".into(), tasks);
        }
        obj.entry("memory").or_insert(Value::Null);
        obj.insert("schema_version".into(), json!(1));
        Ok(raw)
    }
}

struct LazyStep;

impl StateMigration for LazyStep {
    fn from_version(&self) -> u32 {
        0
    }
    fn migrate(&self, raw: Value) -> Result<Value, MigrationError> {
        Ok(raw) // forgets to bump schema_version
    }
}

#[test]
fn migration_registry_runs_registered_steps_and_fails_loudly_otherwise() {
    let mut raw = serde_json::to_value(sample_state()).unwrap();
    let obj = raw.as_object_mut().unwrap();
    obj.insert("schema_version".into(), json!(0));
    let tasks = obj.remove("pending_tasks").unwrap();
    obj.insert("tasks".into(), tasks);
    obj.remove("memory");

    // No migration registered: loud failure.
    let empty = MigrationRegistry::new();
    assert!(matches!(
        empty.migrate_to_current(raw.clone()),
        Err(MigrationError::MissingStep { from: 0, to: 1 })
    ));

    // Registered: migrates and reports.
    let mut reg = MigrationRegistry::new();
    reg.register(Box::new(V0ToV1)).unwrap();
    assert!(matches!(
        reg.register(Box::new(V0ToV1)),
        Err(MigrationError::Duplicate { from: 0 })
    ));
    let m = reg.migrate_to_current(raw.clone()).unwrap();
    assert_eq!(m.migrated, Some((0, 1)));
    assert_eq!(m.state.schema_version, 1);
    assert_eq!(m.state.pending_tasks.len(), 1);
    // The migration was hash-preserving on the hashed fields except schema_version.
    assert_eq!(m.state.messages, sample_state().messages);

    // Current version: no migration, `migrated: None`.
    let cur = reg
        .migrate_to_current(serde_json::to_value(sample_state()).unwrap())
        .unwrap();
    assert_eq!(cur.migrated, None);
    assert_eq!(cur.state, sample_state());

    // Newer than supported.
    let mut newer = serde_json::to_value(sample_state()).unwrap();
    newer["schema_version"] = json!(99);
    assert!(matches!(
        reg.migrate_to_current(newer),
        Err(MigrationError::NewerThanSupported {
            found: 99,
            supported: 1
        })
    ));

    // No version.
    assert!(matches!(
        reg.migrate_to_current(json!({"turn": 1})),
        Err(MigrationError::NoVersion)
    ));

    // A step that does not stamp the version is rejected rather than looping.
    let mut lazy = MigrationRegistry::new();
    lazy.register(Box::new(LazyStep)).unwrap();
    assert!(matches!(
        lazy.migrate_to_current(raw),
        Err(MigrationError::Failed { from: 0, .. })
    ));
}

// ---- tasks ----------------------------------------------------------------------------------

#[test]
fn task_types_round_trip_with_eta_secs() {
    let handle = TaskHandle {
        id: TaskId("t1-c".into()),
        status: TaskStatus::Pending,
        eta: Some(Duration::from_secs(5)),
        check_hint: Some(json!({"pid": 1})),
        description: None,
    };
    let j = round_trip(&handle);
    assert_eq!(j["eta_secs"], 5);
    assert!(j.get("eta").is_none());
    let no_eta: TaskHandle = serde_json::from_value(json!({
        "id": "t1-c", "status": "running", "check_hint": null, "description": null
    }))
    .unwrap();
    assert_eq!(no_eta.eta, None);
    let update = TaskUpdate {
        id: TaskId("t1-c".into()),
        status: TaskStatus::Succeeded,
        outcome: Some(TaskOutcome {
            content: ToolResultContent::Json(json!({"exit_code": 0})),
            is_error: false,
            artifact_handles: vec![],
        }),
        eta: None,
        check_hint: None,
        source: WakerSource::in_process_exit(json!({"pid": 41022})),
    };
    let j = round_trip(&update);
    assert_eq!(j["source"]["kind"], "in_process_exit");
    assert_eq!(j["source"]["trust_tier"], Value::Null);
    assert!(TaskStatus::Succeeded.is_terminal());
    assert!(!TaskStatus::Running.is_terminal());
    assert!(TrustTier::Interactive < TrustTier::Inbound);
}

// ---- tools, provider ------------------------------------------------------------------------------

#[test]
fn tool_call_hashes_are_as_specified() {
    let call = ToolCall {
        tool_use_id: "call_1".into(),
        name: "bash".into(),
        input: json!({"cmd": "ls", "cwd": "/w"}),
    };
    assert_eq!(
        call.args_hash().unwrap(),
        Hash::of_canonical_json(&call.input).unwrap()
    );
    assert_eq!(
        call.request_hash().unwrap(),
        Hash::of_canonical_json(
            &json!({"tool_use_id": "call_1", "name": "bash", "input": {"cwd": "/w", "cmd": "ls"}})
        )
        .unwrap()
    );
    let mut other = call.clone();
    other.tool_use_id = "call_2".into();
    assert_eq!(other.args_hash().unwrap(), call.args_hash().unwrap());
    assert_ne!(other.request_hash().unwrap(), call.request_hash().unwrap());
    round_trip(&call);
    round_trip(&ToolResult::Value(json!(1)));
    round_trip(&ToolResult::Blocks(vec![]));
    let out = ToolOutput {
        content: ToolResultContent::Json(json!({"ok": true})),
        is_error: false,
        artifact_handles: vec![],
        spilled: false,
        task: None,
        origin: ToolOutputOrigin::Invoke,
    };
    let j = round_trip(&out);
    assert_eq!(j["origin"], "invoke");
    assert!(tool::is_valid_tool_name("mcp.docs.search"));
    assert!(tool::is_valid_tool_name("run_script"));
    assert!(!tool::is_valid_tool_name("Read"));
    assert!(!tool::is_valid_tool_name("1x"));
    assert!(!tool::is_valid_tool_name(&"a".repeat(65)));
}

fn sample_request() -> ModelRequest {
    ModelRequest {
        model_id: "stand-in/default".into(),
        system: vec![PromptBlock::new(
            PromptBlockKind::Model,
            "stand-in",
            "Be brief.",
        )],
        messages: vec![Message::user_text("hi")],
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            input_schema: json!({"type": "object"}),
        }],
        params: ModelParams {
            temperature: Some(0.0),
            max_tokens: Some(2048),
            ..ModelParams::default()
        },
        trace: RequestTrace {
            session_id: SessionId("s".into()),
            turn: 1,
            attempt: 1,
            checkpoint_hash: h("7a"),
            request_id: "req-1".into(),
        },
    }
}

#[test]
fn request_and_response_hashes_exclude_volatile_parts() {
    let req = sample_request();
    let rh = req.request_hash().unwrap();
    let mut retry = req.clone();
    retry.trace.attempt = 3;
    retry.trace.request_id = "req-2".into();
    retry.trace.checkpoint_hash = h("00");
    assert_eq!(retry.request_hash().unwrap(), rh, "trace is volatile");
    let mut different = req.clone();
    different.tools[0].description = "Read a file, verbatim".into();
    assert_ne!(
        different.request_hash().unwrap(),
        rh,
        "tool descriptions are hashed"
    );
    let mut reordered = req.clone();
    reordered
        .system
        .push(PromptBlock::new(PromptBlockKind::Role, "r", "x"));
    assert_ne!(reordered.request_hash().unwrap(), rh);
    assert_eq!(
        rh,
        Hash::of_canonical_json(&json!({
            "model_id": req.model_id, "system": req.system, "messages": req.messages,
            "tools": req.tools, "params": req.params
        }))
        .unwrap()
    );
    round_trip(&req);

    let resp = ModelResponse {
        content: vec![
            ContentBlock::Thinking {
                text: "t".into(),
                signature: Some("sig".into()),
            },
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "read".into(),
                input: json!({}),
            },
        ],
        stop_reason: StopReason::ToolUse,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 2,
            ..Usage::default()
        },
        model_id: "qwen".into(),
        raw_response_hash: Hash::of_bytes(b"raw"),
        response_id: Some("chatcmpl-1".into()),
    };
    let rh = resp.response_hash().unwrap();
    let mut v = resp.clone();
    v.response_id = None;
    v.raw_response_hash = Hash::of_bytes(b"other");
    assert_eq!(v.response_hash().unwrap(), rh);
    let mut sig = resp.clone();
    sig.content[0] = ContentBlock::Thinking {
        text: "t".into(),
        signature: None,
    };
    assert_ne!(
        sig.response_hash().unwrap(),
        rh,
        "signature round-trips into the hash"
    );
    assert_eq!(resp.tool_calls().len(), 1);
    assert_eq!(resp.tool_calls()[0].tool_use_id, "c1");
    let j = round_trip(&resp);
    assert_eq!(j["stop_reason"], "tool_use");
    let j = round_trip(&StopReason::Other("weird".into()));
    assert_eq!(j, json!({"other": "weird"}));
    round_trip(&ModelDelta::TextDelta {
        index: 0,
        text: "x".into(),
    });
    let j = round_trip(&ModelDelta::Complete(resp));
    assert_eq!(j["type"], "complete");
}

#[test]
fn provider_error_retry_classes() {
    assert!(ProviderError::RateLimited { retry_after: None }.retryable());
    assert!(
        ProviderError::Server {
            status: 503,
            message: String::new()
        }
        .retryable()
    );
    assert!(
        !ProviderError::Server {
            status: 501,
            message: String::new()
        }
        .retryable()
    );
    assert!(
        !ProviderError::Server {
            status: 505,
            message: String::new()
        }
        .retryable()
    );
    assert!(ProviderError::Timeout(Duration::from_secs(1)).retryable());
    assert!(ProviderError::Transport("reset".into()).retryable());
    assert!(
        !ProviderError::Client {
            status: 400,
            message: String::new()
        }
        .retryable()
    );
    assert!(!ProviderError::Auth("x".into()).retryable());
    assert!(
        !ProviderError::ReplayMiss {
            checkpoint_hash: h("00"),
            request_hash: h("11")
        }
        .retryable()
    );
    assert_eq!(
        ProviderError::RateLimited { retry_after: None }.class(),
        "provider_rate_limited"
    );
    assert_eq!(
        ProviderError::ContextTooLong("x".into()).class(),
        "provider_context_too_long"
    );
}

// ---- capabilities and sandbox policy ------------------------------------------------------------

fn cap(s: &str) -> Capability {
    s.parse().unwrap()
}

#[test]
fn derive_policy_maps_atoms_and_enforces_grants() {
    let grants = vec![
        cap("fs.rw:/work"),
        cap("proc:bash"),
        cap("net:*"),
        cap("tool:bash"),
    ];
    let caps = vec![
        cap("fs.ro:/work/src"),
        cap("fs.rw:/work"),
        cap("proc:bash"),
        cap("tool:bash"),
    ];
    let p = derive_policy(&caps, &grants).unwrap();
    assert_eq!(
        p.mounts,
        vec![
            Mount {
                path: "/work".into(),
                mode: FsMode::Rw
            },
            Mount {
                path: "/work/src".into(),
                mode: FsMode::Ro
            }
        ]
    );
    assert_eq!(p.programs, BTreeSet::from(["bash".to_owned()]));
    assert!(!p.net.enabled, "network defaults to off even when granted");
    assert_eq!(p.timeout, Duration::from_secs(600));
    assert_eq!(p.scratch_tmpfs_mb, 256);
    assert_eq!(p.env_allowlist, SandboxLimits::default().env_allowlist);
    let j = round_trip(&p);
    assert_eq!(j["timeout_secs"], 600);
    assert!(p.hash().is_ok());
    assert_eq!(p.fs().mounts, p.mounts);
    assert_eq!(p.proc_().timeout, p.timeout);

    // Exceeds.
    let err = derive_policy(&[cap("fs.rw:/etc")], &grants).unwrap_err();
    assert_eq!(
        err,
        PolicyError::Exceeds {
            cap: "fs.rw:/etc".into()
        }
    );
    // Secret in caps.
    assert_eq!(
        derive_policy(&[cap("secret:X")], &[cap("secret:X")]).unwrap_err(),
        PolicyError::SecretInSandbox
    );
    // Same path, both modes: the wider wins.
    let p = derive_policy(&[cap("fs.ro:/work"), cap("fs.rw:/work")], &grants).unwrap();
    assert_eq!(p.mounts.len(), 1);
    assert_eq!(p.mounts[0].mode, FsMode::Rw);
    // Empty caps: no mounts, no programs, no net.
    let p = derive_policy(&[], &grants).unwrap();
    assert!(p.mounts.is_empty() && p.programs.is_empty() && !p.net.enabled);
}

#[test]
fn derive_policy_limits_network_env_and_timeouts() {
    let grants = vec![cap("net:a.example,b.example"), cap("net:c.example")];
    let limits = SandboxLimits {
        network: true,
        timeout: Duration::from_secs(0),
        scratch_tmpfs_mb: 0,
        ..SandboxLimits::default()
    };
    let p = derive_policy_with(
        &[cap("net:a.example"), cap("net:c.example")],
        &grants,
        &limits,
    )
    .unwrap();
    assert!(p.net.enabled);
    assert_eq!(
        p.net.allow,
        NetAllow::Hosts(BTreeSet::from([
            "a.example".to_owned(),
            "c.example".to_owned()
        ]))
    );
    assert_eq!(p.timeout, Duration::from_secs(1), "clamped up to 1 s");
    assert_eq!(p.scratch_tmpfs_mb, 1, "never 0");
    // Any absorbs.
    let p = derive_policy_with(
        &[cap("net:a.example"), cap("net:*")],
        &[cap("net:*")],
        &limits,
    )
    .unwrap();
    assert_eq!(p.net.allow, NetAllow::Any);
    // Master switch off masks atoms.
    let off = SandboxLimits::default();
    let p = derive_policy_with(&[cap("net:*")], &[cap("net:*")], &off).unwrap();
    assert!(!p.net.enabled);
    // Timeout clamped down to 24 h.
    let long = SandboxLimits {
        timeout: Duration::from_secs(48 * 3600),
        ..SandboxLimits::default()
    };
    assert_eq!(
        derive_policy_with(&[], &[], &long).unwrap().timeout,
        Duration::from_secs(24 * 3600)
    );
    // Secret-like env names are rejected.
    let bad = SandboxLimits {
        env_allowlist: BTreeSet::from(["PATH".to_owned(), "OPENAI_API_KEY".to_owned()]),
        ..SandboxLimits::default()
    };
    assert_eq!(
        derive_policy_with(&[], &[], &bad).unwrap_err(),
        PolicyError::EnvNameForbidden("OPENAI_API_KEY".into())
    );
    for name in [
        "MyToken",
        "aws_secret",
        "PASSWD",
        "Credential_x",
        "AUTH_HEADER",
    ] {
        assert!(sandbox::env_name_is_secret_like(name), "{name}");
    }
    assert!(!sandbox::env_name_is_secret_like("PATH"));
    let j = round_trip(&SandboxLimits::default());
    assert_eq!(j["timeout_s"], 600);
    assert_eq!(
        j["env_allow"],
        json!(["LANG", "LC_ALL", "PATH", "TERM", "TZ"])
    );
}

#[test]
fn fs_policy_check_most_specific_mount_wins() {
    let policy = FsPolicy {
        mounts: vec![
            Mount {
                path: "/work".into(),
                mode: FsMode::Rw,
            },
            Mount {
                path: "/work/ro".into(),
                mode: FsMode::Ro,
            },
        ],
    };
    assert!(
        policy
            .check(std::path::Path::new("/work/a.txt"), FsMode::Rw)
            .is_ok()
    );
    assert!(
        policy
            .check(std::path::Path::new("/work/ro/x"), FsMode::Ro)
            .is_ok()
    );
    assert!(matches!(
        policy.check(std::path::Path::new("/work/ro/x"), FsMode::Rw),
        Err(PolicyError::PathDenied { .. })
    ));
    assert!(
        policy
            .check(std::path::Path::new("/etc/passwd"), FsMode::Ro)
            .is_err()
    );
    assert!(
        policy
            .check(std::path::Path::new("/workspace"), FsMode::Ro)
            .is_err()
    );
}

// ---- host types ------------------------------------------------------------------------------

#[test]
fn host_types_round_trip_and_secrets_never_print() {
    let cmd = Command {
        program: "bash".into(),
        args: vec!["-c".into(), "ls".into()],
        cwd: Some("/w".into()),
        env: BTreeMap::from([("PATH".to_owned(), "/bin".to_owned())]),
        stdin: Some(b"x".to_vec()),
    };
    round_trip(&cmd);
    let out = ProcessOutput {
        exit_code: Some(0),
        signal: None,
        stdout: b"ok".to_vec(),
        stderr: vec![],
        timed_out: false,
        duration: Duration::from_millis(1234),
    };
    let j = round_trip(&out);
    assert_eq!(j["duration_ms"], 1234);
    let j = round_trip(&HttpMethod::Post);
    assert_eq!(j, "POST");
    let handle = SecretHandle::with_locator("LITELLM_CI_API_KEY", "env:LITELLM_CI_API_KEY");
    let j = serde_json::to_value(&handle).unwrap();
    assert_eq!(
        j,
        json!({"name": "LITELLM_CI_API_KEY"}),
        "locator is never serialized"
    );
    assert_eq!(format!("{handle:?}"), "SecretHandle(LITELLM_CI_API_KEY)");
    let secret = SecretString::new("sk-verysecret");
    assert_eq!(format!("{secret:?}"), "SecretString(***)");
    assert_eq!(format!("{secret}"), "SecretString(***)");
    assert_eq!(secret.expose(), "sk-verysecret");
    round_trip(&AskUserRequest {
        question_id: "q1-c".into(),
        question: "Both?".into(),
        options: vec!["both".into()],
        allow_free_text: true,
    });
    round_trip(&UserAnswer {
        question_id: "q1-c".into(),
        answer: None,
    });
}

// ---- artifact, memory, cancel, config ------------------------------------------------------------

#[tokio::test]
async fn noop_store_and_memory_behave_as_specified() {
    let store = NoopArtifactStore;
    let h1 = store.put(b"bytes", "text/plain").await.unwrap();
    let h2 = store.put(b"bytes", "text/plain").await.unwrap();
    assert_eq!(h1, h2);
    assert_eq!(h1.0, Hash::of_bytes(b"bytes"));
    assert!(matches!(
        store.get(&h1).await,
        Err(ArtifactError::NotFound(_))
    ));
    assert!(matches!(
        store.stat(&h1).await,
        Err(ArtifactError::NotFound(_))
    ));
    assert_eq!(store.name(), "noop");

    let mem = NoopMemory;
    let p = mem
        .store(
            None,
            MemoryItem {
                id: "i".into(),
                content: json!(1),
                tags: vec![],
            },
        )
        .await
        .unwrap();
    assert_eq!(p, MemoryPointer::empty());
    assert_eq!(
        MemoryPointer::empty().0,
        Hash::of_canonical_json(&Value::Null).unwrap()
    );
    assert!(
        mem.retrieve(
            &p,
            &MemoryQuery {
                text: "x".into(),
                limit: 5,
                tags: vec![]
            }
        )
        .await
        .unwrap()
        .is_empty()
    );
    round_trip(&Spilled {
        handle: h1,
        head: "a".into(),
        tail: "b".into(),
        size: 3,
        mime: "text/plain".into(),
    });
}

#[test]
fn config_types_round_trip_with_defaults() {
    let j = round_trip(&SpillConfig::default());
    assert_eq!(
        j,
        json!({"cap_bytes": 16384, "head_bytes": 2048, "tail_bytes": 2048})
    );
    let (clamped, changed) = SpillConfig {
        cap_bytes: 0,
        ..SpillConfig::default()
    }
    .clamped();
    assert!(changed && clamped.cap_bytes == 1024);
    let (clamped, changed) = SpillConfig {
        cap_bytes: 1 << 40,
        ..SpillConfig::default()
    }
    .clamped();
    assert!(changed && clamped.cap_bytes == 1024 * 1024);
    let j = round_trip(&RetryPolicy::default());
    assert_eq!(
        j,
        json!({"max_attempts": 5, "base_delay_ms": 500, "max_delay_ms": 30000, "multiplier": 2.0, "jitter": true, "request_timeout_secs": 300})
    );
    let j = round_trip(&CancelScope::Tool {
        tool_use_id: "c".into(),
    });
    assert_eq!(j, json!({"scope": "tool", "tool_use_id": "c"}));
    let j = round_trip(&ResumeCause::Operator);
    assert_eq!(j, json!({"cause": "operator"}));
    let j = round_trip(&TurnOutcome::Failed {
        error_class: "internal".into(),
    });
    assert_eq!(j["outcome"], "failed");
    let j = round_trip(&RunStop::Done);
    assert_eq!(j, json!({"stopped": "done"}));
}

// ---- events ---------------------------------------------------------------------------------------

fn sample_events() -> Vec<EventBody> {
    vec![
        EventBody::LogOpened(LogOpenedPayload {
            event_schema_version: 1,
            state_schema_version: 1,
            kernel_version: "0.0.0".into(),
            mode: LogMode::Live,
        }),
        EventBody::SessionCreated(SessionCreatedPayload {
            session_id: SessionId("s".into()),
            created_at: "2026-09-06T12:00:00.001Z".into(),
            kernel_version: "0.0.0".into(),
            profiles: profiles(),
            model_id: "stand-in/default".into(),
            tools: vec!["bash".into()],
            grants: vec![cap("fs.rw:/work")],
            sandbox_backend: "bwrap".into(),
            sandbox_policy_hash: h("4d"),
            artifact_store: "noop".into(),
            memory: "noop".into(),
            provider: "openai_compat".into(),
            spill: SpillConfig::default(),
            retry: RetryPolicy::default(),
            notebook_path: None,
            sandbox_limits: SandboxLimits::default(),
            overrides: None,
            parent: None,
        }),
        EventBody::ProfileLoad(ProfileLoadPayload {
            kind: ProfileKind::Model,
            name: "stand-in".into(),
            path: None,
            hash: h("1a"),
            rejected: false,
            turn: 0,
        }),
        EventBody::MiddlewareChainResolved(MiddlewareChainResolvedPayload {
            chain: vec![ChainEntry {
                index: 0,
                name: "recorder".into(),
                priority: 990,
                source: MiddlewareSource::Kernel,
                config_hash: None,
            }],
            chain_hash: h("5e"),
        }),
        EventBody::UserMessage(UserMessagePayload {
            turn: 0,
            applied: AppliedAt {
                turn: 0,
                at: AppliedPoint::Created,
            },
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }),
        EventBody::ModelRequest(ModelRequestPayload {
            turn: 1,
            request_hash: h("6f"),
            checkpoint_hash: h("7a"),
            model_id: "m".into(),
            profiles: profiles(),
            system_prompt_hash: h("8b"),
            prompt_blocks: vec![PromptBlockRef {
                kind: PromptBlockKind::Model,
                name: "stand-in".into(),
                hash: h("a1"),
            }],
            message_count: 1,
            tool_names: vec!["bash".into()],
            params_hash: h("9c"),
        }),
        EventBody::ModelResponse(ModelResponsePayload {
            turn: 1,
            request_hash: h("6f"),
            response_hash: h("0d"),
            raw_response_hash: h("1e"),
            model_id: "m".into(),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            content: vec![],
            attempts: 1,
        }),
        EventBody::ProviderRetry(ProviderRetryPayload {
            turn: 1,
            attempt: 1,
            error_class: "provider_rate_limited".into(),
            message: "429".into(),
            delay_ms: 412,
        }),
        EventBody::ToolCall(ToolCallPayload {
            turn: 1,
            tool_use_id: "c".into(),
            name: "bash".into(),
            args_hash: h("2f"),
            request_hash: h("3a"),
            checkpoint_hash: h("7a"),
            input: json!({}),
            registered: true,
            kind: Some(ToolKind::Stateless),
            capabilities: vec![cap("proc:bash")],
            policy_hash: Some(h("4b")),
        }),
        EventBody::ToolResult(ToolResultPayload {
            turn: 1,
            tool_use_id: "c".into(),
            name: "bash".into(),
            result_hash: h("5c"),
            is_error: false,
            content: ToolResultContent::Json(json!({"ok": true})),
            artifact_handles: vec![],
            spilled: false,
            spill: None,
            duration_ms: 3,
            origin: ToolOutputOrigin::Invoke,
            task: None,
        }),
        EventBody::TaskStarted(TaskStartedPayload {
            turn: 1,
            task_id: TaskId("t1-c".into()),
            tool_use_id: "c".into(),
            tool_name: "run_script".into(),
            status: TaskStatus::Running,
            eta_secs: Some(1),
            description: None,
            check_hint: None,
            in_process_waker: true,
        }),
        EventBody::TaskUpdate(TaskUpdatePayload {
            turn: 1,
            task_id: TaskId("t1-c".into()),
            from_status: Some(TaskStatus::Running),
            status: TaskStatus::Succeeded,
            outcome: None,
            eta_secs: None,
            check_hint: None,
            waker: WakerSource::in_process_exit(Value::Null),
            applied: AppliedAt {
                turn: 1,
                at: AppliedPoint::Ignored,
            },
            ignored_reason: Some("missing_outcome".into()),
        }),
        EventBody::Checkpoint(CheckpointPayload {
            turn: 1,
            reason: CheckpointReason::TurnEnd,
            session_status: SessionStatus::Running,
            state_hash: sample_state().state_hash().unwrap(),
            state: sample_state(),
        }),
        EventBody::Suspended(SuspendedPayload {
            turn: 1,
            reason: SuspendReason::PendingTasks,
            pending_task_ids: vec![TaskId("t1-c".into())],
            in_process_wakers: 1,
            checkpoint_hash: h("9a"),
        }),
        EventBody::Resumed(ResumedPayload {
            from_checkpoint_hash: h("9a"),
            from_checkpoint_seq: 22,
            from_status: SessionStatus::Suspended,
            cause: ResumeCauseKind::TaskUpdate,
            waker: None,
            new_process: true,
            kernel_version: "0.0.0".into(),
            event_schema_version: 1,
        }),
        EventBody::Cancelled(CancelledPayload {
            turn: 1,
            scope: CancelScopeKind::Turn,
            tool_use_id: None,
            task_id: None,
            phase: LoopPhase::ModelCall,
            signalled: false,
            skipped_tool_use_ids: vec![],
        }),
        EventBody::TurnFailed(TurnFailedPayload {
            turn: 1,
            error_class: "middleware".into(),
            attempts: 1,
            message: "boom".into(),
            middleware: Some("x".into()),
            hook: Some("before_model".into()),
        }),
        EventBody::SessionFailed(SessionFailedPayload {
            turn: 1,
            cause_seq: 5,
            error_class: "middleware".into(),
            checkpoint_hash: h("ab"),
            resumable: true,
        }),
        EventBody::SessionEnded(SessionEndedPayload {
            turn: 1,
            by: EndedBy::User,
            checkpoint_hash: h("cd"),
            cancelled_task_ids: vec![],
        }),
        EventBody::Recovered(RecoveredPayload {
            checkpoint_hash: h("ef"),
            checkpoint_seq: 41,
            discarded_seq: Some(SeqRange { from: 42, to: 44 }),
            restored_status: SessionStatus::Running,
            tasks_cancelled: vec![],
            kernel_version: "0.0.0".into(),
            event_schema_version: 1,
        }),
        EventBody::Compaction(CompactionPayload {
            turn: 1,
            strategy: "notebook".into(),
            before_state_hash: h("01"),
            after_state_hash: h("02"),
            notebook_hash: None,
            messages_before: 9,
            messages_after: 2,
            tokens_before: None,
            summary_artifact: None,
        }),
        EventBody::Spawn(SpawnPayload {
            turn: 1,
            tool_use_id: "c".into(),
            task_id: TaskId("t1-c".into()),
            catalog_name: "dbg".into(),
            child_session_id: SessionId("child".into()),
            child_log_path: "/x.jsonl".into(),
            child_profiles: profiles(),
            child_tools: vec![],
            child_grants: vec![],
        }),
        EventBody::ChildCompleted(ChildCompletedPayload {
            turn: 1,
            task_id: TaskId("t1-c".into()),
            child_session_id: SessionId("child".into()),
            child_status: SessionStatus::Done,
            child_final_checkpoint_hash: h("66"),
            child_usage: None,
        }),
        EventBody::ContextUsage(ContextUsagePayload {
            turn: 1,
            input_tokens: 10,
            budget_tokens: 40000,
            over_budget: false,
            source: UsageSource::ProviderUsage,
        }),
        EventBody::HarnessEdit(HarnessEditPayload {
            turn: 1,
            target: "profiles/x.toml".into(),
            target_kind: "prompt".into(),
            before_hash: None,
            after_hash: h("88"),
            rationale_artifact: None,
        }),
        EventBody::AskUser(AskUserPayload {
            turn: 1,
            tool_use_id: "c".into(),
            question_id: "q1-c".into(),
            question: "?".into(),
            options: vec![],
            allow_free_text: true,
        }),
        EventBody::UserAnswer(UserAnswerPayload {
            turn: 1,
            tool_use_id: "c".into(),
            question_id: "q1-c".into(),
            answer: Some("yes".into()),
            declined: false,
        }),
        EventBody::Warning(WarningPayload::kernel(
            0,
            "sandbox_backend_none",
            "none",
            None,
        )),
    ]
}

#[test]
fn every_event_kind_round_trips_and_is_named_as_in_the_spec() {
    let bodies = sample_events();
    assert_eq!(bodies.len(), 28, "one sample per kind");
    let kinds: Vec<&str> = bodies.iter().map(|b| b.kind()).collect();
    assert_eq!(kinds, EventBody::KNOWN_KINDS);
    assert_eq!(
        EventBody::KNOWN_KINDS,
        &[
            "log_opened",
            "session_created",
            "profile_load",
            "middleware_chain_resolved",
            "user_message",
            "model_request",
            "model_response",
            "provider_retry",
            "tool_call",
            "tool_result",
            "task_started",
            "task_update",
            "checkpoint",
            "suspended",
            "resumed",
            "cancelled",
            "turn_failed",
            "session_failed",
            "session_ended",
            "recovered",
            "compaction",
            "spawn",
            "child_completed",
            "context_usage",
            "harness_edit",
            "ask_user",
            "user_answer",
            "warning"
        ]
    );
    for (seq, body) in bodies.into_iter().enumerate() {
        let ev = Event {
            seq: seq as u64,
            ts: "2026-09-06T12:00:00.000Z".into(),
            session_id: SessionId("s".into()),
            body,
        };
        let json = round_trip(&ev);
        let obj = json.as_object().unwrap();
        assert_eq!(obj.len(), 5, "envelope has exactly five members");
        let text = serde_json::to_string(&ev).unwrap();
        assert!(
            text.starts_with(&format!(
                "{{\"seq\":{seq},\"ts\":\"2026-09-06T12:00:00.000Z\",\"session_id\":\"s\",\"kind\":\"{}\",\"payload\":",
                ev.body.kind()
            )),
            "envelope members are written in order: {text}"
        );
        assert_eq!(obj["kind"], ev.body.kind());
        assert_eq!(obj["payload"], ev.body.payload_value().unwrap());
        // Hash stability: the payload's canonical JSON is deterministic.
        let a = Hash::of_canonical_json(&ev.body.payload_value().unwrap()).unwrap();
        let b = Hash::of_canonical_json(
            &serde_json::from_value::<Event>(json)
                .unwrap()
                .body
                .payload_value()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(a, b);
    }
}

#[test]
fn unknown_event_kinds_do_not_fail_the_reader() {
    let line = r#"{"seq":3,"ts":"2026-09-06T12:00:00.000Z","session_id":"s","kind":"future_kind","payload":{"x":1}}"#;
    let ev: Event = serde_json::from_str(line).unwrap();
    assert_eq!(ev.body.kind(), "future_kind");
    assert!(matches!(&ev.body, EventBody::Unknown { payload, .. } if payload["x"] == 1));
    // Unknown payload fields on a known kind are ignored.
    let line = r#"{"seq":0,"ts":"t","session_id":"s","kind":"log_opened","payload":{"event_schema_version":1,"state_schema_version":1,"kernel_version":"0.0.0","mode":"live","future_field":true}}"#;
    let ev: Event = serde_json::from_str(line).unwrap();
    assert!(matches!(ev.body, EventBody::LogOpened(_)));
}

/// The literal JSON examples from `event-schema.md` §2 parse into the typed payloads.
#[test]
fn spec_examples_parse() {
    let examples = [
        r#"{"seq":0,"ts":"2026-09-06T12:00:00.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"log_opened","payload":{"event_schema_version":1,"state_schema_version":1,"kernel_version":"0.0.0","mode":"live"}}"#,
        r#"{"seq":1,"ts":"2026-09-06T12:00:00.003Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"session_created","payload":{"session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","created_at":"2026-09-06T12:00:00.001Z","kernel_version":"0.0.0","profiles":{"model_profile_hash":"b3:1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a","agent_profile_hash":"b3:2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b","resolved_profile_hash":"b3:3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c","project_profile_hash":null,"bundles_hash":"b3:bdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbd"},"model_id":"stand-in/qwen2.5-7b-instruct","tools":["bash","edit","python","read","run_script","write"],"grants":["fs.ro:/opt/inhouse","fs.rw:/work/bracket","proc:sbatch","tool:bash","tool:edit","tool:python","tool:read","tool:run_script","tool:write"],"sandbox_backend":"bwrap","sandbox_policy_hash":"b3:4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d","artifact_store":"noop","memory":"noop","provider":"openai_compat","spill":{"cap_bytes":16384,"head_bytes":2048,"tail_bytes":2048},"retry":{"max_attempts":5,"base_delay_ms":500,"max_delay_ms":30000,"multiplier":2.0,"jitter":true,"request_timeout_secs":300},"notebook_path":"/work/bracket/.grist/notebook.md","sandbox_limits":{"timeout_s":600,"scratch_tmpfs_mb":256,"env_allow":["LANG","PATH","TERM"],"network":false},"overrides":null,"parent":null}}"#,
        r#"{"seq":12,"ts":"2026-09-06T12:00:08.530Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"tool_result","payload":{"turn":1,"tool_use_id":"call_8f2a","name":"run_script","result_hash":"b3:5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c","is_error":false,"content":{"json":{"task_id":"t1-call_8f2a","status":"running","eta_secs":600,"description":"mesh.sh --size 2mm (pid 41022)"}},"artifact_handles":[],"spilled":false,"spill":null,"duration_ms":112,"origin":"invoke","task":{"task_id":"t1-call_8f2a","status":"running"}}}"#,
        r#"{"seq":24,"ts":"2026-09-06T12:09:41.200Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"task_update","payload":{"turn":3,"task_id":"t1-call_8f2a","from_status":"running","status":"succeeded","outcome":{"content":{"json":{"exit_code":0,"log":"/work/bracket/mesh.log","elements":184220}},"is_error":false,"artifact_handles":[]},"eta_secs":null,"check_hint":null,"waker":{"kind":"in_process_exit","trust_tier":null,"detail":{"pid":41022}},"applied":{"turn":3,"at":"suspended"},"ignored_reason":null}}"#,
        r#"{"seq":14,"ts":"2026-09-06T12:00:08.540Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"checkpoint","payload":{"turn":1,"reason":"turn_end","session_status":"running","state_hash":"b3:8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f","state":{"schema_version":1,"session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","created_at":"2026-09-06T12:00:00.001Z","turn":1,"session_status":"running","messages":[{"role":"user","content":[{"type":"text","text":"Mesh the bracket at 2 mm and run the static case."}]},{"role":"assistant","content":[{"type":"text","text":"I'll start the meshing job."},{"type":"tool_use","id":"call_8f2a","name":"run_script","input":{"path":"/work/bracket/mesh.sh","args":["--size","2mm"]}}]},{"role":"tool","content":[{"type":"tool_result","tool_use_id":"call_8f2a","content":{"json":{"task_id":"t1-call_8f2a","status":"running","eta_secs":600,"description":"mesh.sh --size 2mm (pid 41022)"}},"is_error":false}]}],"pending_tasks":{"t1-call_8f2a":{"id":"t1-call_8f2a","tool_use_id":"call_8f2a","tool_name":"run_script","status":"running","started_turn":1,"completed_turn":null,"eta_secs":600,"check_hint":{"pid":41022,"log":"/work/bracket/mesh.log"},"description":"mesh.sh --size 2mm (pid 41022)","outcome":null,"in_process_waker":true},"t2-x":{"id":"t2-x","tool_use_id":"x","tool_name":"run_script","status":"succeeded","started_turn":2,"completed_turn":3,"eta_secs":null,"check_hint":null,"description":null,"outcome":{"content":{"json":1},"is_error":false,"artifact_handles":[]},"in_process_waker":false}},"profiles":{"model_profile_hash":"b3:1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a","agent_profile_hash":"b3:2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b","resolved_profile_hash":"b3:3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c","project_profile_hash":null,"bundles_hash":"b3:bdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbd"},"memory":null,"notebook_path":"/work/bracket/.grist/notebook.md","sandbox_policy_hash":"b3:4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d","sandbox_backend":"bwrap"}}}"#,
        r#"{"seq":40,"ts":"2026-09-06T12:15:03.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"cancelled","payload":{"turn":6,"scope":"turn","tool_use_id":"call_c0de","task_id":null,"phase":"tool","signalled":true,"skipped_tool_use_ids":["call_c0df"]}}"#,
        r#"{"seq":45,"ts":"2026-09-06T12:20:00.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"recovered","payload":{"checkpoint_hash":"b3:efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef","checkpoint_seq":41,"discarded_seq":{"from":42,"to":44},"restored_status":"running","tasks_cancelled":[],"kernel_version":"0.0.0","event_schema_version":1}}"#,
        r#"{"seq":5,"ts":"2026-09-06T12:00:00.009Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"warning","payload":{"turn":0,"class":"sandbox_backend_none","message":"sandbox backend `none` is in use; tools run WITHOUT isolation (development build)","detail":null,"source":"kernel"}}"#,
    ];
    for line in examples {
        let ev: Event = serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"));
        assert!(!matches!(ev.body, EventBody::Unknown { .. }), "{line}");
        // Re-serializing and parsing yields the same typed event (field-level round trip).
        let again: Event = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(again, ev);
    }
}

#[test]
fn extension_events_map_onto_the_closed_set() {
    let ev = ExtensionEvent::Warning(WarningPayload::kernel(1, "x", "y", None));
    let body: EventBody = ev.into();
    assert_eq!(body.kind(), "warning");
    let ev = ExtensionEvent::ProfileLoad(ProfileLoadPayload {
        kind: ProfileKind::Skill,
        name: "s".into(),
        path: None,
        hash: h("00"),
        rejected: true,
        turn: 2,
    });
    assert_eq!(EventBody::from(ev).kind(), "profile_load");
}

// ---- in-memory log ----------------------------------------------------------------------------

#[tokio::test]
async fn memory_log_assigns_seq_redacts_and_restores() {
    let redactor = std::sync::Arc::new(Redactor::new());
    redactor.register_secret(&SecretString::new("sk-verysecretvalue0123456789"));
    let log = MemoryEventLog::new(SessionId("s".into()), redactor);
    assert_eq!(log.last_seq(), None);
    let e0 = log
        .append(EventBody::Warning(WarningPayload::kernel(
            0,
            "x",
            "token sk-verysecretvalue0123456789 here",
            None,
        )))
        .await
        .unwrap();
    assert_eq!(e0.seq, 0);
    match &e0.body {
        EventBody::Warning(w) => assert_eq!(w.message, "token [REDACTED:secret] here"),
        _ => panic!(),
    }
    // The writer pass changed something, so a late_redaction warning follows.
    let events = log.events();
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[1].body, EventBody::Warning(w) if w.class == "late_redaction"));
    assert_eq!(log.last_seq(), Some(1));

    let state = sample_state();
    let cp = CheckpointPayload {
        turn: 1,
        reason: CheckpointReason::TurnEnd,
        session_status: SessionStatus::Running,
        state_hash: state.state_hash().unwrap(),
        state: state.clone(),
    };
    log.append(EventBody::Checkpoint(cp.clone())).await.unwrap();
    let reader = log.reader();
    let (ev, found) = reader.latest_checkpoint().unwrap().unwrap();
    assert_eq!(ev.seq, 2);
    assert_eq!(found, cp);
    let reg = MigrationRegistry::new();
    let restored = reader.restore(&cp.state_hash, &reg).unwrap();
    assert_eq!(restored.state, state);
    assert!(restored.migrated.is_none());
    assert_eq!(reader.restore_latest(&reg).unwrap().unwrap().state, state);
    assert!(matches!(
        reader.restore(&h("00"), &reg),
        Err(RestoreError::NotFound(_))
    ));
    assert_eq!(reader.iter().count(), 3);
    assert_eq!(reader.effective().count(), 3);
}

#[tokio::test]
async fn memory_log_effective_view_hides_discarded_ranges() {
    let redactor = std::sync::Arc::new(Redactor::new());
    let log = MemoryEventLog::new(SessionId("s".into()), redactor);
    let state = sample_state();
    let cp = |reason| {
        EventBody::Checkpoint(CheckpointPayload {
            turn: 1,
            reason,
            session_status: SessionStatus::Running,
            state_hash: state.state_hash().unwrap(),
            state: state.clone(),
        })
    };
    log.append(cp(CheckpointReason::TurnEnd)).await.unwrap(); // 0
    log.append(EventBody::Warning(WarningPayload::kernel(1, "a", "", None)))
        .await
        .unwrap(); // 1
    log.append(EventBody::Warning(WarningPayload::kernel(1, "b", "", None)))
        .await
        .unwrap(); // 2
    log.append(EventBody::Recovered(RecoveredPayload {
        checkpoint_hash: state.state_hash().unwrap(),
        checkpoint_seq: 0,
        discarded_seq: Some(SeqRange { from: 1, to: 2 }),
        restored_status: SessionStatus::Running,
        tasks_cancelled: vec![],
        kernel_version: "0.0.0".into(),
        event_schema_version: 1,
    }))
    .await
    .unwrap(); // 3
    let reader = log.reader();
    assert_eq!(reader.iter().count(), 4);
    let eff: Vec<u64> = reader.effective().map(|e| e.unwrap().seq).collect();
    assert_eq!(eff, vec![0, 3]);
}

#[tokio::test]
async fn restore_detects_a_tampered_checkpoint() {
    let redactor = std::sync::Arc::new(Redactor::new());
    let state = sample_state();
    let mut tampered = state.clone();
    tampered.turn = 99;
    let ev = Event {
        seq: 0,
        ts: "t".into(),
        session_id: SessionId("s".into()),
        body: EventBody::Checkpoint(CheckpointPayload {
            turn: 1,
            reason: CheckpointReason::TurnEnd,
            session_status: SessionStatus::Running,
            state_hash: state.state_hash().unwrap(),
            state: tampered,
        }),
    };
    let log = MemoryEventLog::from_events(SessionId("s".into()), redactor, vec![ev]);
    let reader = log.reader();
    assert!(matches!(
        reader.restore_latest(&MigrationRegistry::new()),
        Err(RestoreError::HashMismatch { .. })
    ));
}
