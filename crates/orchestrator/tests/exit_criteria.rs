//! Phase 1 exit criteria (docs/IMPLEMENTATION_PLAN.md), run end to end against a real kernel,
//! the native host, the `None` sandbox backend (no `bwrap` in CI), the six base tools, and a
//! file-backed log:
//!
//! 1. a coding session (read / edit / bash on a real checkout) is recorded;
//! 2. a `run_script` session (start, suspend, process-exit waker, resume, finish) is recorded;
//! 3. a session reaches `failed` on provider exhaustion and resumes from its checkpoint.
//!
//! The replay half of criteria 1 and 2 (`ReplayProvider` + `diff-logs`) is in `replay_criteria.rs`
//! once P1.4 lands; these tests keep their recorded logs so that file can reuse the shape.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ::host::{MapSecretSource, NativeHost};
use ::sandbox::{NoneBackend, base_tools};
use async_trait::async_trait;
use kernel::event::CheckpointReason;
use kernel::log::FileEventLog;
use kernel::*;
use serde_json::{Value, json};

// ---- a scripted provider (the "model") --------------------------------------------------------

struct Scripted {
    script: Mutex<VecDeque<Result<ModelResponse, ProviderError>>>,
    calls: Mutex<usize>,
}

impl Scripted {
    fn new(script: Vec<Result<ModelResponse, ProviderError>>) -> Arc<Scripted> {
        Arc::new(Scripted {
            script: Mutex::new(script.into()),
            calls: Mutex::new(0),
        })
    }
    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl Provider for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }
    async fn complete(&self, _req: ModelRequest) -> Result<ModelResponse, ProviderError> {
        *self.calls.lock().unwrap() += 1;
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(text("(script exhausted)")))
    }
}

fn response(content: Vec<ContentBlock>, stop: StopReason) -> ModelResponse {
    let raw = serde_json::to_vec(&content).unwrap();
    ModelResponse {
        content,
        stop_reason: stop,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        },
        model_id: "scripted".into(),
        raw_response_hash: Hash::of_bytes(&raw),
        response_id: None,
    }
}

fn text(t: &str) -> ModelResponse {
    response(
        vec![ContentBlock::Text { text: t.into() }],
        StopReason::EndTurn,
    )
}

fn call(id: &str, name: &str, input: Value) -> ModelResponse {
    response(
        vec![
            ContentBlock::Text {
                text: format!("Calling {name}."),
            },
            ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input,
            },
        ],
        StopReason::ToolUse,
    )
}

// ---- kernel assembly ----------------------------------------------------------------------------

struct Session {
    workdir: PathBuf,
    log_path: PathBuf,
    redactor: Arc<Redactor>,
    host: Arc<NativeHost>,
    sandbox: Arc<NoneBackend>,
    provider: Arc<Scripted>,
}

impl Session {
    fn new(dir: &Path, provider: Arc<Scripted>) -> Session {
        let workdir = dir.join("repo").canonicalize().unwrap();
        let redactor = Arc::new(Redactor::new());
        let host = Arc::new(NativeHost::new(
            redactor.clone(),
            Arc::new(MapSecretSource::empty()),
        ));
        let sandbox = Arc::new(NoneBackend::new(host.clone()));
        Session {
            workdir,
            log_path: dir.join("session.jsonl"),
            redactor,
            host,
            sandbox,
            provider,
        }
    }

    fn grants(&self) -> Vec<Capability> {
        let mut g: Vec<Capability> = [
            format!("fs.rw:{}", self.workdir.display()),
            "proc:bash".into(),
            "proc:python3".into(),
        ]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
        for t in ["read", "write", "edit", "bash", "run_script", "python"] {
            g.push(format!("tool:{t}").parse().unwrap());
        }
        g
    }

    fn config(&self, log: Arc<dyn EventLog>, retry: RetryPolicy) -> KernelConfig {
        let tools = base_tools(&self.workdir, self.sandbox.clone());
        KernelConfig {
            tools,
            middleware: vec![],
            provider: self.provider.clone(),
            host: self.host.clone(),
            artifact_store: Arc::new(NoopArtifactStore),
            memory: Arc::new(NoopMemory),
            sandbox: self.sandbox.clone(),
            event_log: log,
            redactor: self.redactor.clone(),
            spill: SpillConfig::default(),
            retry,
            sandbox_limits: SandboxLimits {
                timeout: Duration::from_secs(20),
                ..SandboxLimits::default()
            },
            migrations: MigrationRegistry::new(),
            model_id: "scripted".into(),
            model_params: ModelParams::default(),
            system_prompt: vec![PromptBlock::new(
                PromptBlockKind::Role,
                "default",
                "You work in the repository mounted at the working directory.",
            )],
            grants: self.grants(),
            delta_sink: None,
            event_channel_capacity: 256,
        }
    }

    fn init(&self) -> SessionInit {
        let h = |b: &str| Hash::parse(&format!("b3:{}", b.repeat(32))).unwrap();
        SessionInit {
            session_id: SessionId("s_exit".into()),
            profiles: ActiveProfiles {
                model_profile_hash: h("1a"),
                agent_profile_hash: h("2b"),
                resolved_profile_hash: h("3c"),
                project_profile_hash: None,
                bundles_hash: None,
            },
            profile_loads: vec![],
            runtime_overrides: None,
            notebook_path: None,
            memory: None,
        }
    }

    async fn create_log(&self) -> Arc<FileEventLog> {
        Arc::new(
            FileEventLog::create(
                &self.log_path,
                SessionId("s_exit".into()),
                self.redactor.clone(),
            )
            .await
            .unwrap(),
        )
    }
}

fn fast_retry(max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
        multiplier: 2.0,
        jitter: false,
        request_timeout: Duration::from_secs(5),
    }
}

fn make_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/greet.py"),
        "def greet(name):\n    return 'hello ' + name\n\nprint(greet('world'))\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("job.sh"),
        "#!/usr/bin/env bash\nsleep 0.2\necho \"job output: $1\"\nexit 0\n",
    )
    .unwrap();
    repo
}

fn payload_kinds(events: &[Event]) -> Vec<&str> {
    events.iter().map(|e| e.body.kind()).collect()
}

fn tool_results(events: &[Event]) -> Vec<&kernel::event::ToolResultPayload> {
    events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::ToolResult(p) => Some(p),
            _ => None,
        })
        .collect()
}

// ---- criterion 1: a coding session on a real checkout ------------------------------------------

#[tokio::test]
async fn coding_session_read_edit_bash_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let repo = make_repo(dir.path());
    let provider = Scripted::new(vec![
        Ok(call("c1", "read", json!({"path": "src/greet.py"}))),
        Ok(call(
            "c2",
            "edit",
            json!({"path": "src/greet.py", "old_string": "'hello '", "new_string": "'hi, '"}),
        )),
        Ok(call(
            "c3",
            "bash",
            json!({"command": "python3 src/greet.py"}),
        )),
        Ok(text(
            "Changed the greeting; the script now prints `hi, world`.",
        )),
    ]);
    let s = Session::new(dir.path(), provider.clone());
    let log = s.create_log().await;
    let mut k = Kernel::create(s.config(log.clone(), fast_retry(2)), s.init())
        .await
        .unwrap();
    k.handle()
        .enqueue_user_message(Message::user_text(
            "Change the greeting in src/greet.py from hello to hi and run it.",
        ))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    assert_eq!(k.state().turn, 4);
    assert_eq!(provider.calls(), 4);

    // The checkout really changed and bash really ran in it.
    let edited = std::fs::read_to_string(repo.join("src/greet.py")).unwrap();
    assert!(edited.contains("'hi, '"));
    let events = log.events();
    let results = tool_results(&events);
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|r| !r.is_error), "{results:?}");
    let ToolResultContent::Json(bash) = &results[2].content else {
        panic!()
    };
    assert_eq!(bash["exit_code"], 0);
    assert_eq!(bash["stdout"].as_str().unwrap().trim(), "hi, world");
    let ToolResultContent::Json(read) = &results[0].content else {
        panic!()
    };
    assert!(read["content"].as_str().unwrap().contains("def greet"));

    // The log is complete and ends cleanly (idle checkpoint), ready for replay.
    let kinds = payload_kinds(&events);
    assert_eq!(kinds[0], "log_opened");
    assert_eq!(kinds[1], "session_created");
    assert_eq!(kinds.iter().filter(|k| **k == "model_request").count(), 4);
    assert_eq!(kinds.iter().filter(|k| **k == "tool_call").count(), 3);
    assert_eq!(*kinds.last().unwrap(), "checkpoint");
    let snap = FileEventLog::snapshot(&s.log_path).unwrap();
    let effective: Vec<Event> = snap.effective().map(Result::unwrap).collect();
    assert!(kernel::log::reader::ends_cleanly(&effective));
    let warnings: Vec<String> = events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::Warning(w) => Some(w.class.clone()),
            _ => None,
        })
        .collect();
    assert!(warnings.contains(&"sandbox_backend_none".to_owned()), "D14");
}

// ---- criterion 2: run_script start → suspend → process-exit waker → resume → finish -----------

#[tokio::test]
async fn run_script_session_suspends_and_the_process_exit_waker_resumes_it() {
    let dir = tempfile::tempdir().unwrap();
    let _repo = make_repo(dir.path());
    let provider = Scripted::new(vec![
        Ok(call(
            "c1",
            "run_script",
            json!({"path": "job.sh", "args": ["alpha"]}),
        )),
        Ok(text("Started the job; I'll wait for it.")),
        Ok(text("The job printed `job output: alpha`. Done.")),
    ]);
    let s = Session::new(dir.path(), provider.clone());
    let log = s.create_log().await;
    let mut k = Kernel::create(s.config(log.clone(), fast_retry(2)), s.init())
        .await
        .unwrap();
    k.handle()
        .enqueue_user_message(Message::user_text("Run job.sh with alpha and report."))
        .unwrap();
    // Turn 1 starts the task (Continue); turn 2 has no tool calls while the task is open → suspend.
    let RunStop::Suspended(sus) = k.run().await.unwrap() else {
        panic!("expected suspension")
    };
    assert_eq!(sus.pending_task_ids, vec![TaskId("t1-c1".into())]);
    assert_eq!(
        sus.in_process_wakers, 1,
        "the D1 process-exit waker lives here"
    );
    assert_eq!(k.status(), SessionStatus::Suspended);
    // No polling: `run` awaits the inbox; the waker delivers the outcome when the script exits.
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    assert_eq!(k.state().turn, 3);
    let task = &k.state().pending_tasks[&TaskId("t1-c1".into())];
    assert_eq!(task.status, TaskStatus::Succeeded);
    assert_eq!(task.completed_turn, Some(2));
    let out = task.outcome.as_ref().unwrap();
    let ToolResultContent::Json(v) = &out.content else {
        panic!()
    };
    assert_eq!(v["exit_code"], 0);
    assert_eq!(v["stdout"].as_str().unwrap().trim(), "job output: alpha");
    assert!(!out.is_error);
    // The synthetic TaskResult reached the model before its final turn.
    let last_req_messages = k.state().messages.len();
    assert!(k.state().messages.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::TaskResult { task_id, .. } if task_id.0 == "t1-c1"))
    }));
    assert!(last_req_messages >= 6);
    let events = log.events();
    let kinds = payload_kinds(&events);
    let want = [
        "task_started",
        "checkpoint",
        "model_request",
        "model_response",
        "checkpoint",
        "suspended",
        "task_update",
        "checkpoint",
        "model_request",
    ];
    let start = kinds.iter().position(|k| *k == "task_started").unwrap();
    assert_eq!(&kinds[start..start + want.len()], &want);
    let update = events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::TaskUpdate(p) => Some(p),
            _ => None,
        })
        .unwrap();
    assert_eq!(update.waker.kind, "in_process_exit");
    assert_eq!(update.applied.at, kernel::event::AppliedPoint::Suspended);
}

// ---- criterion 3: provider exhaustion → failed → resume from the checkpoint -------------------

#[tokio::test]
async fn provider_exhaustion_fails_the_session_and_it_resumes_from_its_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let _repo = make_repo(dir.path());
    let rate_limited = || {
        Err(ProviderError::RateLimited {
            retry_after: Some(Duration::from_millis(1)),
        })
    };
    let provider = Scripted::new(vec![
        Ok(call("c1", "read", json!({"path": "job.sh"}))),
        rate_limited(),
        rate_limited(),
        rate_limited(),
        Ok(text(
            "Recovered after the outage; job.sh sleeps then echoes.",
        )),
    ]);
    let s = Session::new(dir.path(), provider.clone());
    let log = s.create_log().await;
    let mut k = Kernel::create(s.config(log.clone(), fast_retry(3)), s.init())
        .await
        .unwrap();
    k.handle()
        .enqueue_user_message(Message::user_text("What does job.sh do?"))
        .unwrap();
    let stop = k.run().await.unwrap();
    assert_eq!(
        stop,
        RunStop::Failed {
            error_class: "provider_rate_limited".into()
        }
    );
    assert_eq!(k.status(), SessionStatus::Failed);
    assert_eq!(provider.calls(), 4, "1 success + 3 attempts");
    let events = log.events();
    let kinds = payload_kinds(&events);
    assert_eq!(kinds.iter().filter(|k| **k == "provider_retry").count(), 2);
    let tail: Vec<&str> = kinds[kinds.len() - 3..].to_vec();
    assert_eq!(tail, ["turn_failed", "checkpoint", "session_failed"]);
    let failed_ck = events
        .iter()
        .rev()
        .find_map(|e| match &e.body {
            EventBody::Checkpoint(c) => Some(c),
            _ => None,
        })
        .unwrap();
    assert_eq!(failed_ck.reason, CheckpointReason::Failure);
    assert_eq!(failed_ck.session_status, SessionStatus::Failed);
    // Checkpoint intact: the conversation up to the successful turn is there, nothing partial.
    assert_eq!(failed_ck.state.messages.len(), 3, "user, assistant, tool");
    let before = k.state().messages.clone();

    // Resume in-process from the checkpoint: the failed turn is retried and succeeds.
    k.resume(ResumeCause::Operator).await.unwrap();
    assert_eq!(k.status(), SessionStatus::Running);
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    assert_eq!(k.state().messages[..3], before[..3]);
    assert_eq!(k.state().messages.len(), 4);

    // And cross-process: the file restores to the failed checkpoint and resumes the same way.
    drop(k);
    drop(log);
    let reopened = FileEventLog::snapshot(&s.log_path).unwrap();
    let restored = reopened
        .restore_latest(&MigrationRegistry::new())
        .unwrap()
        .unwrap();
    assert_eq!(restored.state.session_status, SessionStatus::Idle);
}
