//! Change lifecycle states as the operation log records them (R3.09, ADR
//! 0063).
//!
//! A transition of a change's lifecycle (proposed, withdrawn, landed…) is
//! recorded in an operation's [`ATTR_LIFECYCLE`] attribute, as compact
//! JSON ([`LifecycleRecord`], format 1). The daemon's rows in
//! `reef.sqlite3` hold the state; the attribute is the operation log's
//! record of each transition. A transition the client asked for is an
//! operation of its own with no view change ([`Repo::record_transitions`]);
//! the landing service (R3.13) will carry its transitions in the operation
//! that moves the destination, through [`OpMeta::transitions`].

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use futures::StreamExt as _;
use jj_lib::backend::BackendError;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::revset::RevsetExpression;
use jj_lib::transaction::TransactionCommitError;
use serde::{Deserialize, Serialize};

use super::oplog::{ATTR_LIFECYCLE, OpMeta};
use super::repo::{ChangeIdHex, CommitIdHex, OperationIdHex, Repo};
use super::revset;
use crate::error::{Classify, ErrorKind, classify_by_source};

/// The format [`LifecycleRecord::v`] holds.
pub const RECORD_FORMAT: u32 = 1;

/// A change's lifecycle state (R3.09).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    /// Not put forward; a change with no record is a draft.
    Draft,
    /// Put forward for its destination.
    Proposed,
    /// Taken by the landing service (R3.13).
    Queued,
    /// On its destination. Final.
    Landed,
    /// A reviewer said no.
    Declined,
    /// Its proposer took it back.
    Withdrawn,
    /// Replaced by another change. Final.
    Superseded,
}

impl LifecycleState {
    /// Every state, in order.
    pub const ALL: [LifecycleState; 7] = [
        LifecycleState::Draft,
        LifecycleState::Proposed,
        LifecycleState::Queued,
        LifecycleState::Landed,
        LifecycleState::Declined,
        LifecycleState::Withdrawn,
        LifecycleState::Superseded,
    ];

    /// The name records and columns use.
    pub fn as_str(self) -> &'static str {
        match self {
            LifecycleState::Draft => "draft",
            LifecycleState::Proposed => "proposed",
            LifecycleState::Queued => "queued",
            LifecycleState::Landed => "landed",
            LifecycleState::Declined => "declined",
            LifecycleState::Withdrawn => "withdrawn",
            LifecycleState::Superseded => "superseded",
        }
    }

    /// Whether no transition leaves it.
    pub fn is_final(self) -> bool {
        matches!(self, LifecycleState::Landed | LifecycleState::Superseded)
    }
}

impl fmt::Display for LifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Text that names no [`LifecycleState`].
#[derive(Debug, thiserror::Error)]
#[error("{0:?} is not a lifecycle state")]
pub struct UnknownState(pub String);

impl FromStr for LifecycleState {
    type Err = UnknownState;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        LifecycleState::ALL
            .into_iter()
            .find(|state| state.as_str() == text)
            .ok_or_else(|| UnknownState(text.to_owned()))
    }
}

/// Why a transition was recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionCause {
    /// A client asked for it (`TransitionChange`).
    Request,
    /// It reverts an earlier transition (`UndoOperation`).
    Undo,
    /// The daemon saw the change on `trunk()`.
    Observed,
}

impl TransitionCause {
    /// The name records and columns use.
    pub fn as_str(self) -> &'static str {
        match self {
            TransitionCause::Request => "request",
            TransitionCause::Undo => "undo",
            TransitionCause::Observed => "observed",
        }
    }
}

impl FromStr for TransitionCause {
    type Err = UnknownState;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        [
            TransitionCause::Request,
            TransitionCause::Undo,
            TransitionCause::Observed,
        ]
        .into_iter()
        .find(|cause| cause.as_str() == text)
        .ok_or_else(|| UnknownState(text.to_owned()))
    }
}

/// One transition as an operation's [`ATTR_LIFECYCLE`] holds it. Empty
/// strings stand for absent values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionRecord {
    /// The transition's id, `trn_<16 hex>`, which the daemon's row has too.
    pub id: String,
    /// The change, in hex.
    pub change: String,
    /// Its commit when the transition was recorded.
    pub commit: String,
    /// The state before.
    pub from: LifecycleState,
    /// The state after.
    pub to: LifecycleState,
    /// Why.
    pub cause: TransitionCause,
    /// The proposal round it belongs to: 1 for the first proposal.
    pub round: u32,
    /// The reason its performer gave.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// For a supersede, the superseding change in hex.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub superseded_by: String,
    /// The bookmark the change is proposed for.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub destination: String,
    /// For an undo, the operation it reverts.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub undoes: String,
}

/// An operation's [`ATTR_LIFECYCLE`] value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleRecord {
    /// [`RECORD_FORMAT`].
    pub v: u32,
    /// The transitions, in the order they apply.
    pub transitions: Vec<TransitionRecord>,
}

impl LifecycleRecord {
    /// `transitions` as the attribute's value.
    pub fn encode(transitions: &[TransitionRecord]) -> String {
        let record = LifecycleRecord {
            v: RECORD_FORMAT,
            transitions: transitions.to_vec(),
        };
        serde_json::to_string(&record).unwrap_or_default()
    }
}

/// The transitions an operation's attributes record; empty when there are
/// none, or when the attribute is not format 1 (another tool wrote it, or a
/// later Reef), which is logged.
pub fn transitions_of(attributes: &BTreeMap<String, String>) -> Vec<TransitionRecord> {
    let Some(value) = attributes.get(ATTR_LIFECYCLE) else {
        return Vec::new();
    };
    match serde_json::from_str::<LifecycleRecord>(value) {
        Ok(record) if record.v == RECORD_FORMAT => record.transitions,
        Ok(record) => {
            tracing::debug!(
                "a lifecycle record of format {}, not {RECORD_FORMAT}",
                record.v
            );
            Vec::new()
        }
        Err(error) => {
            tracing::debug!(error = %error, "an unreadable lifecycle record");
            Vec::new()
        }
    }
}

/// Failure to record transitions or to look for changes on `trunk()`.
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    /// The commit had no transitions to record.
    #[error("no transitions to record")]
    Empty,
    /// The operation could not be written.
    #[error("record the transitions")]
    Commit(#[from] TransactionCommitError),
    /// The store or the index failed.
    #[error("read the repository")]
    Backend(#[from] BackendError),
    /// The revset could not be evaluated.
    #[error("evaluate the trunk revset: {0}")]
    Revset(String),
}

impl Classify for LifecycleError {
    fn kind(&self) -> ErrorKind {
        match self {
            LifecycleError::Empty => ErrorKind::InvalidArgument,
            LifecycleError::Revset(_) => ErrorKind::Internal,
            other => classify_by_source(other),
        }
    }
}

impl Repo {
    /// Records `meta.transitions` in one operation that changes nothing in
    /// the view, which this [`Repo`] adopts.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn record_transitions(
        &mut self,
        meta: &OpMeta,
    ) -> Result<OperationIdHex, LifecycleError> {
        if meta.transitions.is_empty() {
            return Err(LifecycleError::Empty);
        }
        let tx = self.repo.start_transaction();
        self.repo = meta.commit(tx).await?;
        Ok(OperationIdHex::of(self.repo.op_id()))
    }
}

/// Where an operation that recorded a transition was found by
/// [`find_transitions`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundTransition {
    /// The operation.
    pub op_id: OperationIdHex,
    /// When it ended.
    pub end: std::time::SystemTime,
}

/// The operations, among `repo`'s head and its ancestors that ended at or
/// after `not_before`, whose [`ATTR_LIFECYCLE`] records a transition with
/// one of `ids`, by transition id. The walk goes newest first and stops at
/// the first operation older than `not_before`, so it reads only the
/// operations since then.
pub async fn find_transitions(
    repo: &ReadonlyRepo,
    ids: &std::collections::BTreeSet<String>,
    not_before: std::time::SystemTime,
) -> Result<BTreeMap<String, FoundTransition>, LifecycleError> {
    let mut found = BTreeMap::new();
    let mut ops = std::pin::pin!(jj_lib::op_walk::walk_ancestors(std::slice::from_ref(
        repo.operation()
    )));
    while let Some(op) = ops.next().await {
        let op = op.map_err(|error| LifecycleError::Revset(error.to_string()))?;
        let entry = super::oplog::entry(&op);
        if entry.end < not_before {
            break;
        }
        for transition in transitions_of(&entry.attributes) {
            if ids.contains(&transition.id) {
                found.insert(
                    transition.id,
                    FoundTransition {
                        op_id: entry.id.clone(),
                        end: entry.end,
                    },
                );
            }
        }
        if found.len() == ids.len() {
            break;
        }
    }
    Ok(found)
}

/// The operation `text` names (a full id or a unique prefix) when it
/// records lifecycle transitions and changes nothing in the view: the
/// operations the daemon undoes in its own records. `None` otherwise,
/// including when `text` resolves to nothing (the view's undo reports
/// that), and for an operation that also changed the view, which the view's
/// undo refuses.
pub async fn transition_only_operation(
    repo: &ReadonlyRepo,
    text: &str,
) -> Option<super::oplog::OpEntry> {
    let op = super::oplog::resolve_operation(repo, text).await.ok()?;
    if !op.metadata().attributes.contains_key(ATTR_LIFECYCLE) {
        return None;
    }
    let [parent] = op.parent_ids() else {
        return None;
    };
    let parent = repo.loader().load_operation(parent).await.ok()?;
    (parent.view_id() == op.view_id()).then(|| super::oplog::entry(&op))
}

/// Whether `repo`'s view has the local bookmark `name`, not conflicted.
pub fn local_bookmark_exists(repo: &ReadonlyRepo, name: &str) -> bool {
    repo.view()
        .get_local_bookmark(jj_lib::ref_name::RefName::new(name))
        .as_normal()
        .is_some()
}

/// Of `changes`, those with a visible commit that is an ancestor of
/// `trunk()` in `repo`, each with that commit (the newest, when a change has
/// several). Empty when `repo` has no `trunk()`.
pub async fn on_trunk(
    repo: &ReadonlyRepo,
    changes: &[ChangeIdHex],
) -> Result<Vec<(ChangeIdHex, CommitIdHex)>, LifecycleError> {
    let Some(trunk) = revset::trunk(repo) else {
        return Ok(Vec::new());
    };
    let mut found: BTreeMap<ChangeIdHex, CommitIdHex> = BTreeMap::new();
    for chunk in changes.chunks(500) {
        let mut ids = Vec::with_capacity(chunk.len());
        for change in chunk {
            let resolved = repo
                .resolve_change_id(&change.to_id())
                .await
                .map_err(|error| LifecycleError::Revset(error.to_string()))?;
            if let Some(targets) = resolved {
                ids.extend(targets.visible_with_offsets().map(|(_, id)| id.clone()));
            }
        }
        if ids.is_empty() {
            continue;
        }
        let evaluated = RevsetExpression::commits(ids)
            .intersection(&RevsetExpression::commit(trunk.commit_id.to_id()).ancestors())
            .evaluate(repo)
            .map_err(|error| LifecycleError::Revset(error.to_string()))?;
        // Newest first, so the first commit seen of a change is its newest.
        let mut stream = evaluated.commit_change_ids();
        while let Some(pair) = stream.next().await {
            let (commit, change) =
                pair.map_err(|error| LifecycleError::Revset(error.to_string()))?;
            found
                .entry(ChangeIdHex::of(&change))
                .or_insert_with(|| CommitIdHex::of(&commit));
        }
    }
    Ok(found.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip() {
        for state in LifecycleState::ALL {
            assert_eq!(state.as_str().parse::<LifecycleState>().unwrap(), state);
        }
        assert!("nope".parse::<LifecycleState>().is_err());
        assert!(LifecycleState::Landed.is_final());
        assert!(LifecycleState::Superseded.is_final());
        assert!(!LifecycleState::Declined.is_final());
    }

    #[test]
    fn record_round_trips_and_skips_empty_fields() {
        let record = TransitionRecord {
            id: "trn_0123456789abcdef".to_owned(),
            change: "ab".repeat(16),
            commit: "cd".repeat(20),
            from: LifecycleState::Draft,
            to: LifecycleState::Proposed,
            cause: TransitionCause::Request,
            round: 1,
            reason: String::new(),
            superseded_by: String::new(),
            destination: "main".to_owned(),
            undoes: String::new(),
        };
        let text = LifecycleRecord::encode(std::slice::from_ref(&record));
        assert!(!text.contains("reason"), "{text}");
        assert!(text.contains(r#""from":"draft""#), "{text}");
        let attributes = BTreeMap::from([(ATTR_LIFECYCLE.to_owned(), text)]);
        assert_eq!(transitions_of(&attributes), vec![record]);
    }

    #[test]
    fn other_formats_and_garbage_read_as_none() {
        for value in [r#"{"v":2,"transitions":[]}"#, "{", "[]"] {
            let attributes = BTreeMap::from([(ATTR_LIFECYCLE.to_owned(), value.to_owned())]);
            assert_eq!(transitions_of(&attributes), Vec::new());
        }
        assert_eq!(transitions_of(&BTreeMap::new()), Vec::new());
    }
}
