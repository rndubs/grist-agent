//! File-backed JSONL event log (`kernel-interface.md` §3.14, `event-schema.md` §1): P1.3 fills this in.

/// File-backed JSONL implementation (P1.3). `create` writes `log_opened`; `open` does not.
pub struct FileEventLog;
