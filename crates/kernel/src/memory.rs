//! `Memory` (§3.11): functional memory snapshots. P1 ships the no-op implementation (D12);
//! P2.7 / ADR-0005 finalizes the shape.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hash::Hash;

/// Content-addressed pointer to a memory snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryPointer(pub Hash);

impl MemoryPointer {
    /// `MemoryPointer(Hash::of_canonical_json(&serde_json::json!(null)))`, i.e. the hash of `null`.
    pub fn empty() -> MemoryPointer {
        MemoryPointer(Hash::of_canonical_json(&Value::Null).expect("null is canonicalizable"))
    }
}

/// One remembered item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryItem {
    /// Item id, unique within a snapshot.
    pub id: String,
    /// Content.
    pub content: Value,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A retrieval query.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryQuery {
    /// Free text.
    pub text: String,
    /// Maximum items.
    pub limit: usize,
    /// Tag filter.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Functional memory: every mutation returns a new content-addressed pointer.
#[async_trait]
pub trait Memory: Send + Sync {
    /// `"noop"`, `"file_notes"`, ...
    fn name(&self) -> &str;
    /// Store an item into a new snapshot derived from `at` (or an empty one).
    async fn store(
        &self,
        at: Option<&MemoryPointer>,
        item: MemoryItem,
    ) -> Result<MemoryPointer, MemoryError>;
    /// Retrieve items.
    async fn retrieve(
        &self,
        at: &MemoryPointer,
        query: &MemoryQuery,
    ) -> Result<Vec<MemoryItem>, MemoryError>;
    /// Replace an item.
    async fn update(
        &self,
        at: &MemoryPointer,
        item: MemoryItem,
    ) -> Result<MemoryPointer, MemoryError>;
    /// Compress a snapshot.
    async fn compress(&self, at: &MemoryPointer) -> Result<MemoryPointer, MemoryError>;
    /// Remove an item.
    async fn forget(&self, at: &MemoryPointer, id: &str) -> Result<MemoryPointer, MemoryError>;
}

/// Memory errors.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    /// No such snapshot.
    #[error("memory snapshot {0:?} not found")]
    NotFound(MemoryPointer),
    /// No such item.
    #[error("item `{0}` not found")]
    NoItem(String),
    /// I/O failure.
    #[error("io error: {0}")]
    Io(String),
}

/// P1 default. `store`/`update`/`compress`/`forget` return `MemoryPointer::empty()`; `retrieve` returns `[]`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopMemory;

#[async_trait]
impl Memory for NoopMemory {
    fn name(&self) -> &str {
        "noop"
    }

    async fn store(
        &self,
        _at: Option<&MemoryPointer>,
        _item: MemoryItem,
    ) -> Result<MemoryPointer, MemoryError> {
        Ok(MemoryPointer::empty())
    }

    async fn retrieve(
        &self,
        _at: &MemoryPointer,
        _query: &MemoryQuery,
    ) -> Result<Vec<MemoryItem>, MemoryError> {
        Ok(Vec::new())
    }

    async fn update(
        &self,
        _at: &MemoryPointer,
        _item: MemoryItem,
    ) -> Result<MemoryPointer, MemoryError> {
        Ok(MemoryPointer::empty())
    }

    async fn compress(&self, _at: &MemoryPointer) -> Result<MemoryPointer, MemoryError> {
        Ok(MemoryPointer::empty())
    }

    async fn forget(&self, _at: &MemoryPointer, _id: &str) -> Result<MemoryPointer, MemoryError> {
        Ok(MemoryPointer::empty())
    }
}
