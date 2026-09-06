//! In-memory `EventLog` for tests and for the P1.2 loop tests.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

use super::reader;
use super::{EventLog, EventLogReader, LogError, RestoreError};
use crate::SessionId;
use crate::event::{CheckpointPayload, Event, EventBody, WarningPayload};
use crate::hash::Hash;
use crate::redact::Redactor;
use crate::state::{Migrated, MigrationRegistry};

/// One written line: the event and its raw (post-redaction) payload JSON.
#[derive(Clone, Debug)]
pub struct Line {
    /// The event as written.
    pub event: Event,
    /// The payload as written, verbatim.
    pub raw_payload: Value,
}

/// In-memory implementation for tests.
pub struct MemoryEventLog {
    session_id: SessionId,
    redactor: Arc<Redactor>,
    lines: Arc<Mutex<Vec<Line>>>,
}

impl MemoryEventLog {
    /// An empty log. Unlike `FileEventLog::create`, this writes no `log_opened`; the caller
    /// (normally the kernel) appends it.
    pub fn new(session_id: SessionId, redactor: Arc<Redactor>) -> MemoryEventLog {
        MemoryEventLog {
            session_id,
            redactor,
            lines: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A log pre-filled with `lines` (to simulate an existing log in a new process).
    pub fn from_events(
        session_id: SessionId,
        redactor: Arc<Redactor>,
        events: Vec<Event>,
    ) -> MemoryEventLog {
        let lines = events
            .into_iter()
            .map(|event| Line {
                raw_payload: event.body.payload_value().unwrap_or(Value::Null),
                event,
            })
            .collect();
        MemoryEventLog {
            session_id,
            redactor,
            lines: Arc::new(Mutex::new(lines)),
        }
    }

    /// Every event written so far.
    pub fn events(&self) -> Vec<Event> {
        self.lines
            .lock()
            .map(|l| l.iter().map(|x| x.event.clone()).collect())
            .unwrap_or_default()
    }

    /// The redactor.
    pub fn redactor(&self) -> &Arc<Redactor> {
        &self.redactor
    }
}

/// Redact `body`'s payload with the writer pass; returns the redacted body, its raw payload, and
/// how many spans changed (a non-zero count is a `late_redaction`).
pub fn writer_redact(
    redactor: &Redactor,
    body: &EventBody,
) -> Result<(EventBody, Value, u32), LogError> {
    let kind = body.kind().to_owned();
    let mut payload = body
        .payload_value()
        .map_err(|e| LogError::Io(e.to_string()))?;
    let report = redactor.redact_value(&mut payload);
    if report.replacements == 0 {
        return Ok((body.clone(), payload, 0));
    }
    let raw = serde_json::json!({"kind": kind, "payload": payload});
    let redacted: EventBody =
        serde_json::from_value(raw).map_err(|e| LogError::Io(e.to_string()))?;
    Ok((redacted, payload, report.replacements))
}

#[async_trait]
impl EventLog for MemoryEventLog {
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    async fn append(&self, body: EventBody) -> Result<Event, LogError> {
        let (body, raw_payload, late) = writer_redact(&self.redactor, &body)?;
        let mut lines = self.lines.lock().map_err(|_| LogError::Closed)?;
        let seq = lines.len() as u64;
        let event = Event {
            seq,
            ts: crate::time::now_rfc3339_ms(),
            session_id: self.session_id.clone(),
            body,
        };
        lines.push(Line {
            event: event.clone(),
            raw_payload,
        });
        if late > 0 {
            let turn = event
                .body
                .payload_value()
                .ok()
                .and_then(|p| p.get("turn").and_then(Value::as_u64))
                .unwrap_or(0);
            let warn = EventBody::Warning(WarningPayload::kernel(
                turn,
                "late_redaction",
                "writer-side redaction changed a payload; an ingress path was missed",
                Some(serde_json::json!({"seq": seq, "replacements": late})),
            ));
            let wseq = lines.len() as u64;
            let raw_payload = warn.payload_value().unwrap_or(Value::Null);
            lines.push(Line {
                event: Event {
                    seq: wseq,
                    ts: crate::time::now_rfc3339_ms(),
                    session_id: self.session_id.clone(),
                    body: warn,
                },
                raw_payload,
            });
        }
        Ok(event)
    }

    fn last_seq(&self) -> Option<u64> {
        self.lines
            .lock()
            .ok()
            .and_then(|l| l.last().map(|x| x.event.seq))
    }

    fn reader(&self) -> Box<dyn EventLogReader> {
        let lines = self.lines.lock().map(|l| l.clone()).unwrap_or_default();
        Box::new(SnapshotReader::new(lines))
    }
}

/// A reader over a snapshot of lines. Shared by `MemoryEventLog` and `FileEventLog`.
pub struct SnapshotReader {
    lines: Vec<Line>,
    effective: Vec<Event>,
}

impl SnapshotReader {
    /// Build from physical lines.
    pub fn new(lines: Vec<Line>) -> SnapshotReader {
        let events: Vec<Event> = lines.iter().map(|l| l.event.clone()).collect();
        let effective = reader::effective(&events);
        SnapshotReader { lines, effective }
    }

    fn raw_payload(&self, seq: u64) -> Option<Value> {
        self.lines
            .iter()
            .find(|l| l.event.seq == seq)
            .map(|l| l.raw_payload.clone())
    }
}

impl EventLogReader for SnapshotReader {
    fn iter(&self) -> Box<dyn Iterator<Item = Result<Event, LogError>> + Send + '_> {
        Box::new(self.lines.iter().map(|l| Ok(l.event.clone())))
    }

    fn effective(&self) -> Box<dyn Iterator<Item = Result<Event, LogError>> + Send + '_> {
        Box::new(self.effective.iter().cloned().map(Ok))
    }

    fn latest_checkpoint(&self) -> Result<Option<(Event, CheckpointPayload)>, LogError> {
        Ok(reader::latest_checkpoint(&self.effective))
    }

    fn restore(
        &self,
        checkpoint_hash: &Hash,
        migrations: &MigrationRegistry,
    ) -> Result<Migrated, RestoreError> {
        reader::restore(
            &self.effective,
            &|seq| self.raw_payload(seq),
            Some(checkpoint_hash),
            migrations,
        )?
        .ok_or_else(|| RestoreError::NotFound(checkpoint_hash.clone()))
    }

    fn restore_latest(
        &self,
        migrations: &MigrationRegistry,
    ) -> Result<Option<Migrated>, RestoreError> {
        reader::restore(
            &self.effective,
            &|seq| self.raw_payload(seq),
            None,
            migrations,
        )
    }
}
