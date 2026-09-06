//! `ArtifactStore` (§3.10): content-addressed blobs. P1 ships the no-op store (D12).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::hash::Hash;

/// Content-addressed handle: `Hash::of_bytes(content)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArtifactHandle(pub Hash);

/// Size and type of a stored artifact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMeta {
    /// Size in bytes.
    pub size: u64,
    /// MIME type.
    pub mime: String,
}

/// The shape that replaces a spilled result in the context (§7.4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Spilled {
    /// Where the full bytes live.
    pub handle: ArtifactHandle,
    /// First bytes, cut back to a UTF-8 boundary.
    pub head: String,
    /// Last bytes, cut back to a UTF-8 boundary.
    pub tail: String,
    /// Full size in bytes.
    pub size: u64,
    /// MIME type of the full bytes.
    pub mime: String,
}

/// Content-addressed artifact storage.
#[async_trait]
pub trait ArtifactStore: Send + Sync {
    /// `"noop"`, `"fs"`, ...
    fn name(&self) -> &str;
    /// Idempotent: putting the same bytes twice returns the same handle.
    async fn put(&self, bytes: &[u8], mime: &str) -> Result<ArtifactHandle, ArtifactError>;
    /// Full bytes.
    async fn get(&self, handle: &ArtifactHandle) -> Result<Vec<u8>, ArtifactError>;
    /// A byte range.
    async fn get_range(
        &self,
        handle: &ArtifactHandle,
        range: std::ops::Range<u64>,
    ) -> Result<Vec<u8>, ArtifactError>;
    /// First `bytes` bytes, cut back to a UTF-8 boundary when `mime` is text-like.
    async fn head(&self, handle: &ArtifactHandle, bytes: u64) -> Result<Vec<u8>, ArtifactError>;
    /// Last `bytes` bytes, cut forward to a UTF-8 boundary when `mime` is text-like.
    async fn tail(&self, handle: &ArtifactHandle, bytes: u64) -> Result<Vec<u8>, ArtifactError>;
    /// Metadata.
    async fn stat(&self, handle: &ArtifactHandle) -> Result<ArtifactMeta, ArtifactError>;
}

/// Artifact store errors.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    /// No such artifact.
    #[error("artifact {0:?} not found")]
    NotFound(ArtifactHandle),
    /// The store refuses writes.
    #[error("store is read-only")]
    ReadOnly,
    /// I/O failure.
    #[error("io error: {0}")]
    Io(String),
}

/// P1 default (D12). `put` hashes and returns a handle but stores nothing; every read is `NotFound`.
/// Spill still works because the kernel computes `head`/`tail` from the bytes it has in hand.
/// A kernel constructed with it logs `warning{class: "artifact_store_noop"}` at session start.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopArtifactStore;

#[async_trait]
impl ArtifactStore for NoopArtifactStore {
    fn name(&self) -> &str {
        "noop"
    }

    async fn put(&self, bytes: &[u8], _mime: &str) -> Result<ArtifactHandle, ArtifactError> {
        Ok(ArtifactHandle(Hash::of_bytes(bytes)))
    }

    async fn get(&self, handle: &ArtifactHandle) -> Result<Vec<u8>, ArtifactError> {
        Err(ArtifactError::NotFound(handle.clone()))
    }

    async fn get_range(
        &self,
        handle: &ArtifactHandle,
        _range: std::ops::Range<u64>,
    ) -> Result<Vec<u8>, ArtifactError> {
        Err(ArtifactError::NotFound(handle.clone()))
    }

    async fn head(&self, handle: &ArtifactHandle, _bytes: u64) -> Result<Vec<u8>, ArtifactError> {
        Err(ArtifactError::NotFound(handle.clone()))
    }

    async fn tail(&self, handle: &ArtifactHandle, _bytes: u64) -> Result<Vec<u8>, ArtifactError> {
        Err(ArtifactError::NotFound(handle.clone()))
    }

    async fn stat(&self, handle: &ArtifactHandle) -> Result<ArtifactMeta, ArtifactError> {
        Err(ArtifactError::NotFound(handle.clone()))
    }
}
