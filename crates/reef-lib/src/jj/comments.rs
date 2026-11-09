//! Comments and instructions as the operation log records them (R3.12,
//! ADR 0069).
//!
//! Every comment write (a note or an instruction written, and each move of
//! an instruction) is an operation with no view change whose
//! [`ATTR_COMMENT`] attribute lists its events as compact JSON
//! ([`CommentRecord`], format 1): ids and states, never a comment's text,
//! which only the daemon's rows hold. This module writes and reads those
//! operations, and checks a comment's anchor against a commit's files.

use std::collections::{BTreeMap, BTreeSet};

use futures::StreamExt as _;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use serde::{Deserialize, Serialize};

use super::files::{self, FileError};
use super::lifecycle::{FoundTransition, LifecycleError};
use super::oplog::{ATTR_COMMENT, OpEntry, OpMeta};
use super::repo::{CommitIdHex, OperationIdHex, Repo};

/// The format [`CommentRecord::v`] holds.
pub const RECORD_FORMAT: u32 = 1;

/// One comment event as an operation's [`ATTR_COMMENT`] holds it. Empty
/// strings stand for absent values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentEventRecord {
    /// The event's id, `cev_<16 hex>`, which the daemon's row has too.
    pub id: String,
    /// The comment, `cmt_<16 hex>`.
    pub comment: String,
    /// Its change, in hex.
    pub change: String,
    /// The change's proposal round when the comment was written.
    pub round: u32,
    /// `note` or `instruction`.
    pub kind: String,
    /// What happened: `comment`, `instruct`, `confirm`, `claim`, `start`,
    /// `finish`, `cancel` or `release`.
    pub event: String,
    /// For an instruction, its state before.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from: String,
    /// For an instruction, its state after.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub to: String,
    /// Why: `request`, `reproposed`, `change_final` or `expired`.
    pub cause: String,
}

/// An operation's [`ATTR_COMMENT`] value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentRecord {
    /// [`RECORD_FORMAT`].
    pub v: u32,
    /// The events, in the order they apply.
    pub comments: Vec<CommentEventRecord>,
}

impl CommentRecord {
    /// `events` as the attribute's value.
    pub fn encode(events: &[CommentEventRecord]) -> String {
        let record = CommentRecord {
            v: RECORD_FORMAT,
            comments: events.to_vec(),
        };
        serde_json::to_string(&record).unwrap_or_default()
    }
}

/// The comment events an operation's attributes record; empty when there
/// are none, or when the attribute is not format 1, which is logged.
pub fn comment_events_of(attributes: &BTreeMap<String, String>) -> Vec<CommentEventRecord> {
    let Some(value) = attributes.get(ATTR_COMMENT) else {
        return Vec::new();
    };
    match serde_json::from_str::<CommentRecord>(value) {
        Ok(record) if record.v == RECORD_FORMAT => record.comments,
        Ok(record) => {
            tracing::debug!(
                "a comment record of format {}, not {RECORD_FORMAT}",
                record.v
            );
            Vec::new()
        }
        Err(error) => {
            tracing::debug!(error = %error, "an unreadable comment record");
            Vec::new()
        }
    }
}

/// The description of a comment operation: `reef: <event> comment <id> on
/// change <change>`, which [`super::oplog::classify`] reads back.
pub fn comment_description(event: &str, comment: &str, change: &str) -> String {
    format!("reef: {event} comment {comment} on change {change}")
}

impl Repo {
    /// Records `meta.comment_events` in one operation that changes nothing
    /// in the view, which this [`Repo`] adopts.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn record_comment_events(
        &mut self,
        meta: &OpMeta,
    ) -> Result<OperationIdHex, LifecycleError> {
        if meta.comment_events.is_empty() {
            return Err(LifecycleError::Empty);
        }
        let tx = self.repo.start_transaction();
        self.repo = meta.commit(tx).await?;
        Ok(OperationIdHex::of(self.repo.op_id()))
    }
}

/// The operations, among `repo`'s head and its ancestors that ended at or
/// after `not_before`, whose [`ATTR_COMMENT`] records an event with one of
/// `ids`, by event id; the walk stops at the first older operation.
pub async fn find_comment_events(
    repo: &ReadonlyRepo,
    ids: &BTreeSet<String>,
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
        for event in comment_events_of(&entry.attributes) {
            if ids.contains(&event.id) {
                found.insert(
                    event.id,
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

/// The operation `text` names when it records comment events: the
/// operations an undo refuses. `None` otherwise, including when `text`
/// resolves to nothing.
pub async fn comment_operation(repo: &ReadonlyRepo, text: &str) -> Option<OpEntry> {
    let op = super::oplog::resolve_operation(repo, text).await.ok()?;
    op.metadata()
        .attributes
        .contains_key(ATTR_COMMENT)
        .then(|| super::oplog::entry(&op))
}

/// What an anchor's file is, for its line check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchorFile {
    /// Its lines, counting a last line with no newline; `None` when only
    /// the first [`files::MAX_READ_BYTES`] were read.
    pub lines: Option<u64>,
    /// Whether its first 8 KiB hold a NUL byte, or it is not a file.
    pub binary: bool,
}

/// The file at `path` in `commit`, or in its first parent with `old`, for
/// an anchor's check; `None` when the path is absent, a directory, or
/// `commit` has no parent for `old`.
pub async fn anchor_file(
    repo: &ReadonlyRepo,
    commit: &CommitIdHex,
    old: bool,
    path: &str,
) -> Result<Option<AnchorFile>, FileError> {
    let Ok(repo_path) = files::check_path(path, false) else {
        return Ok(None);
    };
    let target = if old {
        let commit = repo
            .store()
            .get_commit_async(&commit.to_id())
            .await
            .map_err(FileError::from)?;
        match commit.parent_ids().first() {
            Some(parent) if parent != repo.store().root_commit_id() => CommitIdHex::of(parent),
            _ => return Ok(None),
        }
    } else {
        commit.clone()
    };
    let read = match files::read_file(repo, &target, &repo_path, 0, 0).await {
        Ok(read) => read,
        Err(FileError::NotFound { .. } | FileError::NotAFile { .. }) => return Ok(None),
        Err(error) => return Err(error),
    };
    let content = &read.content;
    let binary = !matches!(
        read.entry.kind,
        super::diff::EntryKind::File | super::diff::EntryKind::Executable
    ) || content.iter().take(8192).any(|byte| *byte == 0);
    let whole = u64::try_from(content.len()).unwrap_or(u64::MAX) == read.size;
    let lines = whole.then(|| {
        let newlines = u64::try_from(memchr_count(content)).unwrap_or(u64::MAX);
        if content.last().is_some_and(|byte| *byte != b'\n') {
            newlines + 1
        } else {
            newlines
        }
    });
    Ok(Some(AnchorFile { lines, binary }))
}

/// How many newlines `bytes` holds.
fn memchr_count(bytes: &[u8]) -> usize {
    bytes.split(|byte| *byte == b'\n').count().saturating_sub(1)
}

/// The id of the file at `path` in `commit`, in hex; `None` when nothing
/// is there or it is a directory. Comments compare it to mark themselves
/// outdated.
pub async fn file_id_at(
    repo: &ReadonlyRepo,
    commit: &CommitIdHex,
    path: &str,
) -> Result<Option<String>, FileError> {
    let tree = repo
        .store()
        .get_commit_async(&commit.to_id())
        .await
        .map_err(FileError::from)?
        .tree();
    Ok(files::tree_file(&tree, path).await?.map(|file| {
        use std::fmt::Write as _;
        file.file_id.iter().fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
    }))
}

/// The tree of `commit`, in hex; `None` when it cannot be read or is
/// conflicted.
pub async fn tree_of(repo: &ReadonlyRepo, commit: &CommitIdHex) -> Option<String> {
    use jj_lib::object_id::ObjectId;
    let commit = repo.store().get_commit_async(&commit.to_id()).await.ok()?;
    commit.tree_ids().as_resolved().map(ObjectId::hex)
}

/// For each `(change, anchor commit, path)`, whether the change's visible
/// commit (its newest, when divergent) has a different entry at `path` than
/// the anchor commit; `false` when the change has no visible commit.
pub async fn outdated(
    repo: &ReadonlyRepo,
    items: &[(super::repo::ChangeIdHex, CommitIdHex, String)],
) -> Vec<bool> {
    let mut out = Vec::with_capacity(items.len());
    for (change, anchor, path) in items {
        let visible = match repo.resolve_change_id(&change.to_id()).await {
            Ok(Some(targets)) => targets
                .visible_with_offsets()
                .next()
                .map(|(_, id)| CommitIdHex::of(id)),
            _ => None,
        };
        let stale = match visible {
            Some(visible) if &visible != anchor => {
                let before = file_id_at(repo, anchor, path).await.ok().flatten();
                let now = file_id_at(repo, &visible, path).await.ok().flatten();
                before != now
            }
            _ => false,
        };
        out.push(stale);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> CommentEventRecord {
        CommentEventRecord {
            id: "cev_0123456789abcdef".to_owned(),
            comment: "cmt_0123456789abcdef".to_owned(),
            change: "ab".repeat(16),
            round: 1,
            kind: "instruction".to_owned(),
            event: "confirm".to_owned(),
            from: "awaiting_confirmation".to_owned(),
            to: "pending".to_owned(),
            cause: "request".to_owned(),
        }
    }

    #[test]
    fn record_round_trips_and_skips_empty_fields() {
        let mut note = event();
        note.kind = "note".to_owned();
        note.event = "comment".to_owned();
        note.from.clear();
        note.to.clear();
        let text = CommentRecord::encode(&[event(), note.clone()]);
        assert_eq!(text.matches("\"from\"").count(), 1, "{text}");
        let attributes = BTreeMap::from([(ATTR_COMMENT.to_owned(), text)]);
        assert_eq!(comment_events_of(&attributes), vec![event(), note]);
    }

    #[test]
    fn other_formats_and_garbage_read_as_none() {
        for value in [r#"{"v":2,"comments":[]}"#, "{", "[]"] {
            let attributes = BTreeMap::from([(ATTR_COMMENT.to_owned(), value.to_owned())]);
            assert_eq!(comment_events_of(&attributes), Vec::new());
        }
        assert_eq!(comment_events_of(&BTreeMap::new()), Vec::new());
    }

    #[test]
    fn descriptions_classify_as_comments() {
        let entry = OpEntry {
            id: OperationIdHex::of(&jj_lib::op_store::OperationId::new(vec![1; 64])),
            parents: vec![OperationIdHex::of(&jj_lib::op_store::OperationId::new(
                vec![0; 64],
            ))],
            description: comment_description("confirm", "cmt_0123456789abcdef", "abcd"),
            start: std::time::UNIX_EPOCH,
            end: std::time::UNIX_EPOCH,
            hostname: String::new(),
            username: String::new(),
            is_snapshot: false,
            workspace: None,
            attributes: BTreeMap::new(),
        };
        assert_eq!(
            super::super::oplog::classify(&entry).kind,
            Some(super::super::oplog::OpKind::Comment)
        );
    }
}
