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
    /// The landing service queued, returned or landed it (R3.13).
    Landing,
    /// The daemon carried a proposal across a rewrite of a change below it
    /// (R3.11).
    Dependency,
}

impl TransitionCause {
    /// The name records and columns use.
    pub fn as_str(self) -> &'static str {
        match self {
            TransitionCause::Request => "request",
            TransitionCause::Undo => "undo",
            TransitionCause::Observed => "observed",
            TransitionCause::Landing => "landing",
            TransitionCause::Dependency => "dependency",
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
            TransitionCause::Landing,
            TransitionCause::Dependency,
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
    /// For a carry (cause `dependency`, R3.11), the change below that moved,
    /// in hex.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dependency: String,
    /// For a carry, what happened to it ([`DependencyEvent::as_str`]).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dependency_event: String,
}

/// What happened to the change below a carried proposal (R3.11).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DependencyEvent {
    /// Rewritten while not landed: amended, described, rebased.
    Amended,
    /// A landing rewrote it (R3.13, R3.16).
    Landed,
    /// It was superseded, and the proposal moved onto its successor.
    Superseded,
}

impl DependencyEvent {
    /// The name records and columns use.
    pub fn as_str(self) -> &'static str {
        match self {
            DependencyEvent::Amended => "amended",
            DependencyEvent::Landed => "landed",
            DependencyEvent::Superseded => "superseded",
        }
    }
}

impl FromStr for DependencyEvent {
    type Err = UnknownState;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        [
            DependencyEvent::Amended,
            DependencyEvent::Landed,
            DependencyEvent::Superseded,
        ]
        .into_iter()
        .find(|event| event.as_str() == text)
        .ok_or_else(|| UnknownState(text.to_owned()))
    }
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
/// records lifecycle transitions: the operations the daemon undoes in its
/// own records, with whether it also changed the view (a supersede that
/// moved its dependents, R3.11), which the undo then reverts in the same
/// operation. `None` otherwise, including when `text` resolves to nothing
/// (the view's undo reports that).
pub async fn transition_operation(
    repo: &ReadonlyRepo,
    text: &str,
) -> Option<(super::oplog::OpEntry, bool)> {
    let op = super::oplog::resolve_operation(repo, text).await.ok()?;
    if !op.metadata().attributes.contains_key(ATTR_LIFECYCLE) {
        return None;
    }
    let [parent] = op.parent_ids() else {
        return None;
    };
    let parent = repo.loader().load_operation(parent).await.ok()?;
    Some((super::oplog::entry(&op), parent.view_id() != op.view_id()))
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
            dependency: String::new(),
            dependency_event: String::new(),
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

/// Whether a proposal recorded at one commit follows its change to another
/// (R3.11): see [`carry_check`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CarryCheck {
    /// The visible commit is the recorded one rebased onto new commits of
    /// the same parent changes: `moved` names those whose commit changed,
    /// in parent order.
    Carried {
        /// The parent changes that moved, in hex.
        moved: Vec<ChangeIdHex>,
        /// A parent was a superseded change, and the commit is on its
        /// successor.
        renamed: bool,
    },
    /// It is not, and why.
    Not(NotCarried),
}

/// Why [`carry_check`] does not carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotCarried {
    /// The visible commit moved onto new commits of the same parent
    /// changes (`moved`), and has a conflict: a rewrite below it left it
    /// conflicted.
    Conflicted {
        /// The parent changes that moved, in parent order.
        moved: Vec<ChangeIdHex>,
    },
    /// Its parents are other changes than the recorded commit's.
    Parents,
    /// Its own diff or description is not the recorded commit's.
    OwnEdit,
    /// The recorded commit is not in the store: garbage collection took it.
    Gone,
}

/// [`carry_check`]'s verdicts that do not carry, by the recorded and the
/// visible commit: each holds as long as the renamed (superseded) changes
/// it was given stay the same, so a caller that remembers them checks each
/// pair once.
pub type Verdicts = std::collections::HashMap<(CommitIdHex, CommitIdHex), NotCarried>;

/// Whether the proposal recorded at `recorded` is carried to `visible`, its
/// change's commit now (R3.11, ADR 0084): `visible`'s parents are commits
/// of the recorded commit's parent changes (each renamed through
/// `renamed`, `(old, new)`, for a superseded parent), at least one of them
/// a new commit, it has no conflict, and rebasing `recorded` onto them gives
/// exactly `visible`'s tree and description. Merges the trees in a
/// transaction it drops: writes trees to the store, but no commit and no
/// operation.
pub async fn carry_check(
    repo: &std::sync::Arc<ReadonlyRepo>,
    recorded: &CommitIdHex,
    visible: &CommitIdHex,
    renamed: &[(ChangeIdHex, ChangeIdHex)],
) -> Result<CarryCheck, BackendError> {
    let store = repo.store();
    let old = match store.get_commit_async(&recorded.to_id()).await {
        Ok(old) => old,
        Err(BackendError::ObjectNotFound { .. }) => return Ok(CarryCheck::Not(NotCarried::Gone)),
        Err(error) => return Err(error),
    };
    let new = store.get_commit_async(&visible.to_id()).await?;
    if old.parent_ids().len() != new.parent_ids().len() {
        return Ok(CarryCheck::Not(NotCarried::Parents));
    }
    let mut moved = Vec::new();
    let mut was_renamed = false;
    for (was, now) in old.parent_ids().iter().zip(new.parent_ids()) {
        let was_change = ChangeIdHex::of(store.get_commit_async(was).await?.change_id());
        let was_change = match renamed.iter().find(|(from, _)| *from == was_change) {
            Some((_, to)) => {
                was_renamed = true;
                to.clone()
            }
            None => was_change,
        };
        let now_change = ChangeIdHex::of(store.get_commit_async(now).await?.change_id());
        if was_change != now_change {
            return Ok(CarryCheck::Not(NotCarried::Parents));
        }
        if was != now {
            moved.push(now_change);
        }
    }
    if moved.is_empty() {
        return Ok(CarryCheck::Not(NotCarried::OwnEdit));
    }
    if new.has_conflict() {
        return Ok(CarryCheck::Not(NotCarried::Conflicted { moved }));
    }
    if old.description() != new.description() {
        return Ok(CarryCheck::Not(NotCarried::OwnEdit));
    }
    let mut tx = repo.start_transaction();
    let rebased =
        jj_lib::rewrite::CommitRewriter::new(tx.repo_mut(), old, new.parent_ids().to_vec())
            .rebase()
            .await?;
    if rebased.tree_ids() != new.tree_ids() {
        return Ok(CarryCheck::Not(NotCarried::OwnEdit));
    }
    Ok(CarryCheck::Carried {
        moved,
        renamed: was_renamed,
    })
}

/// A proposal [`carries`] found carried to its change's visible commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Carried {
    /// The change.
    pub change: ChangeIdHex,
    /// Its visible commit, which the proposal follows to.
    pub visible: CommitIdHex,
    /// The parent changes whose commit moved, in parent order.
    pub moved: Vec<ChangeIdHex>,
    /// It moved onto a superseded parent's successor.
    pub renamed: bool,
}

/// What [`carries`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Carries {
    /// The proposals carried.
    pub carried: Vec<Carried>,
    /// The proposals a rewrite below them left conflicted, each with the
    /// commit it is now and the parent changes that moved.
    pub conflicted: Vec<Carried>,
    /// It stopped at its limit.
    pub more: bool,
    /// The verdicts it reached that were not known, each with its change.
    pub verdicts: Vec<(ChangeIdHex, (CommitIdHex, CommitIdHex), NotCarried)>,
    /// Every pair of a recorded and another visible commit it looked at.
    pub moved: Vec<(CommitIdHex, CommitIdHex)>,
}

/// Of `proposals` (each change with its recorded commit), those whose
/// change has one visible commit, another than the recorded one, to which
/// [`carry_check`] carries the proposal (`renamed`: superseded parent
/// changes and their successors), and those a rewrite below them left
/// conflicted. A pair `known` holds costs no check. Checks no more than
/// `max` proposals that moved (S-247): a check that cannot carry is made
/// once, and known after, so the ones that can are never starved.
pub async fn carries(
    repo: &std::sync::Arc<ReadonlyRepo>,
    proposals: &[(ChangeIdHex, CommitIdHex)],
    renamed: &[(ChangeIdHex, ChangeIdHex)],
    known: &Verdicts,
    max: usize,
) -> Result<Carries, BackendError> {
    let mut found = Carries::default();
    let mut checked = 0;
    for (change, recorded) in proposals {
        let Ok(Some(targets)) = repo.resolve_change_id(&change.to_id()).await else {
            continue;
        };
        let visible: Vec<_> = targets
            .visible_with_offsets()
            .map(|(_, id)| id.clone())
            .collect();
        let [visible] = visible.as_slice() else {
            continue;
        };
        let visible = CommitIdHex::of(visible);
        if visible == *recorded {
            continue;
        }
        let pair = (recorded.clone(), visible.clone());
        found.moved.push(pair.clone());
        let verdict = match known.get(&pair) {
            Some(not) => CarryCheck::Not(not.clone()),
            None if checked == max => {
                found.more = true;
                continue;
            }
            None => {
                checked += 1;
                let verdict = carry_check(repo, recorded, &visible, renamed).await?;
                if let CarryCheck::Not(not) = &verdict {
                    found.verdicts.push((change.clone(), pair, not.clone()));
                }
                verdict
            }
        };
        match verdict {
            CarryCheck::Carried { moved, renamed } => found.carried.push(Carried {
                change: change.clone(),
                visible,
                moved,
                renamed,
            }),
            CarryCheck::Not(NotCarried::Conflicted { moved }) => found.conflicted.push(Carried {
                change: change.clone(),
                visible,
                moved,
                renamed: false,
            }),
            CarryCheck::Not(_) => {}
        }
    }
    Ok(found)
}

/// What [`above_any`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Above {
    /// The heads with a still-left change below them.
    pub heads: Vec<ChangeIdHex>,
    /// The changes of `left` still out of review: at the commit they left
    /// review at, or at that commit rebased with their own diff unchanged
    /// (a rewrite below them), each with its visible commit.
    pub left: Vec<(ChangeIdHex, CommitIdHex)>,
    /// The heads with more than one visible commit (divergent), or whose
    /// commits could not be resolved, and every head when a change of
    /// `left` could not be resolved: whether they are above one is not
    /// known.
    pub unread: Vec<ChangeIdHex>,
    /// The verdicts it reached that were not known, each with its change.
    pub verdicts: Vec<(ChangeIdHex, (CommitIdHex, CommitIdHex), NotCarried)>,
}

/// Of `heads` (changes), those whose visible commit has a proper ancestor
/// in `left` (changes with the commit they were declined or withdrawn at)
/// that is still out of review: its visible commit is that commit, or
/// [`carry_check`] carries it there (`renamed`, `known`: as for
/// [`carries`]), since a rebase by a rewrite below it is not a revision by
/// its owner (R3.11). The stack reads that follow need only these heads.
pub async fn above_any(
    repo: &std::sync::Arc<ReadonlyRepo>,
    heads: &[ChangeIdHex],
    left: &[(ChangeIdHex, CommitIdHex)],
    (renamed, known): (&[(ChangeIdHex, ChangeIdHex)], &Verdicts),
) -> Result<Above, BackendError> {
    let all_visible = async |change: &ChangeIdHex| -> Result<Vec<jj_lib::backend::CommitId>, ()> {
        let Ok(targets) = repo.resolve_change_id(&change.to_id()).await else {
            return Err(());
        };
        Ok(targets
            .into_iter()
            .flat_map(|targets| {
                targets
                    .visible_with_offsets()
                    .map(|(_, id)| id.clone())
                    .collect::<Vec<_>>()
            })
            .collect())
    };
    let visible = async |change: &ChangeIdHex| -> Result<Option<jj_lib::backend::CommitId>, ()> {
        match all_visible(change).await?.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(one.clone())),
            _ => Err(()),
        }
    };
    let mut found = Above::default();
    let mut unreadable = false;
    for (change, commit) in left {
        // A divergent change is still out at any of its commits that is
        // the one it left at, or that commit rebased below.
        let Ok(commits) = all_visible(change).await else {
            unreadable = true;
            continue;
        };
        for now in commits {
            let now_hex = CommitIdHex::of(&now);
            let pair = (commit.clone(), now_hex.clone());
            let still = now == commit.to_id()
                || match known.get(&pair) {
                    Some(_) => false,
                    None => match carry_check(repo, commit, &now_hex, renamed).await? {
                        CarryCheck::Carried { .. } => true,
                        CarryCheck::Not(not) => {
                            found.verdicts.push((change.clone(), pair, not));
                            false
                        }
                    },
                };
            if still {
                found.left.push((change.clone(), now_hex));
            }
        }
    }
    // What is above a change that could not be read is not known.
    if unreadable {
        found.unread.extend(heads.iter().cloned());
        return Ok(found);
    }
    if found.left.is_empty() {
        return Ok(found);
    }
    for head in heads {
        let id = match visible(head).await {
            Ok(Some(id)) => id,
            Ok(None) => continue,
            Err(()) => {
                found.unread.push(head.clone());
                continue;
            }
        };
        for (_, below) in &found.left {
            let below = below.to_id();
            if below != id
                && repo
                    .index()
                    .is_ancestor(&below, &id)
                    .await
                    .map_err(|error| BackendError::Other(error.into()))?
            {
                found.heads.push(head.clone());
                break;
            }
        }
    }
    Ok(found)
}
