//! The event log (§3.14): append-only writer trait, reader trait, errors, and the in-memory
//! implementation. The file-backed JSONL implementation is `log::file` (P1.3).

use async_trait::async_trait;
use serde_json::Value;

use crate::SessionId;
use crate::event::{CheckpointPayload, Event, EventBody, RecoveredPayload};
use crate::hash::Hash;
use crate::state::{Migrated, MigrationError, MigrationRegistry, state_hash_of_raw};

pub mod file;
pub mod memory;
pub use file::FileEventLog;
pub use memory::MemoryEventLog;

/// Append-only writer. One log per session; never rewritten.
#[async_trait]
pub trait EventLog: Send + Sync {
    /// The session this log belongs to.
    fn session_id(&self) -> &SessionId;
    /// Assigns `seq` and `ts`, runs the writer-side redaction pass, writes one line, returns the
    /// event as written. Checkpoint events MUST be durable (`fsync`) before this returns.
    async fn append(&self, body: EventBody) -> Result<Event, LogError>;
    /// The last assigned seq, if any line was written.
    fn last_seq(&self) -> Option<u64>;
    /// Reader over what has been written so far.
    fn reader(&self) -> Box<dyn EventLogReader>;
}

/// Reader. `effective()` hides seq ranges voided by `recovered` events (`event-schema.md` §6).
pub trait EventLogReader: Send + Sync {
    /// Every physical event.
    fn iter(&self) -> Box<dyn Iterator<Item = Result<Event, LogError>> + Send + '_>;
    /// The effective log.
    fn effective(&self) -> Box<dyn Iterator<Item = Result<Event, LogError>> + Send + '_>;
    /// The latest effective checkpoint.
    fn latest_checkpoint(&self) -> Result<Option<(Event, CheckpointPayload)>, LogError>;
    /// Finds the latest effective `checkpoint` with this `state_hash`, migrates if needed, verifies the
    /// hash of the (pre-migration) payload, and returns the `State`.
    fn restore(
        &self,
        checkpoint_hash: &Hash,
        migrations: &MigrationRegistry,
    ) -> Result<Migrated, RestoreError>;
    /// Restores the latest effective checkpoint, if any.
    fn restore_latest(
        &self,
        migrations: &MigrationRegistry,
    ) -> Result<Option<Migrated>, RestoreError>;
}

/// Log errors.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    /// I/O failure.
    #[error("io error: {0}")]
    Io(String),
    /// A line that is not valid JSON or not an event.
    #[error("malformed line {line}: {source}")]
    Malformed {
        /// 1-based line number.
        line: u64,
        /// Cause.
        #[source]
        source: serde_json::Error,
    },
    /// A seq gap or regression.
    #[error("seq {found} out of order (expected {expected})")]
    SeqOrder {
        /// Found.
        found: u64,
        /// Expected.
        expected: u64,
    },
    /// The file belongs to another session.
    #[error("log is for session {found:?}, expected {expected:?}")]
    WrongSession {
        /// Found.
        found: SessionId,
        /// Expected.
        expected: SessionId,
    },
    /// The file was written by a newer event schema.
    #[error("log has event_schema_version {found}, this kernel reads up to {supported}")]
    NewerSchema {
        /// Found.
        found: u32,
        /// Supported.
        supported: u32,
    },
    /// The log was closed.
    #[error("log is closed")]
    Closed,
}

/// Restore errors.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    /// No checkpoint with that hash.
    #[error("no checkpoint with state_hash {0:?}")]
    NotFound(Hash),
    /// The recorded hash does not match the payload.
    #[error("checkpoint payload hash {computed:?} does not match recorded {recorded:?}")]
    HashMismatch {
        /// Recorded.
        recorded: Hash,
        /// Computed.
        computed: Hash,
    },
    /// Migration failed.
    #[error(transparent)]
    Migration(#[from] MigrationError),
    /// Log failure.
    #[error(transparent)]
    Log(#[from] LogError),
}

/// Shared reader logic over an in-memory snapshot of raw events (used by both log
/// implementations). `raw` holds the physical events with their `payload` still as JSON so
/// `restore` can hash the pre-migration state.
pub mod reader {
    use super::*;

    /// Compute the set of seqs voided by `recovered` events (transitively: a voided range that
    /// contains a `recovered` line voids that line's range too, since it is inside the newer range).
    pub fn discarded_seqs(events: &[Event]) -> Vec<(u64, u64)> {
        let mut ranges: Vec<(u64, u64)> = events
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::Recovered(RecoveredPayload {
                    discarded_seq: Some(r),
                    ..
                }) => Some((r.from, r.to)),
                _ => None,
            })
            .collect();
        // A `recovered` line that itself lies in a later range is voided; its range is a subset
        // of the later one (it precedes the later checkpoint), so no special handling is needed.
        ranges.sort_unstable();
        ranges
    }

    /// True iff `seq` is inside any discarded range.
    pub fn is_discarded(ranges: &[(u64, u64)], seq: u64) -> bool {
        ranges.iter().any(|(from, to)| *from <= seq && seq <= *to)
    }

    /// The effective events of a physical sequence.
    pub fn effective(events: &[Event]) -> Vec<Event> {
        let ranges = discarded_seqs(events);
        events
            .iter()
            .filter(|e| !is_discarded(&ranges, e.seq))
            .cloned()
            .collect()
    }

    /// The latest effective checkpoint.
    pub fn latest_checkpoint(effective: &[Event]) -> Option<(Event, CheckpointPayload)> {
        effective.iter().rev().find_map(|e| match &e.body {
            EventBody::Checkpoint(c) => Some((e.clone(), c.clone())),
            _ => None,
        })
    }

    /// `restore` over effective events plus their raw payloads (`event-schema.md` §6.2).
    /// `raw_payloads` maps seq → raw `payload` JSON of that event, needed to hash the
    /// pre-migration state exactly as written.
    pub fn restore(
        effective: &[Event],
        raw_payload_of: &dyn Fn(u64) -> Option<Value>,
        checkpoint_hash: Option<&Hash>,
        migrations: &MigrationRegistry,
    ) -> Result<Option<Migrated>, RestoreError> {
        let found = effective.iter().rev().find_map(|e| {
            checkpoint_hash_of(e)
                .filter(|h| checkpoint_hash.is_none_or(|want| want == h))
                .map(|h| (e.seq, h))
        });
        let Some((seq, recorded)) = found else {
            return match checkpoint_hash {
                Some(h) => Err(RestoreError::NotFound(h.clone())),
                None => Ok(None),
            };
        };
        let raw = raw_payload_of(seq).ok_or_else(|| RestoreError::NotFound(recorded.clone()))?;
        let raw_state = raw
            .get("state")
            .cloned()
            .ok_or_else(|| RestoreError::NotFound(recorded.clone()))?;
        let computed = state_hash_of_raw(&raw_state).map_err(|e| {
            RestoreError::Log(LogError::Io(format!("cannot hash checkpoint state: {e}")))
        })?;
        if computed != recorded {
            return Err(RestoreError::HashMismatch { recorded, computed });
        }
        Ok(Some(migrations.migrate_to_current(raw_state)?))
    }

    /// The `state_hash` of a checkpoint event. Besides typed `Checkpoint` events this also
    /// recognizes `Unknown { kind: "checkpoint" }`, which is how the file reader keeps an
    /// old-`schema_version` checkpoint whose `state` no longer deserializes into the current
    /// `State` (`kernel-interface.md` §3.4: migrations run on the raw JSON for exactly this case).
    pub fn checkpoint_hash_of(e: &Event) -> Option<Hash> {
        match &e.body {
            EventBody::Checkpoint(c) => Some(c.state_hash.clone()),
            EventBody::Unknown { kind, payload } if kind == "checkpoint" => payload
                .get("state_hash")
                .and_then(Value::as_str)
                .and_then(|s| Hash::parse(s).ok()),
            _ => None,
        }
    }

    /// A log ends cleanly iff its last effective event is `suspended`, `session_ended`,
    /// `session_failed`, or a `checkpoint` whose `session_status` is `idle` (§7.3).
    pub fn ends_cleanly(effective: &[Event]) -> bool {
        match effective.last().map(|e| &e.body) {
            Some(EventBody::Suspended(_))
            | Some(EventBody::SessionEnded(_))
            | Some(EventBody::SessionFailed(_)) => true,
            Some(EventBody::Checkpoint(c)) => c.session_status == crate::state::SessionStatus::Idle,
            // An old-schema checkpoint kept raw (see `checkpoint_hash_of`).
            Some(EventBody::Unknown { kind, payload }) if kind == "checkpoint" => {
                payload.get("session_status").and_then(Value::as_str) == Some("idle")
            }
            _ => false,
        }
    }
}

/// Shared writer-side logic (`event-schema.md` §4.1 pass 2, §1.2): the redaction pass and its
/// `late_redaction` follow-up, the fsync rule, and the on-disk envelope line. Both
/// `MemoryEventLog` and `FileEventLog` go through `stage`, so they cannot drift.
pub mod writer {
    use super::memory::{Line, writer_redact};
    use super::*;
    use crate::event::WarningPayload;
    use crate::redact::Redactor;

    /// What one `append` produces: the event as written (redacted, with `seq`/`ts` assigned) and
    /// the physical lines to store — the event line and, when the writer pass changed something,
    /// the `warning{class: "late_redaction"}` that follows it (`event-schema.md` §4.1).
    #[derive(Clone, Debug)]
    pub struct Staged {
        /// The event as written.
        pub event: Event,
        /// One or two lines, `seq` consecutive from `event.seq`.
        pub lines: Vec<Line>,
    }

    /// Stage `body` for writing at `next_seq`: run the writer-side redaction pass, assign `seq`
    /// and `ts`, and produce the `late_redaction` warning if the pass changed anything.
    pub fn stage(
        session_id: &SessionId,
        next_seq: u64,
        redactor: &Redactor,
        body: EventBody,
    ) -> Result<Staged, LogError> {
        let (body, raw_payload, late) = writer_redact(redactor, &body)?;
        let event = Event {
            seq: next_seq,
            ts: crate::time::now_rfc3339_ms(),
            session_id: session_id.clone(),
            body,
        };
        let mut lines = vec![Line {
            event: event.clone(),
            raw_payload,
        }];
        if late > 0 {
            let warn = late_redaction_warning(&event, late);
            let raw_payload = warn
                .payload_value()
                .map_err(|e| LogError::Io(e.to_string()))?;
            lines.push(Line {
                event: Event {
                    seq: next_seq + 1,
                    ts: crate::time::now_rfc3339_ms(),
                    session_id: session_id.clone(),
                    body: warn,
                },
                raw_payload,
            });
        }
        Ok(Staged { event, lines })
    }

    /// `warning{class: "late_redaction", detail: {seq, replacements}}` for `offending`.
    pub fn late_redaction_warning(offending: &Event, replacements: u32) -> EventBody {
        let turn = offending
            .body
            .payload_value()
            .ok()
            .and_then(|p| p.get("turn").and_then(Value::as_u64))
            .unwrap_or(0);
        EventBody::Warning(WarningPayload::kernel(
            turn,
            "late_redaction",
            "writer-side redaction changed a payload; an ingress path was missed",
            Some(serde_json::json!({"seq": offending.seq, "replacements": replacements})),
        ))
    }

    /// Whether the writer MUST `fsync` after this event (`event-schema.md` §1.2 requires it for
    /// `checkpoint`; the terminal/suspension records are synced too since they are the last thing
    /// a process writes before exiting).
    pub fn needs_fsync(body: &EventBody) -> bool {
        matches!(
            body,
            EventBody::Checkpoint(_)
                | EventBody::Suspended(_)
                | EventBody::SessionEnded(_)
                | EventBody::SessionFailed(_)
        )
    }

    /// One JSONL line for `event`, with `raw_payload` written verbatim, in the envelope member
    /// order of `event-schema.md` §1.1 (`seq`, `ts`, `session_id`, `kind`, `payload`), terminated
    /// by `\n`.
    pub fn envelope_line(event: &Event, raw_payload: &Value) -> Result<String, LogError> {
        let io = |e: serde_json::Error| LogError::Io(e.to_string());
        let mut s = String::with_capacity(128);
        s.push_str("{\"seq\":");
        s.push_str(&event.seq.to_string());
        s.push_str(",\"ts\":");
        s.push_str(&serde_json::to_string(&event.ts).map_err(io)?);
        s.push_str(",\"session_id\":");
        s.push_str(&serde_json::to_string(&event.session_id).map_err(io)?);
        s.push_str(",\"kind\":");
        s.push_str(&serde_json::to_string(event.body.kind()).map_err(io)?);
        s.push_str(",\"payload\":");
        s.push_str(&serde_json::to_string(raw_payload).map_err(io)?);
        s.push_str("}\n");
        Ok(s)
    }
}
