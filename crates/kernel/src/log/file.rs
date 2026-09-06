//! File-backed JSONL event log (`kernel-interface.md` §3.14, `event-schema.md` §1, §6; P1.3).
//!
//! One file per session, one event per line, append-only. The writer never rewrites a complete
//! line. The single in-place operation this module ever performs is in [`FileEventLog::open`]:
//! a trailing partial line (a crash mid-write, no terminating `\n`) is treated as absent and the
//! file is truncated to the last complete newline so the next append does not glue itself onto
//! the torn tail. Complete lines are never touched.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::de::Error as _;
use serde_json::Value;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;

use super::memory::{Line, SnapshotReader};
use super::{EventLog, EventLogReader, LogError, writer};
use crate::event::{Event, EventBody, LogMode, LogOpenedPayload};
use crate::redact::Redactor;
use crate::{EVENT_SCHEMA_VERSION, KERNEL_VERSION, STATE_SCHEMA_VERSION, SessionId};

/// File-backed JSONL implementation (P1.3). `create` writes `log_opened`; `open` does not.
pub struct FileEventLog {
    path: PathBuf,
    session_id: SessionId,
    redactor: Arc<Redactor>,
    /// Held for the whole of `append` so `seq` assignment and the write are one critical section.
    file: tokio::sync::Mutex<File>,
    /// Every physical line written or read so far (the reader snapshot source).
    lines: Mutex<Vec<Line>>,
}

impl std::fmt::Debug for FileEventLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileEventLog")
            .field("path", &self.path)
            .field("session_id", &self.session_id)
            .field("last_seq", &self.last_seq())
            .finish()
    }
}

/// The result of parsing a log file's bytes.
#[derive(Clone, Debug)]
pub struct ParsedLog {
    /// Every complete, valid line in file order.
    pub lines: Vec<Line>,
    /// Byte length of the file up to and including the last `\n`; anything after it is a torn
    /// final line that a reader treats as absent.
    pub complete_len: usize,
    /// Whether bytes followed the last `\n`.
    pub truncated_tail: bool,
}

/// The session id a parsed log belongs to: the envelope of line 0.
impl ParsedLog {
    /// The session id from the first envelope.
    pub fn session_id(&self) -> Option<&SessionId> {
        self.lines.first().map(|l| &l.event.session_id)
    }
}

fn io_err(e: std::io::Error) -> LogError {
    LogError::Io(e.to_string())
}

fn malformed(line: u64, msg: impl std::fmt::Display) -> LogError {
    LogError::Malformed {
        line,
        source: serde_json::Error::custom(msg),
    }
}

/// Parse the bytes of a log file per `event-schema.md` §1 and check every MUST there:
/// - a torn final line (no `\n`) is dropped and reported in `truncated_tail`;
/// - any other unparsable line is `LogError::Malformed { line }` (1-based);
/// - line 0 MUST be `log_opened` and its `event_schema_version` MUST NOT exceed
///   `EVENT_SCHEMA_VERSION` (`NewerSchema`); the same check applies to `resumed`/`recovered`,
///   which carry the version a later kernel appended with (§1.3);
/// - `seq` MUST equal the physical line index (`SeqOrder`);
/// - every `session_id` MUST equal line 0's (`WrongSession`);
/// - unknown kinds are kept as `EventBody::Unknown`, never an error; so is a `checkpoint` whose
///   `state.schema_version` is older than this kernel's and no longer deserializes
///   (`kernel-interface.md` §3.4) — `restore` migrates it from the raw JSON.
pub fn parse_log(bytes: &[u8]) -> Result<ParsedLog, LogError> {
    let complete_len = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let truncated_tail = complete_len < bytes.len();
    let mut lines = Vec::new();
    let mut expected_session: Option<SessionId> = None;
    // Every complete line, without its terminating `\n` (an empty file has none).
    let body = &bytes[..complete_len.saturating_sub(1)];
    let complete_lines = if complete_len == 0 {
        None
    } else {
        Some(body.split(|b| *b == b'\n'))
    };
    for (idx, raw) in complete_lines.into_iter().flatten().enumerate() {
        let line_no = idx as u64 + 1;
        let text =
            std::str::from_utf8(raw).map_err(|e| malformed(line_no, format!("not UTF-8: {e}")))?;
        let env: RawEnvelope =
            serde_json::from_str(text).map_err(|source| LogError::Malformed {
                line: line_no,
                source,
            })?;
        let raw_payload = env.payload;
        let typed = serde_json::json!({"kind": env.kind, "payload": raw_payload});
        let body = match serde_json::from_value::<EventBody>(typed) {
            Ok(body) => body,
            // An older-schema checkpoint may not deserialize into the current `State`
            // (`kernel-interface.md` §3.4); keep it raw so `restore` can migrate it.
            Err(_) if env.kind == "checkpoint" && is_old_schema_state(&raw_payload) => {
                EventBody::Unknown {
                    kind: env.kind,
                    payload: raw_payload.clone(),
                }
            }
            Err(source) => {
                return Err(LogError::Malformed {
                    line: line_no,
                    source,
                });
            }
        };
        let event = Event {
            seq: env.seq,
            ts: env.ts,
            session_id: env.session_id,
            body,
        };
        let expected_seq = idx as u64;
        if event.seq != expected_seq {
            return Err(LogError::SeqOrder {
                found: event.seq,
                expected: expected_seq,
            });
        }
        match &expected_session {
            None => {
                let EventBody::LogOpened(opened) = &event.body else {
                    return Err(malformed(
                        line_no,
                        format!(
                            "first event must be `log_opened`, found `{}`",
                            event.body.kind()
                        ),
                    ));
                };
                check_schema(opened.event_schema_version)?;
                expected_session = Some(event.session_id.clone());
            }
            Some(expected) => {
                if &event.session_id != expected {
                    return Err(LogError::WrongSession {
                        found: event.session_id.clone(),
                        expected: expected.clone(),
                    });
                }
                match &event.body {
                    EventBody::Resumed(r) => check_schema(r.event_schema_version)?,
                    EventBody::Recovered(r) => check_schema(r.event_schema_version)?,
                    _ => {}
                }
            }
        }
        lines.push(Line { event, raw_payload });
    }
    Ok(ParsedLog {
        lines,
        complete_len,
        truncated_tail,
    })
}

/// The five envelope members (`event-schema.md` §1.1), payload untyped.
#[derive(serde::Deserialize)]
struct RawEnvelope {
    seq: u64,
    ts: String,
    session_id: SessionId,
    kind: String,
    payload: Value,
}

/// True iff `payload.state.schema_version` is an integer below `STATE_SCHEMA_VERSION`.
fn is_old_schema_state(payload: &Value) -> bool {
    payload
        .get("state")
        .and_then(|s| s.get("schema_version"))
        .and_then(Value::as_u64)
        .is_some_and(|v| v < u64::from(STATE_SCHEMA_VERSION))
}

fn check_schema(found: u32) -> Result<(), LogError> {
    if found > EVENT_SCHEMA_VERSION {
        return Err(LogError::NewerSchema {
            found,
            supported: EVENT_SCHEMA_VERSION,
        });
    }
    Ok(())
}

impl FileEventLog {
    /// Create a new log at `path` (the file MUST NOT exist: a log is never rewritten) and write
    /// `log_opened{mode: live}` at seq 0.
    pub async fn create(
        path: &Path,
        session_id: SessionId,
        redactor: Arc<Redactor>,
    ) -> Result<FileEventLog, LogError> {
        Self::create_with_mode(path, session_id, LogMode::Live, redactor).await
    }

    /// `create` with an explicit `log_opened.mode` (`event-schema.md` §5.4: the P1.4 replay
    /// driver writes `mode: replay`).
    pub async fn create_with_mode(
        path: &Path,
        session_id: SessionId,
        mode: LogMode,
        redactor: Arc<Redactor>,
    ) -> Result<FileEventLog, LogError> {
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(path)
            .await
            .map_err(io_err)?;
        let log = FileEventLog {
            path: path.to_path_buf(),
            session_id,
            redactor,
            file: tokio::sync::Mutex::new(file),
            lines: Mutex::new(Vec::new()),
        };
        log.append(EventBody::LogOpened(LogOpenedPayload {
            event_schema_version: EVENT_SCHEMA_VERSION,
            state_schema_version: STATE_SCHEMA_VERSION,
            kernel_version: KERNEL_VERSION.to_owned(),
            mode,
        }))
        .await?;
        Ok(log)
    }

    /// Open an existing log for appending. Parses and validates every line (see [`parse_log`]);
    /// the session id is taken from the envelope of line 0. A torn final line is dropped and the
    /// file is truncated to the last complete `\n` — the only in-place write ever made to a log.
    pub async fn open(path: &Path, redactor: Arc<Redactor>) -> Result<FileEventLog, LogError> {
        let bytes = tokio::fs::read(path).await.map_err(io_err)?;
        let parsed = parse_log(&bytes)?;
        let Some(session_id) = parsed.session_id().cloned() else {
            return Err(malformed(1, "empty log: no `log_opened` line"));
        };
        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .await
            .map_err(io_err)?;
        if parsed.truncated_tail {
            file.set_len(parsed.complete_len as u64)
                .await
                .map_err(io_err)?;
        }
        Ok(FileEventLog {
            path: path.to_path_buf(),
            session_id,
            redactor,
            file: tokio::sync::Mutex::new(file),
            lines: Mutex::new(parsed.lines),
        })
    }

    /// `open`, additionally requiring the file to belong to `expected` (`LogError::WrongSession`
    /// otherwise). This is what a launcher resuming `<session_id>.jsonl` should call.
    pub async fn open_for(
        path: &Path,
        expected: SessionId,
        redactor: Arc<Redactor>,
    ) -> Result<FileEventLog, LogError> {
        let log = Self::open(path, redactor).await?;
        if log.session_id != expected {
            return Err(LogError::WrongSession {
                found: log.session_id.clone(),
                expected,
            });
        }
        Ok(log)
    }

    /// A read-only snapshot reader over the file at `path` (synchronous; does not truncate a
    /// torn tail, it just ignores it). For tooling and tests that inspect a log another process
    /// is writing.
    pub fn snapshot(path: &Path) -> Result<SnapshotReader, LogError> {
        let bytes = std::fs::read(path).map_err(io_err)?;
        Ok(SnapshotReader::new(parse_log(&bytes)?.lines))
    }

    /// Read a whole file synchronously. For `replay::Cassette::read_from`: the cassette is, like
    /// the log, the kernel's own store and takes no policy (`tests/no_std_fs_process.rs`).
    pub(crate) fn read_file_bytes(path: &Path) -> Result<Vec<u8>, LogError> {
        std::fs::read(path).map_err(io_err)
    }

    /// Write (create or truncate) a whole file synchronously. For `replay::Cassette::write_to`.
    pub(crate) fn write_file_bytes(path: &Path, bytes: &[u8]) -> Result<(), LogError> {
        std::fs::write(path, bytes).map_err(io_err)
    }

    /// The file this log writes to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The redactor.
    pub fn redactor(&self) -> &Arc<Redactor> {
        &self.redactor
    }

    /// Every event written or read so far.
    pub fn events(&self) -> Vec<Event> {
        self.lines
            .lock()
            .map(|l| l.iter().map(|x| x.event.clone()).collect())
            .unwrap_or_default()
    }
}

#[async_trait]
impl EventLog for FileEventLog {
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    async fn append(&self, body: EventBody) -> Result<Event, LogError> {
        let mut file = self.file.lock().await;
        let next_seq = self.lines.lock().map_err(|_| LogError::Closed)?.len() as u64;
        let staged = writer::stage(&self.session_id, next_seq, &self.redactor, body)?;
        let mut buf = String::new();
        for line in &staged.lines {
            buf.push_str(&writer::envelope_line(&line.event, &line.raw_payload)?);
        }
        file.write_all(buf.as_bytes()).await.map_err(io_err)?;
        file.flush().await.map_err(io_err)?;
        if writer::needs_fsync(&staged.event.body) {
            file.sync_all().await.map_err(io_err)?;
        }
        self.lines
            .lock()
            .map_err(|_| LogError::Closed)?
            .extend(staged.lines);
        Ok(staged.event)
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
