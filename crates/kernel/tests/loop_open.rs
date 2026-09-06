//! P1.2: `Kernel::open` (§7.3) — clean `resumed`, dirty `recovered` with discarded range and
//! waker cancellation, state migration, `on_resume`; ingress redaction (invariant 5).

mod support;

use std::sync::Arc;

use kernel::loop_::{Kernel, KernelError};
use kernel::*;
use serde_json::{Value, json};
use support::*;

/// A second-process log: the same events, a fresh `MemoryEventLog`.
fn reopen_log(setup: &Setup) -> Arc<MemoryEventLog> {
    Arc::new(MemoryEventLog::from_events(
        setup.session_id.clone(),
        setup.redactor.clone(),
        setup.events(),
    ))
}

#[tokio::test]
async fn open_on_a_clean_log_writes_resumed_and_applies_the_cause() {
    let provider = FakeProvider::responses(vec![text_response("first")]);
    let setup = Setup::new(provider);
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("hi"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let ck_hash = k.checkpoint_hash().clone();
    let ck_seq = setup.events().last().unwrap().seq;
    drop(k);

    let log = reopen_log(&setup);
    let hooks: HookLog = Default::default();
    let mut setup2 = Setup::new(FakeProvider::responses(vec![text_response("second")]))
        .middleware(RecordingMiddleware::entry("rec", 200, &hooks))
        .with_log(log.clone());
    setup2.sandbox = FakeSandbox::named("none");
    let mut k = Kernel::open(
        setup2.config(),
        ResumeCause::UserMessage(Message::user_text("again")),
    )
    .await
    .unwrap();
    assert_eq!(k.status(), SessionStatus::Running);
    assert_eq!(k.state().turn, 1);
    assert_eq!(k.state().messages.len(), 3);
    assert_eq!(
        k.state().sandbox_backend,
        "none",
        "volatile fields describe this machine"
    );
    assert_eq!(lock(&hooks).clone(), ["rec:on_resume"]);
    let events = log.events();
    let new: Vec<&str> = events
        .iter()
        .skip(ck_seq as usize + 1)
        .map(|e| e.body.kind())
        .collect();
    assert_eq!(
        new,
        [
            "resumed",
            "middleware_chain_resolved",
            "warning",
            "warning",
            "warning",
            "user_message",
            "checkpoint"
        ]
    );
    assert_eq!(
        warning_classes(&events[ck_seq as usize + 1..]),
        ["sandbox_backend_none", "artifact_store_noop", "memory_noop"]
    );
    let res: Vec<&ResumedPayload> = find_all(&events, |b| match b {
        EventBody::Resumed(p) => Some(p),
        _ => None,
    });
    assert_eq!(res[0].from_checkpoint_hash, ck_hash);
    assert_eq!(res[0].from_checkpoint_seq, ck_seq);
    assert_eq!(res[0].from_status, SessionStatus::Idle);
    assert_eq!(res[0].cause, ResumeCauseKind::UserMessage);
    assert!(res[0].new_process);
    let ums: Vec<&UserMessagePayload> = find_all(&events, |b| match b {
        EventBody::UserMessage(u) => Some(u),
        _ => None,
    });
    assert_eq!(ums[1].applied.at, AppliedPoint::Idle);
    let ck = checkpoints(&events);
    assert_eq!(ck.last().unwrap().reason, CheckpointReason::Resume);
    assert_eq!(ck.last().unwrap().session_status, SessionStatus::Running);
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    assert_eq!(k.state().turn, 2);
    assert_invariant_1(&log.events());
}

#[tokio::test]
async fn open_operator_on_idle_keeps_idle_and_writes_no_checkpoint() {
    let setup = Setup::new(FakeProvider::responses(vec![text_response("x")]));
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("hi"))
        .unwrap();
    k.run().await.unwrap();
    let n = setup.events().len();
    let log = reopen_log(&setup);
    let setup2 = Setup::new(FakeProvider::responses(vec![])).with_log(log.clone());
    let k = Kernel::open(setup2.config(), ResumeCause::Operator)
        .await
        .unwrap();
    assert_eq!(k.status(), SessionStatus::Idle);
    let new: Vec<&str> = log
        .events()
        .iter()
        .skip(n)
        .map(|e| e.body.kind())
        .collect::<Vec<_>>()
        .into_iter()
        .map(|s| s.to_owned())
        .collect::<Vec<_>>()
        .leak()
        .iter()
        .map(|s| s.as_str())
        .collect();
    assert!(!new.contains(&"checkpoint"), "{new:?}");
}

#[tokio::test]
async fn open_on_a_dirty_log_recovers_discards_the_tail_and_cancels_in_process_wakers() {
    // Turn 1 starts an in-process task and an external task; the checkpoint is `running`
    // (Continue), then the process "crashes" after logging a model_request for turn 2.
    let provider = FakeProvider::responses(vec![tool_use_response(vec![
        ("c1", "job", json!({})),
        ("c2", "ext", json!({})),
    ])]);
    let setup = Setup::new(provider)
        .tool(TaskTool::new("job"))
        .tool(TaskTool::external("ext"));
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert!(k.apply_queued_input().await.unwrap());
    assert_eq!(k.run_turn().await.unwrap(), TurnOutcome::Continue);
    let ck_hash = k.checkpoint_hash().clone();
    drop(k);
    let mut events = setup.events();
    assert!(!kernel::log::reader::ends_cleanly(&events));
    let ck_seq = events.last().unwrap().seq;
    // Simulate a crash mid-turn 2: two more lines after the checkpoint.
    for i in 1..=2 {
        events.push(Event {
            seq: ck_seq + i,
            ts: kernel::time::now_rfc3339_ms(),
            session_id: setup.session_id.clone(),
            body: EventBody::Warning(WarningPayload::kernel(2, "test", "mid-turn line", None)),
        });
    }
    let log = Arc::new(MemoryEventLog::from_events(
        setup.session_id.clone(),
        setup.redactor.clone(),
        events,
    ));
    let setup2 =
        Setup::new(FakeProvider::responses(vec![text_response("recovered")])).with_log(log.clone());
    let mut k = Kernel::open(setup2.config(), ResumeCause::Recovery)
        .await
        .unwrap();
    // §7.3 step 6: Running stays Running.
    assert_eq!(k.status(), SessionStatus::Running);
    let job = &k.state().pending_tasks[&TaskId("t1-c1".into())];
    assert_eq!(job.status, TaskStatus::Cancelled);
    let ext = &k.state().pending_tasks[&TaskId("t1-c2".into())];
    assert_eq!(ext.status, TaskStatus::Pending, "external wakers stay open");
    assert!(matches!(
        &k.state().messages.last().unwrap().content[0],
        ContentBlock::TaskResult { task_id, status: TaskStatus::Cancelled, is_error: true, content: ToolResultContent::Json(v), .. }
            if task_id.0 == "t1-c1" && v["error"] == "in-process waker lost in crash"
    ));
    let events = log.events();
    let rec: Vec<&RecoveredPayload> = find_all(&events, |b| match b {
        EventBody::Recovered(p) => Some(p),
        _ => None,
    });
    assert_eq!(rec[0].checkpoint_hash, ck_hash);
    assert_eq!(rec[0].checkpoint_seq, ck_seq);
    assert_eq!(
        rec[0].discarded_seq,
        Some(SeqRange {
            from: ck_seq + 1,
            to: ck_seq + 2
        })
    );
    assert_eq!(rec[0].restored_status, SessionStatus::Running);
    assert_eq!(rec[0].tasks_cancelled, vec![TaskId("t1-c1".into())]);
    let rec_seq = events
        .iter()
        .find(|e| e.body.kind() == "recovered")
        .unwrap()
        .seq;
    assert_eq!(rec_seq, ck_seq + 3, "new events continue at last_seq + 1");
    let new: Vec<&str> = events[rec_seq as usize..]
        .iter()
        .map(|e| e.body.kind())
        .collect();
    assert_eq!(
        new,
        [
            "recovered",
            "middleware_chain_resolved",
            "warning",
            "warning",
            "checkpoint"
        ]
    );
    assert_eq!(
        checkpoints(&events).last().unwrap().reason,
        CheckpointReason::Recovery
    );
    // The effective log hides the discarded lines.
    let effective: Vec<Event> = log.reader().effective().map(|e| e.unwrap()).collect();
    assert!(
        effective
            .iter()
            .all(|e| e.seq <= ck_seq || e.seq >= rec_seq)
    );
    // The session continues: turn 2 runs from the recovery checkpoint.
    assert!(matches!(k.run().await.unwrap(), RunStop::Suspended(_)));
    assert_invariant_1(
        &log.reader()
            .effective()
            .map(|e| e.unwrap())
            .collect::<Vec<_>>(),
    );
}

#[tokio::test]
async fn dirty_log_with_non_recovery_cause_still_recovers_with_null_discard_range() {
    let provider =
        FakeProvider::responses(vec![tool_use_response(vec![("c1", "echo", json!({}))])]);
    let setup = Setup::new(provider).tool(ValueTool::new("echo", json!(1)));
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert!(k.apply_queued_input().await.unwrap());
    assert_eq!(k.run_turn().await.unwrap(), TurnOutcome::Continue);
    drop(k);
    let log = reopen_log(&setup);
    let setup2 = Setup::new(FakeProvider::responses(vec![])).with_log(log.clone());
    let k = Kernel::open(
        setup2.config(),
        ResumeCause::UserMessage(Message::user_text("while you were away")),
    )
    .await
    .unwrap();
    assert_eq!(k.status(), SessionStatus::Running);
    let events = log.events();
    let rec: Vec<&RecoveredPayload> = find_all(&events, |b| match b {
        EventBody::Recovered(p) => Some(p),
        _ => None,
    });
    assert_eq!(rec[0].discarded_seq, None);
    assert!(events.iter().all(|e| e.body.kind() != "resumed"));
    assert_eq!(
        k.state().messages.last().unwrap(),
        &Message::user_text("while you were away")
    );
}

#[tokio::test]
async fn open_errors_no_checkpoint_and_done() {
    let setup = Setup::new(FakeProvider::responses(vec![]));
    let _k = setup.create().await;
    let log = reopen_log(&setup);
    let setup2 = Setup::new(FakeProvider::responses(vec![])).with_log(log);
    assert!(matches!(
        Kernel::open(setup2.config(), ResumeCause::Operator).await,
        Err(KernelError::NoCheckpoint)
    ));
    let setup = Setup::new(FakeProvider::responses(vec![]));
    let mut k = setup.create().await;
    k.end().await.unwrap();
    let log = reopen_log(&setup);
    let setup2 = Setup::new(FakeProvider::responses(vec![])).with_log(log);
    assert!(matches!(
        Kernel::open(setup2.config(), ResumeCause::Operator).await,
        Err(KernelError::Done)
    ));
}

#[tokio::test]
async fn open_from_suspended_with_task_update_resumes_and_from_failed_with_operator() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "ext", json!({}))]),
        text_response("waiting"),
    ]);
    let setup = Setup::new(provider).tool(TaskTool::external("ext"));
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert!(matches!(k.run().await.unwrap(), RunStop::Suspended(_)));
    drop(k);
    let log = reopen_log(&setup);
    let setup2 =
        Setup::new(FakeProvider::responses(vec![text_response("done")])).with_log(log.clone());
    let mut k = Kernel::open(
        setup2.config(),
        ResumeCause::TaskUpdate(TaskUpdate {
            id: TaskId("t1-c1".into()),
            status: TaskStatus::Succeeded,
            outcome: Some(TaskOutcome {
                content: ToolResultContent::Json(json!({"exit": 0})),
                is_error: false,
                artifact_handles: vec![],
            }),
            eta: None,
            check_hint: None,
            source: WakerSource {
                kind: "slurm_epilog".into(),
                trust_tier: None,
                detail: json!({"job": 1}),
            },
        }),
    )
    .await
    .unwrap();
    assert_eq!(k.status(), SessionStatus::Running);
    let events = log.events();
    let res: Vec<&ResumedPayload> = find_all(&events, |b| match b {
        EventBody::Resumed(p) => Some(p),
        _ => None,
    });
    assert_eq!(res[0].cause, ResumeCauseKind::TaskUpdate);
    assert_eq!(res[0].waker.as_ref().unwrap().kind, "slurm_epilog");
    assert_eq!(task_updates(&events)[0].applied.at, AppliedPoint::Suspended);
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);

    // Failed + Operator.
    let setup = Setup::new(FakeProvider::new(vec![Err(ProviderError::Auth(
        "x".into(),
    ))]));
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert!(matches!(k.run().await.unwrap(), RunStop::Failed { .. }));
    drop(k);
    let log = reopen_log(&setup);
    let setup2 =
        Setup::new(FakeProvider::responses(vec![text_response("ok now")])).with_log(log.clone());
    let mut k = Kernel::open(setup2.config(), ResumeCause::Operator)
        .await
        .unwrap();
    assert_eq!(k.status(), SessionStatus::Running);
    assert_eq!(
        checkpoints(&log.events()).last().unwrap().reason,
        CheckpointReason::Resume
    );
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
}

struct BumpV0;

impl StateMigration for BumpV0 {
    fn from_version(&self) -> u32 {
        0
    }
    fn migrate(&self, mut raw: Value) -> Result<Value, MigrationError> {
        raw["schema_version"] = json!(1);
        raw["notebook_path"] = json!("/migrated/notebook.md");
        Ok(raw)
    }
}

#[tokio::test]
async fn open_migrates_an_old_checkpoint_and_logs_state_migrated() {
    let setup = Setup::new(FakeProvider::responses(vec![text_response("x")]));
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("hi"))
        .unwrap();
    k.run().await.unwrap();
    // Rewrite the last checkpoint as a schema_version-0 state, hashed as written.
    let mut events = setup.events();
    let last = events.last_mut().unwrap();
    let EventBody::Checkpoint(ck) = &mut last.body else {
        panic!()
    };
    ck.state.schema_version = 0;
    let raw = serde_json::to_value(&ck.state).unwrap();
    ck.state_hash = kernel::state::state_hash_of_raw(&raw).unwrap();
    let old_hash = ck.state_hash.clone();
    let log = Arc::new(MemoryEventLog::from_events(
        setup.session_id.clone(),
        setup.redactor.clone(),
        events,
    ));
    let mut migrations = MigrationRegistry::new();
    migrations.register(Box::new(BumpV0)).unwrap();
    let setup2 = Setup::new(FakeProvider::responses(vec![])).with_log(log.clone());
    *lock(&setup2.migrations) = migrations;
    let k = Kernel::open(setup2.config(), ResumeCause::Operator)
        .await
        .unwrap();
    assert_eq!(k.state().schema_version, STATE_SCHEMA_VERSION);
    assert_eq!(
        k.state().notebook_path.as_deref(),
        Some(std::path::Path::new("/migrated/notebook.md"))
    );
    assert_eq!(k.status(), SessionStatus::Idle);
    let events = log.events();
    let w = warnings(&events);
    let mig = w
        .iter()
        .find(|w| w.class == "state_migrated")
        .expect("state_migrated warning");
    assert_eq!(mig.detail, Some(json!({"from": 0, "to": 1})));
    let ck = checkpoints(&events);
    let fresh = ck.last().unwrap();
    assert_eq!(fresh.reason, CheckpointReason::Resume);
    assert_eq!(fresh.state.schema_version, 1);
    assert_ne!(fresh.state_hash, old_hash);
    assert_eq!(fresh.state_hash, k.state().state_hash().unwrap());
    // Without the migration, open fails loudly.
    let setup3 = Setup::new(FakeProvider::responses(vec![])).with_log(reopen_log(&setup));
    let mut events = setup.events();
    let EventBody::Checkpoint(ck) = &mut events.last_mut().unwrap().body else {
        panic!()
    };
    ck.state.schema_version = 0;
    ck.state_hash =
        kernel::state::state_hash_of_raw(&serde_json::to_value(&ck.state).unwrap()).unwrap();
    let log3 = Arc::new(MemoryEventLog::from_events(
        setup.session_id.clone(),
        setup.redactor.clone(),
        events,
    ));
    let setup3 = setup3.with_log(log3);
    assert!(matches!(
        Kernel::open(setup3.config(), ResumeCause::Operator).await,
        Err(KernelError::Restore(RestoreError::Migration(
            MigrationError::MissingStep { from: 0, .. }
        )))
    ));
}

#[tokio::test]
async fn ingress_redaction_precedes_hashing_spill_and_state() {
    let secret = "hunter2-super-secret-value";
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "leak", json!({"token": secret}))]),
        text_response(&format!("the key is {secret}")),
    ]);
    let leak = ValueTool::new(
        "leak",
        json!({"env": format!("API_TOKEN={secret}"), "raw": secret}),
    );
    let mut after_tool_saw = FnMiddleware::empty();
    let seen: Arc<std::sync::Mutex<Option<ToolOutput>>> = Default::default();
    let seen2 = seen.clone();
    after_tool_saw.after_tool = Some(Box::new(move |_, _, out, _| {
        *lock(&seen2) = Some(out.clone());
        Ok(())
    }));
    let setup = Setup::new(provider.clone())
        .tool(leak)
        .middleware(FnMiddleware::entry("peek", 200, after_tool_saw));
    setup.redactor.register_secret(&SecretString::new(secret));
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text(format!("use {secret}")))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    // The secret is nowhere: not in state, not in the log, not in what after_tool saw, not in
    // the model's next request.
    let state_text = serde_json::to_string(k.state()).unwrap();
    assert!(!state_text.contains(secret));
    let events = setup.events();
    let log_text = serde_json::to_string(&events).unwrap();
    assert!(!log_text.contains(secret));
    assert!(
        !serde_json::to_string(lock(&seen).as_ref().unwrap())
            .unwrap()
            .contains(secret)
    );
    assert!(
        !serde_json::to_string(&provider.requests()[1])
            .unwrap()
            .contains(secret)
    );
    // Hashes are of redacted content.
    let res = tool_results(&events)[0];
    assert_eq!(
        res.content,
        ToolResultContent::Json(
            json!({"env": "API_TOKEN=[REDACTED:secret]", "raw": "[REDACTED:secret]"})
        )
    );
    assert_eq!(
        res.result_hash,
        Hash::of_canonical_json(&json!({"content": {"json": {"env": "API_TOKEN=[REDACTED:secret]", "raw": "[REDACTED:secret]"}}, "is_error": false})).unwrap()
    );
    let ck = checkpoints(&events).last().copied().unwrap();
    assert_eq!(ck.state_hash, k.state().state_hash().unwrap());
    assert_eq!(
        ck.state_hash,
        kernel::state::state_hash_of_raw(&serde_json::to_value(&ck.state).unwrap()).unwrap()
    );
    // The writer's second pass found nothing to do.
    assert!(!warning_classes(&events).contains(&"late_redaction".to_owned()));
    // The redacted log restores and re-hashes cleanly.
    let log = reopen_log(&setup);
    let setup2 = Setup::new(FakeProvider::responses(vec![])).with_log(log);
    let k2 = Kernel::open(setup2.config(), ResumeCause::Operator)
        .await
        .unwrap();
    assert_eq!(k2.state().messages, k.state().messages);
}
