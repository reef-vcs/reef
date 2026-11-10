//! Resolver assignments as the operation log announces them (R3.27, ADR
//! 0068).
//!
//! The daemon's rows in `reef.sqlite3` hold every assignment and its
//! events; an operation that changes nothing in the view announces each
//! change of an assignment in its [`ATTR_CONFLICT`] attribute, as compact
//! JSON ([`AssignmentRecord`], format 1), so subscribers, the activity feed
//! and `reef op log` see it. The operation is a notice: nothing reads it
//! back to decide an assignment's state.

use std::collections::{BTreeMap, HashSet};

use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::transaction::TransactionCommitError;
use serde::{Deserialize, Serialize};

use super::conflict::{self, ConflictError};
use super::oplog::{ATTR_CONFLICT, OpMeta};
use super::repo::{ChangeIdHex, CommitIdHex, OperationIdHex, Repo};
use super::revset::ParsedRevset;
use crate::error::{Classify, ErrorKind, classify_by_source};

/// The format [`AssignmentRecord::v`] holds.
pub const RECORD_FORMAT: u32 = 1;

/// The most events one operation carries; a larger batch is several
/// operations.
pub const MAX_NOTICES: usize = 64;

/// One change of an assignment as an operation's [`ATTR_CONFLICT`] holds
/// it. Empty strings stand for absent values; states and reasons are the
/// daemon's lowercase names (`assigned`, `owner_ended`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentNotice {
    /// The event's id, `cae_<16 hex>`, which the daemon's row has too.
    pub id: String,
    /// The event's sequence number in its repository.
    pub seq: u64,
    /// The assignment, `asg_<16 hex>`.
    pub assignment: String,
    /// The change, in hex.
    pub change: String,
    /// The conflict's id when the event was recorded.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub conflict: String,
    /// The state before; empty when the assignment opened.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from: String,
    /// The state after.
    pub to: String,
    /// Why.
    pub reason: String,
    /// The owner's workspace afterwards, if a session owns it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace: String,
    /// The owner's agent afterwards.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    /// The owner's workspace before, when the event moved it away: so the
    /// previous owner's subscription sees it too.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from_workspace: String,
    /// The priority afterwards.
    #[serde(default)]
    pub priority: i32,
}

/// An operation's [`ATTR_CONFLICT`] value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentRecord {
    /// [`RECORD_FORMAT`].
    pub v: u32,
    /// The events, in the order they were recorded.
    pub events: Vec<AssignmentNotice>,
}

impl AssignmentRecord {
    /// `events` as the attribute's value.
    pub fn encode(events: &[AssignmentNotice]) -> String {
        let record = AssignmentRecord {
            v: RECORD_FORMAT,
            events: events.to_vec(),
        };
        serde_json::to_string(&record).unwrap_or_default()
    }
}

/// The events an operation's attributes announce; empty when there are
/// none, or when the attribute is not format 1 (another tool wrote it, or
/// a later Reef).
pub fn notices_of(attributes: &BTreeMap<String, String>) -> Vec<AssignmentNotice> {
    let Some(value) = attributes.get(ATTR_CONFLICT) else {
        return Vec::new();
    };
    match serde_json::from_str::<AssignmentRecord>(value) {
        Ok(record) if record.v == RECORD_FORMAT => record.events,
        Ok(record) => {
            tracing::debug!(
                "an assignment record of format {}, not {RECORD_FORMAT}",
                record.v
            );
            Vec::new()
        }
        Err(error) => {
            tracing::debug!(error = %error, "an unreadable assignment record");
            Vec::new()
        }
    }
}

/// Failure to announce assignment events.
#[derive(Debug, thiserror::Error)]
pub enum AssignmentRecordError {
    /// There was nothing to announce.
    #[error("no assignment events to record")]
    Empty,
    /// The operation could not be written.
    #[error("record the assignment events")]
    Commit(#[from] TransactionCommitError),
}

impl Classify for AssignmentRecordError {
    fn kind(&self) -> ErrorKind {
        match self {
            AssignmentRecordError::Empty => ErrorKind::InvalidArgument,
            AssignmentRecordError::Commit(source) => classify_by_source(source),
        }
    }
}

impl Repo {
    /// Records `meta`, which must carry [`OpMeta::conflict_events`], in one
    /// operation that changes nothing in the view, which this [`Repo`]
    /// adopts.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn record_assignment_events(
        &mut self,
        meta: &OpMeta,
    ) -> Result<OperationIdHex, AssignmentRecordError> {
        if meta.conflict_events.is_empty() {
            return Err(AssignmentRecordError::Empty);
        }
        let tx = self.repo.start_transaction();
        self.repo = meta.commit(tx).await?;
        Ok(OperationIdHex::of(self.repo.op_id()))
    }
}

/// How many conflicted changes [`conflicted_changes`] lists in all, per
/// one it may list that introduces a conflict.
pub const INHERITED_FACTOR: usize = 8;

/// One conflicted commit the follower found: its change, and the
/// conflict's id and counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflicted {
    /// The change.
    pub change: ChangeIdHex,
    /// The conflicted commit.
    pub commit: CommitIdHex,
    /// R3.25's conflict id.
    pub conflict_id: String,
    /// Conflicted files no parent carries.
    pub introduced: u32,
    /// Conflicted files in all.
    pub files_total: u32,
}

/// The conflicted commits of `parsed` in log order, one per change (the
/// first in log order), with whether there were more: at most `cap` that
/// introduce a conflict, and [`INHERITED_FACTOR`] times as many in all, so a
/// tall stack of children that only inherit a conflict, which log order
/// puts before the commit that introduces it, cannot use up the cap. Reads
/// each conflict without its files' detail (R3.25's `read` with no files),
/// so inherited conflicts are listed too, with `introduced` 0.
pub async fn conflicted_changes(
    repo: &ReadonlyRepo,
    parsed: &ParsedRevset,
    cap: usize,
    should_stop: &dyn Fn() -> bool,
) -> Result<(Vec<Conflicted>, bool), ConflictError> {
    let mut found: Vec<Conflicted> = Vec::new();
    let mut seen: HashSet<ChangeIdHex> = HashSet::new();
    let mut introducing = 0usize;
    let total = cap.saturating_mul(INHERITED_FACTOR);
    let mut skip = 0usize;
    loop {
        let page = conflict::list(repo, parsed, skip, cap, 0, false, should_stop).await?;
        for listed in page.conflicts {
            let commit = repo
                .store()
                .get_commit_async(&listed.commit_id.to_id())
                .await?;
            let change = ChangeIdHex::of(commit.change_id());
            if !seen.insert(change.clone()) {
                continue;
            }
            let introduces = listed.object.introduced > 0;
            if found.len() == total || (introduces && introducing == cap) {
                return Ok((found, true));
            }
            introducing += usize::from(introduces);
            found.push(Conflicted {
                change,
                commit: listed.commit_id,
                conflict_id: listed.object.id,
                introduced: listed.object.introduced,
                files_total: listed.object.files_total,
            });
        }
        match page.resume {
            Some((position, _)) if page.more && !should_stop() => skip = position,
            _ => return Ok((found, page.more)),
        }
    }
}

/// A change's newest visible commit, and whether its tree is conflicted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Visible {
    /// The commit.
    pub commit: CommitIdHex,
    /// Its tree has a conflict.
    pub conflicted: bool,
}

/// `change`'s first visible commit in `repo`'s view, if it has one.
pub async fn visible_commit(
    repo: &ReadonlyRepo,
    change: &ChangeIdHex,
) -> Result<Option<Visible>, ConflictError> {
    let targets = repo
        .resolve_change_id(&change.to_id())
        .await
        .map_err(|error| ConflictError::Index(error.into()))?;
    let Some(first) = targets
        .and_then(jj_lib::index::ResolvedChangeTargets::into_visible)
        .and_then(|visible| visible.into_iter().next())
    else {
        return Ok(None);
    };
    let commit = repo.store().get_commit_async(&first).await?;
    Ok(Some(Visible {
        commit: CommitIdHex::of(commit.id()),
        conflicted: commit.has_conflict(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_round_trip() {
        let notice = AssignmentNotice {
            id: "cae_0123456789abcdef".into(),
            seq: 3,
            assignment: "asg_0123456789abcdef".into(),
            change: "aa".into(),
            conflict: "cfl_x".into(),
            from: String::new(),
            to: "assigned".into(),
            reason: "default".into(),
            workspace: "ws".into(),
            agent: "bot".into(),
            from_workspace: "old".into(),
            priority: 2,
        };
        let mut attributes = BTreeMap::new();
        attributes.insert(
            ATTR_CONFLICT.to_owned(),
            AssignmentRecord::encode(std::slice::from_ref(&notice)),
        );
        assert_eq!(notices_of(&attributes), [notice]);
        attributes.insert(ATTR_CONFLICT.to_owned(), r#"{"v":2,"events":[]}"#.into());
        assert_eq!(notices_of(&attributes), []);
        attributes.insert(ATTR_CONFLICT.to_owned(), "nonsense".into());
        assert_eq!(notices_of(&attributes), []);
    }
}
