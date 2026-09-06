//! P1.3: the crash-recovery data path on the file reader (`kernel-interface.md` §7.3,
//! `event-schema.md` §6): `ends_cleanly`, the effective log, `restore`/`restore_latest`, hash
//! verification, and migrations. `Kernel::open` orchestration is P1.2's and is not tested here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kernel::log::FileEventLog;
use kernel::log::memory::SnapshotReader;
use kernel::log::reader;
use kernel::*;
use serde_json::{Value, json};

fn h(byte: &str) -> Hash {
    Hash::parse(&format!("b3:{}", byte.repeat(32))).unwrap()
}

fn sid() -> SessionId {
    SessionId("s_rec".into())
}

fn state(turn: u64, status: SessionStatus, text: &str) -> State {
    State {
        schema_version: STATE_SCHEMA_VERSION,
        session_id: sid(),
        created_at: "2026-09-06T12:00:00.001Z".into(),
        turn,
        session_status: status,
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
        notebook_path: None,
        sandbox_policy_hash: h("4d"),
        sandbox_backend: "none".into(),
    }
}

fn checkpoint(s: &State) -> EventBody {
    EventBody::Checkpoint(CheckpointPayload {
        turn: s.turn,
        reason: CheckpointReason::TurnEnd,
        session_status: s.session_status,
        state_hash: s.state_hash().unwrap(),
        state: s.clone(),
    })
}

fn model_request(turn: u64) -> EventBody {
    EventBody::Warning(WarningPayload::kernel(
        turn,
        "mid_turn_marker",
        "stand-in for a mid-turn event",
        None,
    ))
}

fn recovered(cp: &State, cp_seq: u64, discarded: Option<(u64, u64)>) -> EventBody {
    EventBody::Recovered(RecoveredPayload {
        checkpoint_hash: cp.state_hash().unwrap(),
        checkpoint_seq: cp_seq,
        discarded_seq: discarded.map(|(from, to)| SeqRange { from, to }),
        restored_status: cp.session_status,
        tasks_cancelled: vec![],
        kernel_version: KERNEL_VERSION.into(),
        event_schema_version: EVENT_SCHEMA_VERSION,
    })
}

struct Tmp {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn tmp() -> Tmp {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s_rec.jsonl");
    Tmp { _dir: dir, path }
}

async fn create(path: &Path) -> FileEventLog {
    FileEventLog::create(path, sid(), Arc::new(Redactor::new()))
        .await
        .unwrap()
}

fn snapshot(path: &Path) -> SnapshotReader {
    FileEventLog::snapshot(path).unwrap()
}

fn effective_seqs(r: &SnapshotReader) -> Vec<u64> {
    r.effective().map(|e| e.unwrap().seq).collect()
}

// ---- ends_cleanly (§7.3) ----------------------------------------------------------------------

#[tokio::test]
async fn a_log_ending_in_an_idle_checkpoint_ends_cleanly() {
    let t = tmp();
    let log = create(&t.path).await;
    log.append(checkpoint(&state(1, SessionStatus::Idle, "a")))
        .await
        .unwrap();
    assert!(snapshot(&t.path).ends_cleanly());
    // A running checkpoint (turn_end with Continue) does not: the process died before the next
    // turn finished.
    log.append(checkpoint(&state(2, SessionStatus::Running, "b")))
        .await
        .unwrap();
    assert!(!snapshot(&t.path).ends_cleanly());
}

#[tokio::test]
async fn a_log_ending_in_suspended_ended_or_failed_ends_cleanly() {
    let s = state(1, SessionStatus::Suspended, "a");
    let terminal: Vec<EventBody> = vec![
        EventBody::Suspended(SuspendedPayload {
            turn: 1,
            reason: SuspendReason::PendingTasks,
            pending_task_ids: vec![TaskId("t1-x".into())],
            in_process_wakers: 0,
            checkpoint_hash: s.state_hash().unwrap(),
        }),
        EventBody::SessionEnded(SessionEndedPayload {
            turn: 1,
            by: EndedBy::User,
            checkpoint_hash: s.state_hash().unwrap(),
            cancelled_task_ids: vec![],
        }),
        EventBody::SessionFailed(SessionFailedPayload {
            turn: 1,
            cause_seq: 2,
            error_class: "provider".into(),
            checkpoint_hash: s.state_hash().unwrap(),
            resumable: true,
        }),
    ];
    for body in terminal {
        let t = tmp();
        let log = create(&t.path).await;
        log.append(checkpoint(&state(1, SessionStatus::Running, "a")))
            .await
            .unwrap();
        assert!(!snapshot(&t.path).ends_cleanly());
        let kind = body.kind().to_owned();
        log.append(body).await.unwrap();
        assert!(snapshot(&t.path).ends_cleanly(), "{kind}");
        // Anything after it that is not itself clean makes the log unclean again.
        log.append(model_request(2)).await.unwrap();
        assert!(!snapshot(&t.path).ends_cleanly(), "{kind}");
    }
}

#[tokio::test]
async fn a_log_that_stops_mid_turn_or_before_any_checkpoint_does_not_end_cleanly() {
    let t = tmp();
    let log = create(&t.path).await;
    assert!(!snapshot(&t.path).ends_cleanly(), "only log_opened");
    log.append(checkpoint(&state(1, SessionStatus::Idle, "a")))
        .await
        .unwrap();
    log.append(model_request(2)).await.unwrap();
    assert!(!snapshot(&t.path).ends_cleanly(), "mid-turn");
    assert!(!reader::ends_cleanly(&[]));
}

#[tokio::test]
async fn ends_cleanly_looks_at_the_effective_log_not_the_physical_one() {
    // Physical tail is a `recovered` (not clean by itself)... but a recovery that discards a
    // mid-turn tail and is followed by nothing: the last effective event is the `recovered`
    // line, which is not in the clean list, so a second crash right after recovery recovers again.
    let t = tmp();
    let log = create(&t.path).await;
    let s = state(1, SessionStatus::Idle, "a");
    log.append(checkpoint(&s)).await.unwrap(); // 1
    log.append(model_request(2)).await.unwrap(); // 2
    log.append(recovered(&s, 1, Some((2, 2)))).await.unwrap(); // 3
    let r = snapshot(&t.path);
    assert_eq!(effective_seqs(&r), vec![0, 1, 3]);
    assert!(!r.ends_cleanly());
}

// ---- effective log (§6.1) ---------------------------------------------------------------------

#[tokio::test]
async fn effective_hides_discarded_ranges_including_nested_recoveries() {
    let t = tmp();
    let log = create(&t.path).await; // 0
    let s1 = state(1, SessionStatus::Idle, "one");
    log.append(checkpoint(&s1)).await.unwrap(); // 1
    log.append(model_request(2)).await.unwrap(); // 2  (crash #1 after this)
    log.append(recovered(&s1, 1, Some((2, 2)))).await.unwrap(); // 3
    log.append(model_request(2)).await.unwrap(); // 4  (crash #2 after this)
    // Recovery #2 restores the same checkpoint and discards everything after it, including the
    // first `recovered` line and its range.
    log.append(recovered(&s1, 1, Some((2, 4)))).await.unwrap(); // 5
    let s2 = state(2, SessionStatus::Idle, "two");
    log.append(checkpoint(&s2)).await.unwrap(); // 6
    let r = snapshot(&t.path);
    assert_eq!(r.iter().count(), 7, "nothing is physically removed");
    assert_eq!(effective_seqs(&r), vec![0, 1, 5, 6]);
    assert_eq!(reader::discarded_seqs(&r.events()), vec![(2, 2), (2, 4)]);
    assert!(reader::is_discarded(&[(2, 4)], 3));
    assert!(!reader::is_discarded(&[(2, 4)], 5));
    assert!(r.ends_cleanly());
    assert_eq!(r.latest_checkpoint().unwrap().unwrap().0.seq, 6);
}

#[tokio::test]
async fn recovered_with_null_discarded_seq_hides_nothing() {
    let t = tmp();
    let log = create(&t.path).await;
    let s = state(1, SessionStatus::Idle, "a");
    log.append(checkpoint(&s)).await.unwrap(); // 1
    log.append(recovered(&s, 1, None)).await.unwrap(); // 2
    let r = snapshot(&t.path);
    assert_eq!(effective_seqs(&r), vec![0, 1, 2]);
    assert_eq!(reader::discarded_seqs(&r.events()), vec![]);
}

// ---- restore (§6.2) ---------------------------------------------------------------------------

#[tokio::test]
async fn restore_latest_after_a_recovery_picks_the_restored_checkpoint_not_a_discarded_later_one() {
    let t = tmp();
    let log = create(&t.path).await; // 0
    let good = state(1, SessionStatus::Idle, "good");
    log.append(checkpoint(&good)).await.unwrap(); // 1
    // A later checkpoint (turn_end, still running) followed by a crash mid-turn.
    let later = state(2, SessionStatus::Running, "later");
    log.append(checkpoint(&later)).await.unwrap(); // 2
    log.append(model_request(3)).await.unwrap(); // 3
    // The launcher restores `later` (last effective checkpoint) — the D15 algorithm; then the
    // operator rolls back further by recovering from `good` explicitly. Either way, once the
    // `recovered` line names its range, `restore_latest` follows it.
    log.append(recovered(&good, 1, Some((2, 3)))).await.unwrap(); // 4
    let r = snapshot(&t.path);
    let reg = MigrationRegistry::new();
    let m = r.restore_latest(&reg).unwrap().unwrap();
    assert_eq!(m.state, good);
    assert!(m.migrated.is_none());
    let (ev, cp) = r.latest_checkpoint().unwrap().unwrap();
    assert_eq!(ev.seq, 1);
    assert_eq!(cp.state, good);
    // The discarded checkpoint is not restorable by hash either.
    let err = r.restore(&later.state_hash().unwrap(), &reg).err().unwrap();
    assert!(matches!(err, RestoreError::NotFound(_)), "{err:?}");
    assert_eq!(
        r.restore(&good.state_hash().unwrap(), &reg).unwrap().state,
        good
    );
    // Before the recovery line existed, the last effective checkpoint was `later` (step 2 of
    // §7.3): the reader reports it so the kernel can compute the discarded range.
    let before = SnapshotReader::new(r.lines()[..4].to_vec());
    assert_eq!(before.restore_latest(&reg).unwrap().unwrap().state, later);
    assert!(!before.ends_cleanly());
}

#[tokio::test]
async fn restore_picks_the_latest_effective_checkpoint_with_that_hash() {
    // §3.7: a duplicate hash is harmless; the latest wins.
    let t = tmp();
    let log = create(&t.path).await;
    let s = state(1, SessionStatus::Idle, "same");
    log.append(checkpoint(&s)).await.unwrap(); // 1
    log.append(checkpoint(&s)).await.unwrap(); // 2
    let r = snapshot(&t.path);
    assert_eq!(r.latest_checkpoint().unwrap().unwrap().0.seq, 2);
    assert_eq!(
        r.restore(&s.state_hash().unwrap(), &MigrationRegistry::new())
            .unwrap()
            .state,
        s
    );
}

#[tokio::test]
async fn restore_latest_on_a_log_without_checkpoints_is_none() {
    let t = tmp();
    let log = create(&t.path).await;
    log.append(model_request(1)).await.unwrap();
    let r = snapshot(&t.path);
    assert!(
        r.restore_latest(&MigrationRegistry::new())
            .unwrap()
            .is_none()
    );
    assert!(r.latest_checkpoint().unwrap().is_none());
    assert!(matches!(
        r.restore(&h("00"), &MigrationRegistry::new()),
        Err(RestoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn a_tampered_checkpoint_on_disk_is_a_hash_mismatch() {
    let t = tmp();
    let log = create(&t.path).await;
    let s = state(1, SessionStatus::Idle, "mesh the bracket");
    log.append(checkpoint(&s)).await.unwrap();
    drop(log);
    let text = std::fs::read_to_string(&t.path).unwrap();
    // Editing a hashed field (message text) breaks verification...
    let tampered = text.replacen("mesh the bracket", "mesh the BRACKET", 1);
    assert_ne!(tampered, text);
    std::fs::write(&t.path, &tampered).unwrap();
    let r = snapshot(&t.path);
    let err = r.restore_latest(&MigrationRegistry::new()).err().unwrap();
    match err {
        RestoreError::HashMismatch { recorded, computed } => {
            assert_eq!(recorded, s.state_hash().unwrap());
            assert_ne!(computed, recorded);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        r.restore(&s.state_hash().unwrap(), &MigrationRegistry::new()),
        Err(RestoreError::HashMismatch { .. })
    ));
    // ...while editing a volatile field (§3.5) does not.
    let volatile = text.replacen(
        "\"sandbox_backend\":\"none\"",
        "\"sandbox_backend\":\"bwrap\"",
        1,
    );
    assert_ne!(volatile, text);
    std::fs::write(&t.path, &volatile).unwrap();
    let m = snapshot(&t.path)
        .restore_latest(&MigrationRegistry::new())
        .unwrap()
        .unwrap();
    assert_eq!(m.state.sandbox_backend, "bwrap");
}

// ---- migrations (§6.3, kernel-interface §3.4) -------------------------------------------------

/// v0 → v1: the v0 schema stored the conversation under `history` (hash-changing: the key
/// differs, and `schema_version` is hashed anyway).
struct RenameHistory;
impl StateMigration for RenameHistory {
    fn from_version(&self) -> u32 {
        0
    }
    fn migrate(&self, mut raw: Value) -> Result<Value, MigrationError> {
        let obj = raw.as_object_mut().unwrap();
        if let Some(history) = obj.remove("history") {
            obj.insert("messages".into(), history);
        }
        obj.insert("schema_version".into(), json!(1));
        Ok(raw)
    }
}

/// Write a hand-made v0 checkpoint line (seq 1) after `log_opened`; returns the recorded hash.
fn write_v0_checkpoint(path: &Path, deserializable: bool) -> Hash {
    let mut raw = serde_json::to_value(state(1, SessionStatus::Idle, "old")).unwrap();
    raw["schema_version"] = json!(0);
    if !deserializable {
        // The v0 shape: `history` instead of the required `messages` — the current `State`
        // rejects it (an absent `Option` field would not do: serde defaults it to `None`).
        let obj = raw.as_object_mut().unwrap();
        let msgs = obj.remove("messages").unwrap();
        obj.insert("history".into(), msgs);
    }
    let state_hash = kernel::state::state_hash_of_raw(&raw).unwrap();
    let line = json!({
        "seq": 1, "ts": "2026-09-06T12:00:00.000Z", "session_id": sid(),
        "kind": "checkpoint",
        "payload": {"turn": 1, "reason": "turn_end", "session_status": "idle", "state_hash": state_hash, "state": raw}
    });
    let mut text = std::fs::read_to_string(path).unwrap();
    text.push_str(&serde_json::to_string(&line).unwrap());
    text.push('\n');
    std::fs::write(path, text).unwrap();
    state_hash
}

#[tokio::test]
async fn an_older_schema_checkpoint_restores_through_a_registered_migration() {
    let t = tmp();
    drop(create(&t.path).await);
    let recorded = write_v0_checkpoint(&t.path, true);
    let mut reg = MigrationRegistry::new();
    reg.register(Box::new(RenameHistory)).unwrap();
    let r = snapshot(&t.path);
    let m = r.restore_latest(&reg).unwrap().unwrap();
    assert_eq!(m.migrated, Some((0, STATE_SCHEMA_VERSION)));
    assert_eq!(m.state.schema_version, STATE_SCHEMA_VERSION);
    assert_eq!(m.state.messages, vec![Message::user_text("old")]);
    // The hash was verified over the pre-migration payload; after migration it differs.
    assert_ne!(m.state.state_hash().unwrap(), recorded);
    assert_eq!(r.restore(&recorded, &reg).unwrap().state, m.state);
    // The log can still be opened and appended to.
    let log = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .unwrap();
    assert_eq!(log.last_seq(), Some(1));
    assert!(r.ends_cleanly());
}

#[tokio::test]
async fn an_older_schema_checkpoint_fails_loudly_without_a_migration() {
    let t = tmp();
    drop(create(&t.path).await);
    let recorded = write_v0_checkpoint(&t.path, true);
    let reg = MigrationRegistry::new();
    let r = snapshot(&t.path);
    let err = r.restore_latest(&reg).err().unwrap();
    assert!(
        matches!(err, RestoreError::Migration(MigrationError::MissingStep { from: 0, to }) if to == STATE_SCHEMA_VERSION),
        "{err:?}"
    );
    assert!(matches!(
        r.restore(&recorded, &reg),
        Err(RestoreError::Migration(MigrationError::MissingStep { .. }))
    ));
}

#[tokio::test]
async fn an_older_checkpoint_that_no_longer_deserializes_is_kept_raw_and_migrated() {
    // kernel-interface §3.4: "an older checkpoint may not deserialize into the current struct".
    let t = tmp();
    drop(create(&t.path).await);
    let recorded = write_v0_checkpoint(&t.path, false);
    let log = FileEventLog::open(&t.path, Arc::new(Redactor::new()))
        .await
        .unwrap();
    assert!(
        matches!(&log.events()[1].body, EventBody::Unknown { kind, .. } if kind == "checkpoint"),
        "kept raw, not Malformed"
    );
    let r = log.reader();
    assert!(
        r.latest_checkpoint().unwrap().is_none(),
        "no typed checkpoint"
    );
    let mut reg = MigrationRegistry::new();
    reg.register(Box::new(RenameHistory)).unwrap();
    let m = r.restore(&recorded, &reg).unwrap();
    assert_eq!(m.migrated, Some((0, 1)));
    assert_eq!(m.state.messages, vec![Message::user_text("old")]);
    assert_eq!(r.restore_latest(&reg).unwrap().unwrap().state, m.state);
    assert!(matches!(
        r.restore_latest(&MigrationRegistry::new()),
        Err(RestoreError::Migration(MigrationError::MissingStep { .. }))
    ));
    assert!(
        snapshot(&t.path).ends_cleanly(),
        "raw idle checkpoint counts as clean"
    );
    // Tampering with the raw payload is still caught before any migration runs.
    let text = std::fs::read_to_string(&t.path).unwrap();
    std::fs::write(&t.path, text.replacen("\"old\"", "\"new\"", 1)).unwrap();
    assert!(matches!(
        snapshot(&t.path).restore_latest(&reg),
        Err(RestoreError::HashMismatch { .. })
    ));
    // A malformed payload of a *current*-schema checkpoint is still Malformed.
    let mut current = serde_json::to_value(state(1, SessionStatus::Idle, "x")).unwrap();
    current.as_object_mut().unwrap().remove("messages");
    let bad = json!({"seq": 2, "ts": "t", "session_id": sid(), "kind": "checkpoint",
        "payload": {"turn": 1, "reason": "turn_end", "session_status": "idle", "state_hash": h("00"), "state": current}});
    let mut text = std::fs::read_to_string(&t.path).unwrap();
    text.push_str(&serde_json::to_string(&bad).unwrap());
    text.push('\n');
    std::fs::write(&t.path, text).unwrap();
    assert!(matches!(
        FileEventLog::snapshot(&t.path),
        Err(LogError::Malformed { line: 3, .. })
    ));
}

#[tokio::test]
async fn a_newer_state_schema_checkpoint_is_refused() {
    let t = tmp();
    let log = create(&t.path).await;
    let mut s = state(1, SessionStatus::Idle, "future");
    s.schema_version = STATE_SCHEMA_VERSION + 1;
    log.append(checkpoint(&s)).await.unwrap();
    let err = snapshot(&t.path)
        .restore_latest(&MigrationRegistry::new())
        .err()
        .unwrap();
    assert!(
        matches!(err, RestoreError::Migration(MigrationError::NewerThanSupported { found, supported })
            if found == STATE_SCHEMA_VERSION + 1 && supported == STATE_SCHEMA_VERSION),
        "{err:?}"
    );
}
