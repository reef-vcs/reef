//! Provenance records (R3.04, ADR 0058).
//!
//! Every commit the daemon records for a workspace (each snapshot that
//! commits, and each `EditFiles` naming the workspace) gets one record: an
//! in-toto statement in `reef-evidence`'s envelope, about the commit,
//! signed by a key the daemon holds in memory for the workspace's
//! provenance session ([`record`]). The session binds the workspace to an
//! operator identity and an actor by R3.03's issuance rules, fixed for the
//! session, and carries the context a client declared last ([`session`]).
//! Reads check every record again ([`read`]); GC compacts and expires them
//! ([`retention`]).
//!
//! The daemon signs only statements about commits it has just recorded:
//! [`reef_evidence::provenance::sign`] takes a record, never bytes, and the
//! held keys are reachable only through it. Recording never fails a
//! snapshot or a write: a failure is logged and counted
//! (`missing_records`).
//!
//! On one uid, a record shows what the daemon saw and what it was told, not
//! who typed what (S-67, S-68).

pub mod audit;
pub mod read;
pub mod record;
pub mod retention;
pub mod session;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use reef_lib::error::{Classify, ErrorKind};

use crate::config::ProvenanceSection;
use crate::db::{Db, DbError};
use crate::error::{Advice, Advise};
use crate::identity::issue::HeldKey;
use crate::identity::{Identities, IdentityError};
use crate::model::WorkspaceId;

/// How long before its window ends a held key is renewed.
pub const RENEW_BEFORE: Duration = Duration::from_mins(5);

/// The most change ids a listing filters on.
pub const MAX_CHANGE_IDS: usize = 100;

/// Why a provenance request failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProvenanceError {
    /// A declared field or a filter out of its bounds: `INVALID_PROVENANCE`.
    #[error("{field}: {message}")]
    Invalid {
        /// The field, as the API names it.
        field: &'static str,
        /// What is wrong with it.
        message: String,
    },
    /// `[provenance] enabled` is false: `PROVENANCE_DISABLED`.
    #[error("provenance is disabled: [provenance] enabled is false")]
    Disabled,
    /// No record of the commit: `PROVENANCE_NOT_FOUND`.
    #[error("commit {commit_id} has no provenance record")]
    CommitNotFound {
        /// The commit.
        commit_id: String,
        /// Its change; empty when unknown.
        change_id: String,
    },
    /// No record has the id or prefix: `PROVENANCE_NOT_FOUND`.
    #[error("no provenance record is {id}")]
    IdNotFound {
        /// The id or prefix.
        id: String,
    },
    /// A prefix of several records: `AMBIGUOUS_PROVENANCE_ID`.
    #[error("provenance id {id:?} is ambiguous; it matches {}", candidates.join(", "))]
    Ambiguous {
        /// The prefix.
        id: String,
        /// Up to ten of the ids it matches.
        candidates: Vec<String>,
    },
    /// The identity rules refused the session's identity or actor.
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// A task on the blocking pool did not finish.
    #[error("a provenance task failed: {0}")]
    Task(String),
    /// The workspace has ended: no session starts for it.
    #[error("workspace {0} has ended")]
    Ended(WorkspaceId),
}

impl Classify for ProvenanceError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Invalid { .. } | Self::Ambiguous { .. } => ErrorKind::InvalidArgument,
            Self::Disabled | Self::Ended(_) => ErrorKind::Conflict,
            Self::CommitNotFound { .. } | Self::IdNotFound { .. } => ErrorKind::NotFound,
            Self::Identity(error) => error.kind(),
            Self::Db(error) => error.kind(),
            Self::Task(_) => ErrorKind::Internal,
        }
    }
}

impl Advise for ProvenanceError {
    fn advice(&self) -> Option<Advice> {
        Some(match self {
            Self::Invalid { field, .. } => Advice::hint(match *field {
                "instruction_sha256" => {
                    "Send the instruction's SHA-256 as 64 lowercase hex digits, never its text: `reef provenance declare --instruction-file <path>` hashes a file."
                }
                "task" => "A task reference is 1 to 512 bytes with no control characters.",
                "identity" | "actor" => {
                    "A session's identity and actor are fixed when its workspace is created: create a new workspace for another."
                }
                "change_ids" => {
                    "Give at most 100 change ids, each 32 lowercase hex digits (`Change.change_id`)."
                }
                _ => {
                    "Model, harness and client session are 1 to 128 bytes of visible ASCII, with no spaces."
                }
            }),
            Self::Disabled => Advice::hint(
                "Set [provenance] enabled = true and restart the daemon to record provenance.",
            ),
            Self::CommitNotFound { change_id, .. } if !change_id.is_empty() => Advice::run(
                "Only commits a workspace's snapshot or edit recorded have a record; a describe, a rebase or the jj CLI writes none. List the change's records.",
                ["reef", "provenance", "list", "--change", change_id.as_str()],
            ),
            Self::CommitNotFound { .. } | Self::IdNotFound { .. } => Advice::run(
                "List the repository's provenance records.",
                ["reef", "provenance", "list"],
            ),
            Self::Ambiguous { .. } => {
                Advice::hint("Give more of the record id: `reef provenance list` prints them.")
            }
            Self::Identity(error) => return error.advice(),
            Self::Db(error) => return error.advice(),
            Self::Task(_) | Self::Ended(_) => return None,
        })
    }
}

impl ProvenanceError {
    /// The reason and metadata of `ErrorDetail`, for `crate::error`.
    pub(crate) fn reason(&self) -> Option<(&'static str, Vec<(&'static str, String)>)> {
        use reef_api::error as reason;
        Some(match self {
            Self::Invalid { field, .. } => (
                reason::INVALID_PROVENANCE,
                vec![("field", (*field).to_owned())],
            ),
            Self::Disabled => (reason::PROVENANCE_DISABLED, Vec::new()),
            Self::CommitNotFound {
                commit_id,
                change_id,
            } => (
                reason::PROVENANCE_NOT_FOUND,
                vec![
                    ("commit_id", commit_id.clone()),
                    ("change_id", change_id.clone()),
                ],
            ),
            Self::IdNotFound { id } => (reason::PROVENANCE_NOT_FOUND, vec![("id", id.clone())]),
            Self::Ambiguous { id, candidates } => (
                reason::AMBIGUOUS_PROVENANCE_ID,
                vec![("id", id.clone()), ("ids", candidates.join(","))],
            ),
            Self::Identity(error) => return error.reason(),
            Self::Db(_) | Self::Task(_) | Self::Ended(_) => return None,
        })
    }
}

/// A workspace's held state: the session its key is for, and the key.
#[derive(Debug, Default)]
pub(crate) struct Held {
    /// The session the key is certified for (or generated for, uncertified).
    pub(crate) session_id: String,
    /// The key.
    pub(crate) key: Option<HeldKey>,
}

/// What a revocation covered, for [`Provenance::revoked`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Revoked {
    /// One session key, by id.
    Key(String),
    /// A session, by id.
    Session(String),
    /// An identity, by id.
    Identity(String),
    /// An authority, by key id.
    Authority(String),
}

/// Which step [`record::record`] fails at, for the never-fail test.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FailAt {
    /// Nothing fails.
    Nothing = 0,
    /// Issuing the held key.
    Issue = 1,
    /// Signing.
    Sign = 2,
    /// The insert.
    Insert = 3,
}

/// The daemon's provenance state, in `AppState` and the workspace manager.
pub struct Provenance {
    pub(crate) db: Arc<Db>,
    pub(crate) identities: Arc<Identities>,
    pub(crate) settings: ProvenanceSection,
    /// One lock per workspace: recording, declaring and starting a session
    /// hold it, so no record sees half a declaration and a session's records
    /// are numbered in order.
    held: Mutex<HashMap<WorkspaceId, Arc<Mutex<Held>>>>,
    /// Test hook: fail one step of every record.
    fail_at: AtomicU8,
    /// Test hook: a held key's life instead of `[session_keys]
    /// max_lifetime`.
    lifetime: Mutex<Option<Duration>>,
}

impl std::fmt::Debug for Provenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provenance")
            .field("enabled", &self.settings.enabled)
            .finish_non_exhaustive()
    }
}

impl Provenance {
    /// The provenance state over the database and the identity state.
    pub fn new(
        db: Arc<Db>,
        identities: Arc<Identities>,
        settings: ProvenanceSection,
    ) -> Provenance {
        Provenance {
            db,
            identities,
            settings,
            held: Mutex::new(HashMap::new()),
            fail_at: AtomicU8::new(FailAt::Nothing as u8),
            lifetime: Mutex::new(None),
        }
    }

    /// Whether records are recorded (`[provenance] enabled`).
    pub fn enabled(&self) -> bool {
        self.settings.enabled
    }

    /// `[provenance]`.
    pub fn settings(&self) -> &ProvenanceSection {
        &self.settings
    }

    /// The lock of workspace `id`'s held state.
    pub(crate) fn held(&self, id: WorkspaceId) -> Arc<Mutex<Held>> {
        Arc::clone(
            self.held
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(id)
                .or_default(),
        )
    }

    /// Forgets workspace `id`'s held key: its session ended.
    pub(crate) fn forget(&self, id: WorkspaceId) {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }

    /// Drops the held keys a revocation (R3.03) covers: the next record of
    /// each of their workspaces issues a new key by the rules as they now
    /// stand. Keys it does not cover are kept, so a revocation does not make
    /// every live workspace issue a new key.
    pub fn revoked(&self, scope: &Revoked) {
        let held: Vec<Arc<Mutex<Held>>> = self
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        let mut dropped = 0_usize;
        for held in held {
            let mut held = held.lock().unwrap_or_else(PoisonError::into_inner);
            let covered = held.key.as_ref().is_some_and(|key| match scope {
                Revoked::Key(id) => key.key_id == *id,
                Revoked::Session(id) => held.session_id == *id,
                Revoked::Identity(id) => key.identity_id.as_ref() == Some(id),
                Revoked::Authority(id) => key.authority_id.as_ref() == Some(id),
            });
            if covered {
                held.key = None;
                dropped += 1;
            }
        }
        if dropped > 0 {
            tracing::info!(
                target: "reef_daemon::provenance::audit",
                held_key_count = dropped,
                "dropped held provenance keys after a revocation"
            );
        }
    }

    /// The life of a new held key.
    pub(crate) fn key_lifetime(&self) -> Duration {
        self.lifetime
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .unwrap_or(self.identities.settings().max_lifetime)
    }

    /// Test hook: issue held keys for `lifetime` instead.
    #[doc(hidden)]
    pub fn set_key_lifetime(&self, lifetime: Option<Duration>) {
        *self.lifetime.lock().unwrap_or_else(PoisonError::into_inner) = lifetime;
    }

    /// Test hook: fail `step` of every record from now on.
    #[doc(hidden)]
    pub fn set_fail_at(&self, step: FailAt) {
        self.fail_at.store(step as u8, Ordering::Relaxed);
    }

    pub(crate) fn fails_at(&self, step: FailAt) -> bool {
        self.fail_at.load(Ordering::Relaxed) == step as u8
    }
}
