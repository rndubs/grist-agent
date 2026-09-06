//! `State` (§3.4): the checkpointed session state, its hash, and schema migrations.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::Message;
use crate::hash::{Hash, HashError};
use crate::memory::MemoryPointer;
use crate::task::{Task, TaskId};
use crate::{STATE_SCHEMA_VERSION, SessionId};

/// Session states (D2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Created, no user message yet.
    Created,
    /// A turn is running or about to run.
    Running,
    /// Waiting for the user.
    Idle,
    /// Waiting for a waker.
    Suspended,
    /// Explicitly ended.
    Done,
    /// Last checkpoint kept, resumable.
    Failed,
}

/// Hashes of the profiles active in a session (D13).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveProfiles {
    /// Model profile source hash.
    pub model_profile_hash: Hash,
    /// Agent profile source hash.
    pub agent_profile_hash: Hash,
    /// Hash of the fully resolved profile (kernel defaults + model + agent + project overrides), D7.
    pub resolved_profile_hash: Hash,
    /// `<workdir>/.grist/agent.toml` when present (`profile-schema.md` layer 3).
    pub project_profile_hash: Option<Hash>,
    /// `profiles/bundles.toml`.
    pub bundles_hash: Option<Hash>,
}

/// The checkpointed session state (§4.2 of the dev plan). Field order is the serialized order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// MUST be the first field. Equals `STATE_SCHEMA_VERSION` for states this build writes.
    pub schema_version: u32,
    /// Volatile (excluded from `state_hash`).
    pub session_id: SessionId,
    /// RFC 3339 UTC. Volatile.
    pub created_at: String,
    /// Number of turns started (incremented at the start of each turn). 0 before the first turn.
    pub turn: u64,
    /// Session status (D2).
    pub session_status: SessionStatus,
    /// The conversation.
    pub messages: Vec<Message>,
    /// Every task ever started in this session, keyed by id; terminal tasks stay (D1 needs their outcome for provenance).
    pub pending_tasks: BTreeMap<TaskId, Task>,
    /// Active profile hashes.
    pub profiles: ActiveProfiles,
    /// `None` until a `Memory` implementation writes one (P2.7).
    pub memory: Option<MemoryPointer>,
    /// Lab notebook path (P2.6). `None` when the agent profile declares none.
    pub notebook_path: Option<PathBuf>,
    /// Hash of the session envelope policy, `derive_policy_with(grants, grants, limits)` (§3.12). Volatile.
    pub sandbox_policy_hash: Hash,
    /// `SandboxBackend::name()` in use, e.g. `"bwrap"` or `"none"` (D14). Volatile.
    pub sandbox_backend: String,
}

impl State {
    /// The volatile field names, in serialized order. `event-schema.md` §3.5 is the canonical,
    /// normative list; this constant MUST match it and a test asserts that `state_hash` ignores
    /// exactly these.
    pub const VOLATILE_FIELDS: &'static [&'static str] = &[
        "session_id",
        "created_at",
        "sandbox_policy_hash",
        "sandbox_backend",
    ];

    /// `Hash::of_canonical_json` of `self` with the volatile fields removed (D3).
    pub fn state_hash(&self) -> Result<Hash, HashError> {
        let raw = serde_json::to_value(self)?;
        state_hash_of_raw(&raw)
    }

    /// Tasks in `Pending` or `Running`.
    pub fn open_tasks(&self) -> impl Iterator<Item = &Task> {
        self.pending_tasks.values().filter(|t| t.is_open())
    }
}

/// `state_hash` over a raw JSON state (used by `restore` before migration).
pub fn state_hash_of_raw(raw: &Value) -> Result<Hash, HashError> {
    let mut raw = raw.clone();
    if let Some(obj) = raw.as_object_mut() {
        for f in State::VOLATILE_FIELDS {
            obj.remove(*f);
        }
    } else {
        return Err(HashError::NotCanonicalizable(
            "state is not a JSON object".to_owned(),
        ));
    }
    Hash::of_canonical_json(&raw)
}

/// A single-step migration from `from_version()` to `from_version() + 1`.
pub trait StateMigration: Send + Sync {
    /// The schema version this migration reads.
    #[allow(clippy::wrong_self_convention)] // Spec-fixed name (`kernel-interface.md` §3.4).
    fn from_version(&self) -> u32;
    /// Rewrite the raw JSON of a `from_version()` state into a `from_version() + 1` state.
    fn migrate(&self, raw: Value) -> Result<Value, MigrationError>;
}

/// Registered migrations, applied in order on raw JSON.
#[derive(Default)]
pub struct MigrationRegistry {
    steps: BTreeMap<u32, Box<dyn StateMigration>>,
}

impl std::fmt::Debug for MigrationRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MigrationRegistry")
            .field("from_versions", &self.steps.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Result of `migrate_to_current`.
pub struct Migrated {
    /// The current-version state.
    pub state: State,
    /// `Some((from, to))` if any migration ran; the kernel then logs a `warning{class: "state_migrated"}`.
    pub migrated: Option<(u32, u32)>,
}

/// Migration errors. Every case fails loudly; there is no best-effort parse.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// The checkpoint was written by a newer kernel.
    #[error("checkpoint has schema_version {found}, this kernel supports up to {supported}")]
    NewerThanSupported {
        /// Version found.
        found: u32,
        /// Version supported.
        supported: u32,
    },
    /// No migration registered for a needed step.
    #[error("no migration registered from schema_version {from} (target {to})")]
    MissingStep {
        /// Version at which the chain stops.
        from: u32,
        /// Target version.
        to: u32,
    },
    /// A migration step failed.
    #[error("migration {from}->{} failed: {source}", .from + 1)]
    Failed {
        /// The step's source version.
        from: u32,
        /// Cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// No integer `schema_version` in the raw state.
    #[error("checkpoint has no integer `schema_version`")]
    NoVersion,
    /// A migration for this version is already registered.
    #[error("migration {from} already registered")]
    Duplicate {
        /// The duplicated source version.
        from: u32,
    },
    /// The migrated JSON does not deserialize into `State`.
    #[error("state does not deserialize after migration: {0}")]
    Invalid(#[from] serde_json::Error),
}

impl MigrationRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Errors if a migration for the same `from_version` is already registered.
    pub fn register(&mut self, m: Box<dyn StateMigration>) -> Result<(), MigrationError> {
        let from = m.from_version();
        if self.steps.contains_key(&from) {
            return Err(MigrationError::Duplicate { from });
        }
        self.steps.insert(from, m);
        Ok(())
    }

    /// Reads `schema_version` from `raw`, applies migrations in order up to `STATE_SCHEMA_VERSION`,
    /// then deserializes. Fails loudly: a missing step is an error, never a best-effort parse.
    pub fn migrate_to_current(&self, raw: Value) -> Result<Migrated, MigrationError> {
        let found = raw
            .get("schema_version")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or(MigrationError::NoVersion)?;
        if found > STATE_SCHEMA_VERSION {
            return Err(MigrationError::NewerThanSupported {
                found,
                supported: STATE_SCHEMA_VERSION,
            });
        }
        let mut raw = raw;
        let mut version = found;
        while version < STATE_SCHEMA_VERSION {
            let step = self
                .steps
                .get(&version)
                .ok_or(MigrationError::MissingStep {
                    from: version,
                    to: STATE_SCHEMA_VERSION,
                })?;
            raw = step.migrate(raw)?;
            version += 1;
            // A migration MUST stamp the new version; enforce it so a lazy step cannot loop.
            let stamped = raw
                .get("schema_version")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok());
            if stamped != Some(version) {
                return Err(MigrationError::Failed {
                    from: version - 1,
                    source: format!(
                        "migration did not set schema_version to {version} (found {stamped:?})"
                    )
                    .into(),
                });
            }
        }
        let state: State = serde_json::from_value(raw)?;
        Ok(Migrated {
            state,
            migrated: (found != STATE_SCHEMA_VERSION).then_some((found, STATE_SCHEMA_VERSION)),
        })
    }
}
