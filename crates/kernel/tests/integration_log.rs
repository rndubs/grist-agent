//! P1.3 kernel-level tests on the file-backed log: run → suspend → resume in a new process
//! produces the same conversation as an uninterrupted run, and crash recovery discards the
//! events after the last checkpoint.

mod support;

use std::sync::Arc;

use kernel::event::{AppliedPoint, CheckpointReason};
use kernel::log::FileEventLog;
use kernel::*;
use serde_json::{json, Value};
use support::*;

fn outcome(v: Value, is_error: bool) -> TaskOutcome {
    TaskOutcome {
        content: ToolResultContent::Json(v),
        is_error,
        artifact_handles: vec![],
    }
}

/// The comparable projection of a log: kinds and hashed payload fields, envelope stripped,
/// `checkpoint` reduced to `(state_hash, session_status)` (the reason differs between the
/// in-process waker path and the `open` path), `resumed`/`recovered`/`warning`/`log_opened`
/// dropped (they say *how* the session ran, not *what* the conversation is).
fn projection(events: &[Event]) -> Vec<Value> {
    events
        .iter()
        .filter_map(|e| {
            let payload = e.body.payload_value().unwrap();
            let kind = e.body.kind();
            match kind {
                "resumed" | "recovered" | "warning" | "log_opened" | "session_created"
                | "profile_load" | "middleware_chain_resolved" => None,
                "checkpoint" => Some(json!({
                    "kind": kind,
                    "state_hash": payload["state_hash"],
                    "session_status": payload["session_status"],
                })),
                "task_update" => Some(json!({
                    "kind": kind,
                    "task_id": payload["task_id"],
                    "status": payload["status"],
                    "outcome": payload["outcome"],
                    "applied": payload["applied"],
                })),
                "tool_result" => Some(json!({
                    "kind": kind,
                    "result_hash": payload["result_hash"],
                    "content": payload["content"],
                    "is_error": payload["is_error"],
                })),
                _ => {
                    let mut p = payload.clone();
                    if let Some(o) = p.as_object_mut() {
                        for v in ["attempts", "duration_ms", "policy_hash", "waker"] {
                            o.remove(v);
                        }
                    }
                    Some(json!({"kind": kind, "payload": p}))
                }
            }
        })
        .collect()
}

fn scripted_provider() -> Arc<FakeProvider> {
    FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "job", json!({"n": 1}))]),
        text_response("Started the job; waiting."),
        text_response("The job finished; all done."),
    ])
}

#[tokio::test]
async fn run_suspend_resume_in_a_new_process_matches_an_uninterrupted_run() {
    let dir = tempfile::tempdir().unwrap();

    // ---- A: uninterrupted (in-process waker completes the task while suspended) --------------
    let provider_a = scripted_provider();
    let job_a = TaskTool::new("job");
    let setup_a = Setup::new(provider_a.clone()).tool(job_a.clone());
    let redactor_a = setup_a.redactor.clone();
    let log_a = Arc::new(
        FileEventLog::create(&dir.path().join("a.jsonl"), setup_a.session_id.clone(), redactor_a)
            .await
            .unwrap(),
    );
    let mut config = setup_a.config();
    config.event_log = log_a.clone();
    let mut ka = Kernel::create(config, setup_a.init()).await.unwrap();
    ka.handle()
        .enqueue_user_message(Message::user_text("run the job"))
        .unwrap();
    assert!(matches!(ka.run().await.unwrap(), RunStop::Suspended(_)));
    let task_outcome = outcome(json!({"exit_code": 0, "lines": 3}), false);
    job_a.complete(task_outcome.clone());
    // The in-process waker delivers the update; `run` resumes and finishes the turn.
    assert_eq!(ka.run().await.unwrap(), RunStop::Idle);
    let final_a = ka.state().clone();
    let events_a = log_a.events();

    // ---- B: interrupted (process exits at suspension; a new process opens the log) ----------
    let provider_b = scripted_provider();
    let job_b = TaskTool::new("job");
    let setup_b = Setup::new(provider_b.clone()).tool(job_b.clone());
    let redactor_b = setup_b.redactor.clone();
    let path_b = dir.path().join("b.jsonl");
    let log_b = Arc::new(
        FileEventLog::create(&path_b, setup_b.session_id.clone(), redactor_b.clone())
            .await
            .unwrap(),
    );
    let mut config = setup_b.config();
    config.event_log = log_b.clone();
    let mut kb = Kernel::create(config, setup_b.init()).await.unwrap();
    kb.handle()
        .enqueue_user_message(Message::user_text("run the job"))
        .unwrap();
    let RunStop::Suspended(s) = kb.run().await.unwrap() else {
        panic!("expected suspension")
    };
    assert_eq!(s.pending_task_ids, vec![TaskId("t1-c1".into())]);
    drop(kb);
    drop(log_b);

    // New process: reopen the file and deliver the task completion as the resume cause.
    let reopened = Arc::new(FileEventLog::open(&path_b, redactor_b).await.unwrap());
    assert!(
        reopened.reader().effective().count() > 0,
        "the log has the recorded events"
    );
    let mut config = setup_b.config();
    config.event_log = reopened.clone();
    let update = TaskUpdate {
        id: TaskId("t1-c1".into()),
        status: TaskStatus::Succeeded,
        outcome: Some(task_outcome),
        eta: None,
        check_hint: None,
        source: WakerSource::in_process_exit(json!({"pid": 4242})),
    };
    let mut kb2 = Kernel::open(config, ResumeCause::TaskUpdate(update))
        .await
        .unwrap();
    assert_eq!(kb2.status(), SessionStatus::Running);
    assert_eq!(kb2.run().await.unwrap(), RunStop::Idle);
    let final_b = kb2.state().clone();
    let events_b = reopened.events();

    // ---- Same conversation, same hashes -------------------------------------------------------
    assert_eq!(final_a.state_hash().unwrap(), final_b.state_hash().unwrap());
    assert_eq!(final_a.messages, final_b.messages);
    assert_eq!(final_a.pending_tasks, final_b.pending_tasks);
    assert_eq!(projection(&events_a), projection(&events_b));
    // B has exactly one `resumed`, A none; both have one `suspended`.
    assert_eq!(kinds(&events_a).iter().filter(|k| **k == "resumed").count(), 0);
    assert_eq!(kinds(&events_b).iter().filter(|k| **k == "resumed").count(), 1);
    let ups = task_updates(&events_b);
    assert_eq!(ups.len(), 1);
    assert_eq!(ups[0].applied.at, AppliedPoint::Suspended);
    // The on-disk log restores to the final state.
    let restored = reopened
        .reader()
        .restore_latest(&MigrationRegistry::new())
        .unwrap()
        .unwrap();
    assert_eq!(restored.state, final_b);
}

#[tokio::test]
async fn crash_mid_turn_recovers_from_the_last_checkpoint_and_discards_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "echo", json!({"x": 1}))]),
        text_response("done"),
    ]);
    let echo = ValueTool::new("echo", json!({"ok": true}));
    let setup = Setup::new(provider.clone()).tool(echo.clone());
    let redactor = setup.redactor.clone();
    let log = Arc::new(
        FileEventLog::create(&path, setup.session_id.clone(), redactor.clone())
            .await
            .unwrap(),
    );
    let mut config = setup.config();
    config.event_log = log.clone();
    let mut k = Kernel::create(config, setup.init()).await.unwrap();
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    k.apply_queued_input().await.unwrap();
    // Turn 1: tool call → Continue; the turn_end checkpoint says `running`.
    assert_eq!(k.run_turn().await.unwrap(), TurnOutcome::Continue);
    let ck_hash = k.checkpoint_hash().clone();
    let ck_seq = log.last_seq().unwrap();
    // Simulate the crash: the process dies after starting turn 2 (a `model_request` was
    // written, no response, no checkpoint).
    let stray = kernel::event::ModelRequestPayload {
        turn: 2,
        request_hash: h("aa"),
        checkpoint_hash: ck_hash.clone(),
        model_id: "fake-model".into(),
        profiles: profiles(),
        system_prompt_hash: h("bb"),
        prompt_blocks: vec![],
        message_count: 3,
        tool_names: vec!["echo".into()],
        params_hash: h("cc"),
    };
    log.append(EventBody::ModelRequest(stray)).await.unwrap();
    let stray_seq = log.last_seq().unwrap();
    drop(k);
    drop(log);

    // New process.
    let reopened = Arc::new(FileEventLog::open(&path, redactor).await.unwrap());
    assert!(
        !reopened.reader().effective().count().eq(&0),
        "log has content"
    );
    let mut config = setup.config();
    config.event_log = reopened.clone();
    let mut k2 = Kernel::open(config, ResumeCause::Recovery).await.unwrap();
    assert_eq!(k2.status(), SessionStatus::Running, "restored mid-conversation");
    assert_eq!(k2.checkpoint_hash(), &ck_hash);
    let events = reopened.events();
    let rec = find_all(&events, |b| match b {
        EventBody::Recovered(p) => Some(p.clone()),
        _ => None,
    });
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].checkpoint_hash, ck_hash);
    assert_eq!(rec[0].checkpoint_seq, ck_seq);
    let range = rec[0].discarded_seq.unwrap();
    assert_eq!((range.from, range.to), (ck_seq + 1, stray_seq));
    assert_eq!(rec[0].restored_status, SessionStatus::Running);
    // The stray request is hidden from the effective log but still on disk.
    let reader = reopened.reader();
    assert!(reader.iter().any(|e| e.unwrap().seq == stray_seq));
    assert!(!reader.effective().any(|e| e.unwrap().seq == stray_seq));
    // The session continues: turn 2 runs from the restored state and goes idle.
    assert_eq!(k2.run().await.unwrap(), RunStop::Idle);
    assert_eq!(k2.state().turn, 2);
    let all = reopened.events();
    let cks = checkpoints(&all);
    assert_eq!(cks.last().unwrap().reason, CheckpointReason::TurnEnd);
    assert_eq!(cks.last().unwrap().session_status, SessionStatus::Idle);
    // Only the recorded turn-1 tool call ever ran; turn 2 was text only.
    assert_eq!(echo.call_count(), 1);
    assert_eq!(provider.requests().len(), 2, "one live request per process");
}

#[tokio::test]
async fn a_cleanly_suspended_log_resumes_and_a_secret_never_reaches_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let secret = "sk-live-veryveryverysecretvalue123";
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "leak", json!({}))]),
        text_response("ok"),
    ]);
    let leak = ValueTool::new("leak", json!({"token": secret, "note": "leaked"}));
    let setup = Setup::new(provider).tool(leak);
    let redactor = setup.redactor.clone();
    redactor.register_secret(&SecretString::new(secret));
    let log = Arc::new(
        FileEventLog::create(&path, setup.session_id.clone(), redactor.clone())
            .await
            .unwrap(),
    );
    let mut config = setup.config();
    config.event_log = log.clone();
    let mut k = Kernel::create(config, setup.init()).await.unwrap();
    k.handle()
        .enqueue_user_message(Message::user_text("leak it"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    // The model saw the redacted value, and the file never contains the secret.
    let msgs = &k.state().messages;
    let text = serde_json::to_string(msgs).unwrap();
    assert!(text.contains("[REDACTED:secret]"));
    assert!(!text.contains(secret));
    drop(k);
    drop(log);
    let bytes = std::fs::read(&path).unwrap();
    assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()));
    assert!(String::from_utf8_lossy(&bytes).contains("[REDACTED:secret]"));
    // No late_redaction warning: ingress redaction caught it.
    let snap = FileEventLog::snapshot(&path).unwrap();
    assert!(!snap
        .iter()
        .any(|e| matches!(e.unwrap().body, EventBody::Warning(w) if w.class == "late_redaction")));
    assert!(kernel::log::reader::ends_cleanly(
        &snap.effective().map(Result::unwrap).collect::<Vec<_>>()
    ));
}
