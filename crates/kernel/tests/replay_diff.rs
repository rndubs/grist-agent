//! P1.4: `diff_logs` (D16, `event-schema.md` §5.3) — volatile kinds and fields are ignored,
//! divergences in payload, kind, and length are reported — and the `diff-logs` binary.

mod support;

use std::sync::Arc;

use kernel::log::writer::envelope_line;
use kernel::replay::*;
use kernel::*;
use serde_json::json;
use support::*;

async fn recorded_session() -> Vec<Event> {
    let rec = Arc::new(Recorder::new());
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "job", json!({}))]),
        text_response("waiting"),
        text_response("done"),
    ]);
    let job = TaskTool::new("job");
    let setup = Setup::new(provider)
        .tool(job.clone())
        .middleware(Recorder::entry(rec.clone()));
    let mut k = setup.create().await;
    rec.attach(&k.handle());
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert!(matches!(k.run().await.unwrap(), RunStop::Suspended(_)));
    job.complete(TaskOutcome {
        content: ToolResultContent::Json(json!({"exit_code": 0})),
        is_error: false,
        artifact_handles: vec![],
    });
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    setup.events()
}

fn log_of(events: &[Event]) -> MemoryEventLog {
    MemoryEventLog::from_events(
        SessionId("x".into()),
        Arc::new(Redactor::new()),
        events.to_vec(),
    )
}

fn diff(a: &[Event], b: &[Event]) -> DiffReport {
    diff_logs(&*log_of(a).reader(), &*log_of(b).reader()).unwrap()
}

fn warning(seq: u64) -> Event {
    Event {
        seq,
        ts: "2026-09-06T00:00:00.000Z".into(),
        session_id: SessionId("other".into()),
        body: EventBody::Warning(WarningPayload::kernel(1, "noise", "ignored", None)),
    }
}

fn reseq(events: &mut [Event]) {
    for (i, e) in events.iter_mut().enumerate() {
        e.seq = i as u64;
    }
}

#[tokio::test]
async fn every_volatile_field_and_kind_is_ignored() {
    let recorded = recorded_session().await;
    let mut mutated = recorded.clone();
    let mut touched = Vec::new();
    for e in &mut mutated {
        // Envelope.
        e.ts = "1999-01-01T00:00:00.000Z".into();
        e.session_id = SessionId("s_other".into());
        match &mut e.body {
            EventBody::LogOpened(p) => {
                p.kernel_version = "9.9.9".into();
                p.mode = LogMode::Replay;
                touched.push("log_opened");
            }
            EventBody::SessionCreated(p) => {
                p.session_id = SessionId("s_other".into());
                p.created_at = "1999".into();
                p.kernel_version = "9.9.9".into();
                p.sandbox_backend = "bwrap".into();
                p.sandbox_policy_hash = h("ee");
                p.artifact_store = "fs".into();
                p.memory = "sqlite".into();
                p.provider = "replay".into();
                touched.push("session_created");
            }
            EventBody::ProfileLoad(p) => {
                p.path = Some("/elsewhere/model.toml".into());
                touched.push("profile_load");
            }
            EventBody::ModelResponse(p) => {
                p.attempts = 7;
                touched.push("model_response");
            }
            EventBody::ToolCall(p) => {
                p.policy_hash = Some(h("dd"));
                touched.push("tool_call");
            }
            EventBody::ToolResult(p) => {
                p.duration_ms = 123_456;
                touched.push("tool_result");
            }
            EventBody::TaskUpdate(p) => {
                p.waker = WakerSource {
                    kind: "replay".into(),
                    trust_tier: Some(TrustTier::Inbound),
                    detail: json!({"recorded_kind": "in_process_exit"}),
                };
                touched.push("task_update");
            }
            EventBody::Checkpoint(p) => {
                p.state.session_id = SessionId("s_other".into());
                p.state.created_at = "1999".into();
                p.state.sandbox_policy_hash = h("cc");
                p.state.sandbox_backend = "bwrap".into();
                touched.push("checkpoint");
            }
            _ => {}
        }
    }
    for kind in [
        "log_opened",
        "session_created",
        "profile_load",
        "model_response",
        "tool_call",
        "tool_result",
        "task_update",
        "checkpoint",
    ] {
        assert!(touched.contains(&kind), "scenario lacks `{kind}`");
    }
    // Whole volatile kinds: a `warning`, a `provider_retry`, and a `recovered` inserted anywhere.
    mutated.insert(3, warning(0));
    mutated.push(warning(0));
    let mut retry = warning(0);
    retry.body = EventBody::ProviderRetry(ProviderRetryPayload {
        turn: 1,
        attempt: 1,
        error_class: "provider_transport".into(),
        message: "x".into(),
        delay_ms: 5,
    });
    mutated.insert(8, retry);
    let mut recovered = warning(0);
    recovered.body = EventBody::Recovered(RecoveredPayload {
        checkpoint_hash: h("aa"),
        checkpoint_seq: 1,
        discarded_seq: None,
        restored_status: SessionStatus::Idle,
        tasks_cancelled: vec![],
        kernel_version: "9".into(),
        event_schema_version: EVENT_SCHEMA_VERSION,
    });
    mutated.insert(5, recovered);
    reseq(&mut mutated);
    let report = diff(&recorded, &mutated);
    assert!(report.identical, "{:?}", report.first_diff);
    assert_eq!(
        report.compared,
        recorded
            .iter()
            .filter(|e| !VOLATILE_KINDS.contains(&e.body.kind()))
            .count()
    );
    assert!(report.compared > 10);
    // Symmetric.
    assert!(diff(&mutated, &recorded).identical);
    // The `resumed`, `turn_failed`, `session_failed`, `spawn`, `child_completed` rows of the table.
    for (kind, field) in [
        ("resumed", "waker"),
        ("resumed", "new_process"),
        ("resumed", "kernel_version"),
        ("turn_failed", "attempts"),
        ("turn_failed", "message"),
        ("session_failed", "cause_seq"),
        ("spawn", "child_session_id"),
        ("spawn", "child_log_path"),
        ("child_completed", "child_session_id"),
    ] {
        let mut v = json!({ field: "x", "kept": 1 });
        strip_volatile(kind, &mut v);
        assert_eq!(v, json!({"kept": 1}), "{kind}.{field}");
    }
    let mut v = json!({"state": {"session_id": "a", "turn": 3}, "state_hash": "h"});
    strip_volatile("checkpoint", &mut v);
    assert_eq!(v, json!({"state": {"turn": 3}, "state_hash": "h"}));
    let mut v = json!({"anything": 1});
    strip_volatile("user_message", &mut v);
    assert_eq!(v, json!({"anything": 1}));
}

#[tokio::test]
async fn a_payload_divergence_is_reported_at_its_index() {
    let recorded = recorded_session().await;
    let mut changed = recorded.clone();
    let idx = changed
        .iter()
        .position(|e| matches!(e.body, EventBody::ModelResponse(_)))
        .unwrap();
    let EventBody::ModelResponse(p) = &mut changed[idx].body else {
        panic!()
    };
    p.content = vec![ContentBlock::Text {
        text: "something else".into(),
    }];
    let report = diff(&recorded, &changed);
    assert!(!report.identical);
    let (i, desc) = report.first_diff.clone().unwrap();
    let expected_index = recorded[..idx]
        .iter()
        .filter(|e| !VOLATILE_KINDS.contains(&e.body.kind()))
        .count();
    assert_eq!(i, expected_index);
    assert_eq!(report.compared, expected_index);
    assert!(desc.contains("model_response"), "{desc}");
    assert!(desc.contains("something else"), "{desc}");
    // A byte-level difference in a nested checkpoint state field (not volatile) is caught too.
    let mut changed = recorded.clone();
    let ck = changed
        .iter()
        .rposition(|e| matches!(e.body, EventBody::Checkpoint(_)))
        .unwrap();
    let EventBody::Checkpoint(p) = &mut changed[ck].body else {
        panic!()
    };
    p.state.turn += 1;
    let report = diff(&recorded, &changed);
    assert!(!report.identical);
    assert!(report.first_diff.unwrap().1.contains("checkpoint"));
}

#[tokio::test]
async fn kind_and_length_divergences_are_reported() {
    let recorded = recorded_session().await;
    // Kind: swap two adjacent non-volatile events.
    let mut swapped = recorded.clone();
    let i = swapped
        .iter()
        .position(|e| matches!(e.body, EventBody::ModelRequest(_)))
        .unwrap();
    swapped.swap(i, i + 1);
    let report = diff(&recorded, &swapped);
    assert!(!report.identical);
    let (idx, desc) = report.first_diff.unwrap();
    assert!(
        desc.contains("kind `model_request` recorded, `model_response` replayed"),
        "{desc}"
    );
    assert_eq!(idx, report.compared);
    // Length: a truncated replay.
    let truncated = recorded[..recorded.len() - 2].to_vec();
    let report = diff(&recorded, &truncated);
    assert!(!report.identical);
    let (idx, desc) = report.first_diff.unwrap();
    assert_eq!(idx, report.compared);
    assert!(desc.contains("events"), "{desc}");
    // Length only counts non-volatile events: appending a warning is not a divergence.
    let mut longer = recorded.clone();
    longer.push(warning(longer.len() as u64));
    assert!(diff(&recorded, &longer).identical);
}

#[tokio::test]
async fn effective_log_is_compared_not_the_physical_one() {
    // Events voided by a `recovered` range are invisible to the diff.
    let recorded = recorded_session().await;
    let mut with_discard = recorded.clone();
    let n = with_discard.len() as u64;
    with_discard.push(Event {
        seq: n,
        ts: "t".into(),
        session_id: SessionId("s_test".into()),
        body: EventBody::Warning(WarningPayload::kernel(9, "garbage", "to be voided", None)),
    });
    with_discard.push(Event {
        seq: n + 1,
        ts: "t".into(),
        session_id: SessionId("s_test".into()),
        body: EventBody::UserMessage(UserMessagePayload {
            turn: 9,
            applied: AppliedAt {
                turn: 9,
                at: AppliedPoint::Idle,
            },
            content: vec![],
        }),
    });
    with_discard.push(Event {
        seq: n + 2,
        ts: "t".into(),
        session_id: SessionId("s_test".into()),
        body: EventBody::Recovered(RecoveredPayload {
            checkpoint_hash: h("aa"),
            checkpoint_seq: n - 1,
            discarded_seq: Some(SeqRange { from: n, to: n + 1 }),
            restored_status: SessionStatus::Idle,
            tasks_cancelled: vec![],
            kernel_version: "x".into(),
            event_schema_version: EVENT_SCHEMA_VERSION,
        }),
    });
    assert!(diff(&recorded, &with_discard).identical);
}

// ---- the binary -----------------------------------------------------------------------------

fn write_jsonl(path: &std::path::Path, events: &[Event]) {
    let mut text = String::new();
    for e in events {
        let raw = e.body.payload_value().unwrap();
        text.push_str(&envelope_line(e, &raw).unwrap());
    }
    std::fs::write(path, text).unwrap();
}

#[tokio::test]
async fn diff_logs_binary_exits_0_on_identical_and_1_with_the_first_divergence() {
    let recorded = recorded_session().await;
    let mut replayed = recorded.clone();
    for e in &mut replayed {
        e.ts = "2030-01-01T00:00:00.000Z".into();
        if let EventBody::ToolResult(p) = &mut e.body {
            p.duration_ms = 42;
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("recorded.jsonl");
    let b = dir.path().join("replayed.jsonl");
    write_jsonl(&a, &recorded);
    write_jsonl(&b, &replayed);
    let bin = env!("CARGO_BIN_EXE_diff-logs");
    let out = std::process::Command::new(bin)
        .arg(&a)
        .arg(&b)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("identical"), "{stdout}");

    // A divergence.
    let mut diverged = recorded.clone();
    let i = diverged
        .iter()
        .position(|e| matches!(e.body, EventBody::ToolResult(_)))
        .unwrap();
    let EventBody::ToolResult(p) = &mut diverged[i].body else {
        panic!()
    };
    p.is_error = true;
    let c = dir.path().join("diverged.jsonl");
    write_jsonl(&c, &diverged);
    let out = std::process::Command::new(bin)
        .arg(&a)
        .arg(&c)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("divergence at index"), "{stdout}");
    assert!(stdout.contains("tool_result"), "{stdout}");

    // Usage and unreadable input also exit 1, on stderr.
    let out = std::process::Command::new(bin).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage"));
    let out = std::process::Command::new(bin)
        .arg(&a)
        .arg(dir.path().join("nope.jsonl"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}
