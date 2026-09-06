//! P1.3: `FileEventLog` against `event-schema.md` §1 (envelope, framing, schema version,
//! ordering), §2.1 (`log_opened`), §4 (the D10 acceptance test), and the robustness list of the
//! P1.3 plan (round trips, re-open, torn tails, wrong session, newer schema).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kernel::log::file::parse_log;
use kernel::log::{FileEventLog, writer};
use kernel::state::state_hash_of_raw;
use kernel::*;
use proptest::prelude::*;
use serde_json::{Value, json};

fn h(byte: &str) -> Hash {
    Hash::parse(&format!("b3:{}", byte.repeat(32))).unwrap()
}

fn sid() -> SessionId {
    SessionId("s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4".into())
}

fn state_with_text(text: &str) -> State {
    State {
        schema_version: STATE_SCHEMA_VERSION,
        session_id: sid(),
        created_at: "2026-09-06T12:00:00.001Z".into(),
        turn: 1,
        session_status: SessionStatus::Running,
        messages: vec![Message::user_text(text)],
        pending_tasks: BTreeMap::new(),
        profiles: ActiveProfiles {
            model_profile_hash: h("1a"),
            agent_profile_hash: h("2b"),
            resolved_profile_hash: h("3c"),
            project_profile_hash: None,
            bundles_hash: None,
        },
        memory: None,
        notebook_path: Some(PathBuf::from("/work/.grist/notebook.md")),
        sandbox_policy_hash: h("4d"),
        sandbox_backend: "none".into(),
    }
}

fn checkpoint(state: &State, status: SessionStatus) -> EventBody {
    EventBody::Checkpoint(CheckpointPayload {
        turn: state.turn,
        reason: CheckpointReason::TurnEnd,
        session_status: status,
        state_hash: state.state_hash().unwrap(),
        state: state.clone(),
    })
}

fn warning(turn: u64, class: &str, message: &str) -> EventBody {
    EventBody::Warning(WarningPayload::kernel(turn, class, message, None))
}

fn tool_result(text: &str) -> EventBody {
    EventBody::ToolResult(ToolResultPayload {
        turn: 1,
        tool_use_id: "call_1".into(),
        name: "run_script".into(),
        result_hash: h("aa"),
        is_error: false,
        content: ToolResultContent::Json(json!({"stdout": text})),
        artifact_handles: vec![],
        spilled: false,
        spill: None,
        duration_ms: 3,
        origin: ToolOutputOrigin::Invoke,
        task: None,
    })
}

fn model_response(text: &str) -> EventBody {
    EventBody::ModelResponse(ModelResponsePayload {
        turn: 1,
        request_hash: h("bb"),
        response_hash: h("cc"),
        raw_response_hash: h("dd"),
        model_id: "m".into(),
        stop_reason: StopReason::EndTurn,
        usage: Usage {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_tokens: None,
            reasoning_tokens: None,
        },
        content: vec![ContentBlock::Text { text: text.into() }],
        attempts: 1,
    })
}

struct Tmp {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn tmp() -> Tmp {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("{}.jsonl", sid()));
    Tmp { _dir: dir, path }
}

async fn create(path: &Path) -> FileEventLog {
    FileEventLog::create(path, sid(), Arc::new(Redactor::new()))
        .await
        .unwrap()
}

fn lines_of(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

// ---- create / log_opened (§2.1) ---------------------------------------------------------------

#[tokio::test]
async fn create_writes_log_opened_live_at_seq0_in_envelope_member_order() {
    let t = tmp();
    let log = create(&t.path).await;
    assert_eq!(log.session_id(), &sid());
    assert_eq!(log.last_seq(), Some(0));
    assert_eq!(log.path(), t.path);
    let lines = lines_of(&t.path);
    assert_eq!(lines.len(), 1);
    // Exactly the five members, in §1.1 order, with `log_opened` at seq 0.
    let prefix = format!(
        "{{\"seq\":0,\"ts\":\"{}\",\"session_id\":\"{}\",\"kind\":\"log_opened\",\"payload\":",
        log.events()[0].ts,
        sid()
    );
    assert!(lines[0].starts_with(&prefix), "{}", lines[0]);
    let v: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(v.as_object().unwrap().len(), 5);
    assert_eq!(
        v["payload"],
        json!({
            "event_schema_version": EVENT_SCHEMA_VERSION,
            "state_schema_version": STATE_SCHEMA_VERSION,
            "kernel_version": KERNEL_VERSION,
            "mode": "live"
        })
    );
    // The file is UTF-8 without BOM and every line ends with \n.
    let bytes = std::fs::read(&t.path).unwrap();
    assert_eq!(bytes[0], b'{');
    assert_eq!(*bytes.last().unwrap(), b'\n');
}

#[tokio::test]
async fn create_with_mode_writes_replay() {
    let t = tmp();
    let log =
        FileEventLog::create_with_mode(&t.path, sid(), LogMode::Replay, Arc::new(Redactor::new()))
            .await
            .unwrap();
    match &log.events()[0].body {
        EventBody::LogOpened(p) => assert_eq!(p.mode, LogMode::Replay),
        other => panic!("{other:?}"),
    }
    let reopened = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .unwrap();
    assert_eq!(reopened.events(), log.events());
}

#[tokio::test]
async fn create_refuses_an_existing_file() {
    let t = tmp();
    let _log = create(&t.path).await;
    let err = FileEventLog::create(&t.path, sid(), Arc::new(Redactor::new()))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, LogError::Io(_)), "{err:?}");
    assert_eq!(lines_of(&t.path).len(), 1, "the existing log is untouched");
}

// ---- append (§1.1, §1.2, §1.4) ----------------------------------------------------------------

#[tokio::test]
async fn append_assigns_seq_and_ts_and_writes_one_line_per_event() {
    let t = tmp();
    let log = create(&t.path).await;
    let e1 = log.append(warning(0, "a", "one")).await.unwrap();
    let e2 = log.append(warning(0, "b", "two")).await.unwrap();
    assert_eq!((e1.seq, e2.seq), (1, 2));
    assert_eq!(log.last_seq(), Some(2));
    for e in [&e1, &e2] {
        assert_eq!(e.ts.len(), 24, "{}", e.ts);
        assert!(e.ts.ends_with('Z'));
        assert_eq!(e.session_id, sid());
    }
    let lines = lines_of(&t.path);
    assert_eq!(lines.len(), 3);
    let on_disk: Event = serde_json::from_str(&lines[2]).unwrap();
    assert_eq!(on_disk, e2, "the returned event is the event as written");
    assert!(std::fs::read(&t.path).unwrap().ends_with(b"}\n"));
}

#[tokio::test]
async fn reader_snapshot_and_snapshot_from_path_agree_with_events() {
    let t = tmp();
    let log = create(&t.path).await;
    log.append(warning(0, "a", "one")).await.unwrap();
    let reader = log.reader();
    let from_reader: Vec<Event> = reader.iter().map(Result::unwrap).collect();
    assert_eq!(from_reader, log.events());
    let snap = FileEventLog::snapshot(&t.path).unwrap();
    assert_eq!(snap.events(), log.events());
    assert_eq!(snap.effective_events(), from_reader.as_slice());
    // The snapshot is a snapshot: a later append is not visible through it.
    log.append(warning(0, "b", "two")).await.unwrap();
    assert_eq!(reader.iter().count(), 2);
    assert_eq!(log.events().len(), 3);
}

#[tokio::test]
async fn reopen_and_append_continues_seq_without_gap() {
    let t = tmp();
    {
        let log = create(&t.path).await;
        log.append(warning(0, "a", "one")).await.unwrap();
    }
    let log = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .unwrap();
    assert_eq!(log.session_id(), &sid());
    assert_eq!(log.last_seq(), Some(1));
    let e = log.append(warning(0, "b", "two")).await.unwrap();
    assert_eq!(e.seq, 2);
    let reopened = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .unwrap();
    let seqs: Vec<u64> = reopened.events().iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![0, 1, 2]);
    // `open` writes nothing: no `log_opened` was added.
    assert_eq!(
        reopened
            .events()
            .iter()
            .filter(|e| e.body.kind() == "log_opened")
            .count(),
        1
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    #[test]
    fn many_events_written_then_reopened_read_back_identical(
        msgs in prop::collection::vec(("[a-z]{0,12}", 0u64..5, "[a-z]{1,8}"), 1..40)
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let t = tmp();
            let written = {
                let log = create(&t.path).await;
                let mut written = log.events();
                for (message, turn, class) in &msgs {
                    written.push(log.append(warning(*turn, class, message)).await.unwrap());
                }
                written
            };
            let reopened = FileEventLog::open(&t.path, Arc::new(Redactor::new())).await.unwrap();
            prop_assert_eq!(reopened.events(), written.clone());
            prop_assert_eq!(reopened.last_seq(), Some(msgs.len() as u64));
            let snap = FileEventLog::snapshot(&t.path).unwrap();
            prop_assert_eq!(snap.events(), written);
            Ok(())
        })?;
    }
}

// ---- open: validation (§1.2, §1.3, §1.4) -----------------------------------------------------

#[tokio::test]
async fn open_rejects_a_seq_gap() {
    let t = tmp();
    {
        let log = create(&t.path).await;
        log.append(warning(0, "a", "one")).await.unwrap();
        log.append(warning(0, "b", "two")).await.unwrap();
    }
    let mut lines = lines_of(&t.path);
    lines[2] = lines[2].replacen("\"seq\":2", "\"seq\":3", 1);
    std::fs::write(&t.path, lines.join("\n") + "\n").unwrap();
    let err = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(
            err,
            LogError::SeqOrder {
                found: 3,
                expected: 2
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn open_rejects_a_line_from_another_session() {
    let t = tmp();
    {
        let log = create(&t.path).await;
        log.append(warning(0, "a", "one")).await.unwrap();
    }
    let mut lines = lines_of(&t.path);
    lines[1] = lines[1].replacen(&sid().0, "s_other", 1);
    std::fs::write(&t.path, lines.join("\n") + "\n").unwrap();
    let err = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .err()
        .unwrap();
    match err {
        LogError::WrongSession { found, expected } => {
            assert_eq!(found, SessionId("s_other".into()));
            assert_eq!(expected, sid());
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn open_for_rejects_a_file_of_another_session() {
    let t = tmp();
    let _ = create(&t.path).await;
    let err = FileEventLog::open_for(
        &t.path,
        SessionId("s_other".into()),
        Arc::new(Redactor::new()),
    )
    .await
    .err()
    .unwrap();
    match err {
        LogError::WrongSession { found, expected } => {
            assert_eq!(found, sid());
            assert_eq!(expected, SessionId("s_other".into()));
        }
        other => panic!("{other:?}"),
    }
    let ok = FileEventLog::open_for(&t.path, sid(), Arc::new(Redactor::new()))
        .await
        .unwrap();
    assert_eq!(ok.session_id(), &sid());
}

#[tokio::test]
async fn open_rejects_a_newer_event_schema_version() {
    let t = tmp();
    let _ = create(&t.path).await;
    let mut lines = lines_of(&t.path);
    lines[0] = lines[0].replacen(
        &format!("\"event_schema_version\":{EVENT_SCHEMA_VERSION}"),
        "\"event_schema_version\":99",
        1,
    );
    assert!(lines[0].contains("\"event_schema_version\":99"));
    std::fs::write(&t.path, lines.join("\n") + "\n").unwrap();
    let err = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(err, LogError::NewerSchema { found: 99, supported } if supported == EVENT_SCHEMA_VERSION),
        "{err:?}"
    );
    assert!(matches!(
        FileEventLog::snapshot(&t.path),
        Err(LogError::NewerSchema { found: 99, .. })
    ));
}

#[tokio::test]
async fn open_rejects_a_newer_event_schema_version_on_recovered() {
    // §1.3: a newer kernel that appended to this file stamps its version on `recovered`.
    let t = tmp();
    {
        let log = create(&t.path).await;
        log.append(EventBody::Recovered(RecoveredPayload {
            checkpoint_hash: h("ee"),
            checkpoint_seq: 0,
            discarded_seq: None,
            restored_status: SessionStatus::Idle,
            tasks_cancelled: vec![],
            kernel_version: "9.9.9".into(),
            event_schema_version: EVENT_SCHEMA_VERSION + 1,
        }))
        .await
        .unwrap();
    }
    let err = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, LogError::NewerSchema { .. }), "{err:?}");
}

#[tokio::test]
async fn open_tolerates_a_truncated_last_line_and_the_next_append_lands_at_the_right_seq() {
    let t = tmp();
    {
        let log = create(&t.path).await;
        log.append(warning(0, "a", "one")).await.unwrap();
        log.append(warning(0, "b", "two")).await.unwrap();
    }
    let complete = std::fs::read(&t.path).unwrap();
    let mut torn = complete.clone();
    torn.extend_from_slice(
        b"{\"seq\":3,\"ts\":\"2026-09-06T12:00:00.000Z\",\"session_id\":\"s\",\"kind\":\"warn",
    );
    std::fs::write(&t.path, &torn).unwrap();

    let log = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .unwrap();
    assert_eq!(log.last_seq(), Some(2), "the torn line is absent");
    // The only in-place operation: the file is cut back to the last complete newline; the
    // complete lines are byte-identical.
    assert_eq!(std::fs::read(&t.path).unwrap(), complete);
    let e = log.append(warning(0, "c", "three")).await.unwrap();
    assert_eq!(e.seq, 3);
    let bytes = std::fs::read(&t.path).unwrap();
    assert!(bytes.starts_with(&complete));
    let reopened = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .unwrap();
    let seqs: Vec<u64> = reopened.events().iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![0, 1, 2, 3]);
    // The read-only snapshot tolerates the torn tail too, without touching the file.
    std::fs::write(&t.path, &torn).unwrap();
    assert_eq!(FileEventLog::snapshot(&t.path).unwrap().events().len(), 3);
    assert_eq!(std::fs::read(&t.path).unwrap(), torn);
}

#[tokio::test]
async fn open_fails_on_a_malformed_non_final_line() {
    let t = tmp();
    {
        let log = create(&t.path).await;
        log.append(warning(0, "a", "one")).await.unwrap();
        log.append(warning(0, "b", "two")).await.unwrap();
    }
    let mut lines = lines_of(&t.path);
    lines[1] = "{\"seq\":1,\"ts\":\"x\",\"session_id\":\"s\",\"kind\":\"warning\"".into(); // no closing brace
    std::fs::write(&t.path, lines.join("\n") + "\n").unwrap();
    let err = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(err, LogError::Malformed { line: 2, .. }),
        "{err:?}"
    );
    // A blank line in the middle is malformed too; and a known kind with an invalid payload.
    lines[1] = String::new();
    std::fs::write(&t.path, lines.join("\n") + "\n").unwrap();
    let err = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(err, LogError::Malformed { line: 2, .. }),
        "{err:?}"
    );
    lines[1] = format!(
        "{{\"seq\":1,\"ts\":\"x\",\"session_id\":\"{}\",\"kind\":\"warning\",\"payload\":{{\"turn\":\"not a number\"}}}}",
        sid()
    );
    std::fs::write(&t.path, lines.join("\n") + "\n").unwrap();
    let err = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(err, LogError::Malformed { line: 2, .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn open_requires_a_log_opened_first_line() {
    let t = tmp();
    std::fs::write(&t.path, b"").unwrap();
    let err = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(err, LogError::Malformed { line: 1, .. }),
        "{err:?}"
    );
    let ev = Event {
        seq: 0,
        ts: "2026-09-06T12:00:00.000Z".into(),
        session_id: sid(),
        body: warning(0, "a", "not log_opened"),
    };
    std::fs::write(&t.path, serde_json::to_string(&ev).unwrap() + "\n").unwrap();
    let err = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(err, LogError::Malformed { line: 1, .. }),
        "{err:?}"
    );
    // A missing file is an I/O error, not a silent create.
    let err = FileEventLog::open(&t.path.with_extension("missing"), Arc::new(Redactor::new()))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, LogError::Io(_)), "{err:?}");
}

#[tokio::test]
async fn unknown_kinds_and_unknown_payload_fields_survive_open() {
    let t = tmp();
    let _ = create(&t.path).await;
    let mut lines = lines_of(&t.path);
    lines.push(format!(
        "{{\"seq\":1,\"ts\":\"2026-09-06T12:00:00.000Z\",\"session_id\":\"{}\",\"kind\":\"future_kind\",\"payload\":{{\"x\":1}}}}",
        sid()
    ));
    lines.push(format!(
        "{{\"seq\":2,\"ts\":\"2026-09-06T12:00:00.000Z\",\"session_id\":\"{}\",\"kind\":\"warning\",\"payload\":{{\"turn\":0,\"class\":\"c\",\"message\":\"m\",\"detail\":null,\"source\":\"kernel\",\"added_later\":true}}}}",
        sid()
    ));
    std::fs::write(&t.path, lines.join("\n") + "\n").unwrap();
    let log = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .unwrap();
    let events = log.events();
    assert!(
        matches!(&events[1].body, EventBody::Unknown { kind, payload } if kind == "future_kind" && payload == &json!({"x": 1}))
    );
    assert!(matches!(&events[2].body, EventBody::Warning(w) if w.class == "c"));
    let e = log.append(warning(0, "d", "after")).await.unwrap();
    assert_eq!(e.seq, 3);
}

// ---- checkpoints (§1.2 fsync, §2.13, §3.5) ---------------------------------------------------

#[tokio::test]
async fn checkpoint_payload_bytes_on_disk_canonicalize_to_the_state_hash() {
    let t = tmp();
    let log = create(&t.path).await;
    let state = state_with_text("Mesh the bracket at 2 mm.");
    let ev = log
        .append(checkpoint(&state, SessionStatus::Idle))
        .await
        .unwrap();
    let line = &lines_of(&t.path)[ev.seq as usize];
    let v: Value = serde_json::from_str(line).unwrap();
    let recorded = Hash::parse(v["payload"]["state_hash"].as_str().unwrap()).unwrap();
    assert_eq!(state_hash_of_raw(&v["payload"]["state"]).unwrap(), recorded);
    assert_eq!(recorded, state.state_hash().unwrap());
    // The full state is inline (§2.13), not a reference.
    assert_eq!(
        v["payload"]["state"]["messages"][0]["content"][0]["text"],
        "Mesh the bracket at 2 mm."
    );
    // And it restores from disk alone.
    let restored = FileEventLog::snapshot(&t.path)
        .unwrap()
        .restore(&recorded, &MigrationRegistry::new())
        .unwrap();
    assert_eq!(restored.state, state);
    assert!(restored.migrated.is_none());
}

#[tokio::test]
async fn checkpoint_and_terminal_events_are_on_disk_when_append_returns() {
    // fsync itself is not observable from a test; what is: the bytes are readable by another
    // handle the moment `append` returns, for every kind `writer::needs_fsync` names.
    let t = tmp();
    let log = create(&t.path).await;
    let state = state_with_text("x");
    let bodies = vec![
        checkpoint(&state, SessionStatus::Idle),
        EventBody::Suspended(SuspendedPayload {
            turn: 1,
            reason: SuspendReason::Explicit,
            pending_task_ids: vec![],
            in_process_wakers: 0,
            checkpoint_hash: state.state_hash().unwrap(),
        }),
        EventBody::SessionFailed(SessionFailedPayload {
            turn: 1,
            cause_seq: 1,
            error_class: "internal".into(),
            checkpoint_hash: state.state_hash().unwrap(),
            resumable: true,
        }),
        EventBody::SessionEnded(SessionEndedPayload {
            turn: 1,
            by: EndedBy::User,
            checkpoint_hash: state.state_hash().unwrap(),
            cancelled_task_ids: vec![],
        }),
    ];
    for body in bodies {
        assert!(writer::needs_fsync(&body), "{}", body.kind());
        let ev = log.append(body).await.unwrap();
        let on_disk: Event = serde_json::from_str(&lines_of(&t.path)[ev.seq as usize]).unwrap();
        assert_eq!(on_disk, ev);
    }
    assert!(!writer::needs_fsync(&warning(0, "a", "b")));
    assert!(!writer::needs_fsync(&tool_result("x")));
}

// ---- D10 acceptance (§4.1 pass 2, §4.3) -------------------------------------------------------

const SECRET: &str = "sk-ant-api03-REGISTEREDSECRETVALUE0123456789";
/// A registered value that matches no built-in pattern, so only the known-value pass can catch it.
const PLAIN_SECRET: &str = "correct horse battery staple";

fn secret_redactor() -> Arc<Redactor> {
    let r = Redactor::new();
    r.register_secret(&SecretString::new(SECRET));
    r.register_secret(&SecretString::new(PLAIN_SECRET));
    Arc::new(r)
}

fn assert_no_secret_bytes(path: &Path) {
    let bytes = std::fs::read(path).unwrap();
    for s in [SECRET, PLAIN_SECRET] {
        assert!(
            !bytes.windows(s.len()).any(|w| w == s.as_bytes()),
            "secret {s:?} leaked into the log file"
        );
    }
}

fn late_redactions(log: &FileEventLog) -> Vec<(u64, Value)> {
    log.events()
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::Warning(w) if w.class == "late_redaction" => {
                Some((e.seq, w.detail.clone().unwrap()))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn d10_registered_secret_in_a_tool_result_never_reaches_the_file() {
    let t = tmp();
    let log = FileEventLog::create(&t.path, sid(), secret_redactor())
        .await
        .unwrap();
    let ev = log
        .append(tool_result(&format!("token={SECRET} pw={PLAIN_SECRET}")))
        .await
        .unwrap();
    assert_no_secret_bytes(&t.path);
    match &ev.body {
        EventBody::ToolResult(p) => assert_eq!(
            p.content,
            ToolResultContent::Json(
                json!({"stdout": "token=[REDACTED:secret] pw=[REDACTED:secret]"})
            )
        ),
        other => panic!("{other:?}"),
    }
    // The writer pass had to change something: `late_redaction` follows the offending line,
    // with `detail: {seq, replacements}` (§4.1).
    let late = late_redactions(&log);
    assert_eq!(
        late,
        vec![(ev.seq + 1, json!({"seq": ev.seq, "replacements": 2}))]
    );
    assert_eq!(log.last_seq(), Some(ev.seq + 1));
    // Reading back keeps the redacted content and the warning.
    let reopened = FileEventLog::open(&t.path, secret_redactor())
        .await
        .unwrap();
    assert_eq!(reopened.events(), log.events());
}

#[tokio::test]
async fn d10_registered_secret_in_a_model_response_and_a_checkpoint_state_never_reaches_the_file() {
    let t = tmp();
    let log = FileEventLog::create(&t.path, sid(), secret_redactor())
        .await
        .unwrap();
    let mr = log
        .append(model_response(&format!("the key is {SECRET}")))
        .await
        .unwrap();
    let state = state_with_text(&format!("my password is {PLAIN_SECRET}"));
    let cp = log
        .append(checkpoint(&state, SessionStatus::Idle))
        .await
        .unwrap();
    assert_no_secret_bytes(&t.path);
    match &mr.body {
        EventBody::ModelResponse(p) => assert_eq!(
            p.content,
            vec![ContentBlock::Text {
                text: "the key is [REDACTED:secret]".into()
            }]
        ),
        other => panic!("{other:?}"),
    }
    match &cp.body {
        EventBody::Checkpoint(p) => assert_eq!(
            p.state.messages[0].content[0],
            ContentBlock::Text {
                text: "my password is [REDACTED:secret]".into()
            }
        ),
        other => panic!("{other:?}"),
    }
    let late = late_redactions(&log);
    assert_eq!(late.len(), 2);
    assert_eq!(late[0].0, mr.seq + 1);
    assert_eq!(late[1].0, cp.seq + 1);
    // A late redaction of a checkpoint changes the state but not the recorded hash, so the
    // line no longer verifies: restore MUST refuse it rather than silently accept (§6.2).
    let err = log
        .reader()
        .restore_latest(&MigrationRegistry::new())
        .err()
        .unwrap();
    assert!(matches!(err, RestoreError::HashMismatch { .. }), "{err:?}");
}

#[tokio::test]
async fn late_redaction_is_not_written_when_the_payload_is_already_clean() {
    let t = tmp();
    let log = FileEventLog::create(&t.path, sid(), secret_redactor())
        .await
        .unwrap();
    // Ingress already redacted: the writer pass finds nothing (fixed point) and adds nothing.
    let ev = log
        .append(tool_result(
            "token=[REDACTED:secret] and Bearer [REDACTED:bearer]",
        ))
        .await
        .unwrap();
    log.append(model_response("clean text")).await.unwrap();
    let state = state_with_text("clean state");
    log.append(checkpoint(&state, SessionStatus::Idle))
        .await
        .unwrap();
    assert!(late_redactions(&log).is_empty());
    assert_eq!(log.last_seq(), Some(ev.seq + 2));
    assert_eq!(lines_of(&t.path).len(), 4);
}

#[tokio::test]
async fn writer_pass_also_catches_pattern_shaped_tokens_from_unregistered_sources() {
    let t = tmp();
    let log = create(&t.path).await;
    let ghp = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
    let ev = log
        .append(warning(2, "x", &format!("leaked {ghp}")))
        .await
        .unwrap();
    let bytes = std::fs::read(&t.path).unwrap();
    assert!(!bytes.windows(ghp.len()).any(|w| w == ghp.as_bytes()));
    let late = late_redactions(&log);
    assert_eq!(
        late,
        vec![(ev.seq + 1, json!({"seq": ev.seq, "replacements": 1}))]
    );
    // The warning's `turn` follows the offending payload's `turn`.
    match &log.events()[ev.seq as usize + 1].body {
        EventBody::Warning(w) => assert_eq!(w.turn, 2),
        other => panic!("{other:?}"),
    }
}

// ---- the memory and file logs share one writer path -------------------------------------------

#[tokio::test]
async fn memory_and_file_logs_produce_the_same_redacted_events() {
    let t = tmp();
    let file = FileEventLog::create(&t.path, sid(), secret_redactor())
        .await
        .unwrap();
    let mem = MemoryEventLog::new(sid(), secret_redactor());
    mem.append(EventBody::LogOpened(LogOpenedPayload {
        event_schema_version: EVENT_SCHEMA_VERSION,
        state_schema_version: STATE_SCHEMA_VERSION,
        kernel_version: KERNEL_VERSION.into(),
        mode: LogMode::Live,
    }))
    .await
    .unwrap();
    let body = tool_result(&format!("token={SECRET}"));
    let a = file.append(body.clone()).await.unwrap();
    let b = mem.append(body).await.unwrap();
    assert_eq!(a.body, b.body);
    assert_eq!(a.seq, b.seq);
    let strip_ts = |events: Vec<Event>| -> Vec<(u64, EventBody)> {
        events.into_iter().map(|e| (e.seq, e.body)).collect()
    };
    assert_eq!(strip_ts(file.events()), strip_ts(mem.events()));
    assert_eq!(file.last_seq(), mem.last_seq());
}

// ---- parse_log: the pure parser used by open and snapshot -------------------------------------

#[test]
fn parse_log_reports_complete_len_and_torn_tail() {
    let ev = Event {
        seq: 0,
        ts: "2026-09-06T12:00:00.000Z".into(),
        session_id: sid(),
        body: EventBody::LogOpened(LogOpenedPayload {
            event_schema_version: EVENT_SCHEMA_VERSION,
            state_schema_version: STATE_SCHEMA_VERSION,
            kernel_version: KERNEL_VERSION.into(),
            mode: LogMode::Live,
        }),
    };
    let line = serde_json::to_string(&ev).unwrap() + "\n";
    let parsed = parse_log(line.as_bytes()).unwrap();
    assert_eq!(parsed.lines.len(), 1);
    assert_eq!(parsed.complete_len, line.len());
    assert!(!parsed.truncated_tail);
    assert_eq!(parsed.session_id(), Some(&sid()));
    let torn = format!("{line}{{\"seq\":1");
    let parsed = parse_log(torn.as_bytes()).unwrap();
    assert_eq!(parsed.lines.len(), 1);
    assert_eq!(parsed.complete_len, line.len());
    assert!(parsed.truncated_tail);
    let empty = parse_log(b"").unwrap();
    assert!(empty.lines.is_empty());
    assert_eq!(empty.complete_len, 0);
    assert!(!empty.truncated_tail);
    assert!(matches!(
        parse_log(b"\xff\xfe\n"),
        Err(LogError::Malformed { line: 1, .. })
    ));
}

#[test]
fn envelope_line_round_trips_through_serde_with_the_payload_verbatim() {
    let state = state_with_text("x");
    let body = checkpoint(&state, SessionStatus::Idle);
    let ev = Event {
        seq: 7,
        ts: "2026-09-06T12:00:00.000Z".into(),
        session_id: sid(),
        body: body.clone(),
    };
    let raw = body.payload_value().unwrap();
    let line = writer::envelope_line(&ev, &raw).unwrap();
    assert!(line.ends_with("}\n"));
    assert_eq!(line.matches('\n').count(), 1);
    let back: Event = serde_json::from_str(&line).unwrap();
    assert_eq!(back, ev);
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["payload"], raw);
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys.len(), 5);
    assert!(line.starts_with("{\"seq\":7,\"ts\":\"2026-09-06T12:00:00.000Z\",\"session_id\":"));
}
