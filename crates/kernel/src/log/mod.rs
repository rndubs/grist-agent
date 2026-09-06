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
        let found = effective.iter().rev().find_map(|e| match &e.body {
            EventBody::Checkpoint(c) if checkpoint_hash.is_none_or(|h| h == &c.state_hash) => {
                Some((e.seq, c.state_hash.clone()))
            }
            _ => None,
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

    /// A log ends cleanly iff its last effective event is `suspended`, `session_ended`,
    /// `session_failed`, or a `checkpoint` whose `session_status` is `idle` (§7.3).
    pub fn ends_cleanly(effective: &[Event]) -> bool {
        match effective.last().map(|e| &e.body) {
            Some(EventBody::Suspended(_))
            | Some(EventBody::SessionEnded(_))
            | Some(EventBody::SessionFailed(_)) => true,
            Some(EventBody::Checkpoint(c)) => c.session_status == crate::state::SessionStatus::Idle,
            _ => false,
        }
    }
}
