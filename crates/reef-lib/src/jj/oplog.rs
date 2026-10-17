//! Reading the operation log, and recognizing the snapshot operations Reef
//! writes (R1.16).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt as _;
use jj_lib::backend::{BackendError, Timestamp};
use jj_lib::op_store::{OpStoreError, OperationId};
use jj_lib::op_walk::walk_ancestors;
use jj_lib::operation::Operation;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::Repo as _;

use super::repo::{ChangeIdHex, CommitIdHex, OperationIdHex, Repo};
use crate::error::{Classify, ErrorKind, classify_by_source};

/// One operation in the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpEntry {
    /// The operation's id.
    pub id: OperationIdHex,
    /// Parent operations; more than one for a merge of divergent heads.
    pub parents: Vec<OperationIdHex>,
    /// The operation's description, such as `snapshot working copy`.
    pub description: String,
    /// When the operation started.
    pub start: SystemTime,
    /// When the operation ended.
    pub end: SystemTime,
    /// Host recorded by the writer: the machine's hostname for operations
    /// the daemon writes, empty for settings built without a
    /// [`SettingsSpec`](super::settings::SettingsSpec).
    pub hostname: String,
    /// User recorded by the writer: the OS user for operations the daemon
    /// writes, empty for settings built without a
    /// [`SettingsSpec`](super::settings::SettingsSpec).
    pub username: String,
    /// Whether the operation only snapshotted the working copy.
    pub is_snapshot: bool,
    /// The workspace the operation ran in, if recorded.
    pub workspace: Option<String>,
}

/// What [`Repo::walk_op_log`] walked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpWalk {
    /// Operations visited.
    pub walked: u64,
    /// The limit stopped the walk before the root operation.
    pub truncated: bool,
}

/// A snapshot operation Reef wrote, recognized by its description (the
/// operation itself has no field to mark it with; ADR 0008).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotMark {
    /// The Reef workspace id, in hex: from the description, else from the
    /// jj workspace name `reef-<id>`.
    pub workspace_id: String,
    /// Which description it carries.
    pub kind: SnapshotMarkKind,
}

/// The description forms of [`is_reef_snapshot`]. Watcher and explicit
/// snapshots share one form, as do final and shutdown ones, so the daemon
/// keeps the finer kind in its index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotMarkKind {
    /// `reef: snapshot workspace <id>`, or any other description on a
    /// snapshot of a `reef-<id>` jj workspace.
    Snapshot,
    /// `reef: final snapshot of workspace <id> (<reason>)`.
    Final,
    /// `reef: move workspace <id> to <change>`.
    Move,
}

/// Prefix of watcher and explicit snapshot descriptions.
const SNAPSHOT_PREFIX: &str = "reef: snapshot workspace ";
/// Prefix of final (and shutdown) snapshot descriptions.
const FINAL_PREFIX: &str = "reef: final snapshot of workspace ";
/// Prefix of move snapshot descriptions.
const MOVE_PREFIX: &str = "reef: move workspace ";
/// Prefix of the jj workspace names of Reef workspaces.
const WORKSPACE_PREFIX: &str = "reef-";

/// Whether `entry` is a snapshot operation of a Reef workspace, and of
/// which one: a snapshot operation (`is_snapshot`) with one of the daemon's
/// descriptions (R1.13), or with any other description recorded in a
/// `reef-<id>` jj workspace. The second covers a snapshot given a custom
/// description through the `Snapshot` RPC; it also covers `jj` run inside a
/// Reef workspace, whose snapshot holds that workspace's local edits as
/// much as one the daemon took. `None` for anything else, such as jj's
/// `snapshot working copy` in the user's `default` workspace.
pub fn is_reef_snapshot(entry: &OpEntry) -> Option<SnapshotMark> {
    if !entry.is_snapshot {
        return None;
    }
    described(&entry.description).or_else(|| {
        let id = entry.workspace.as_deref()?.strip_prefix(WORKSPACE_PREFIX)?;
        is_hex(id).then(|| SnapshotMark {
            workspace_id: id.to_owned(),
            kind: SnapshotMarkKind::Snapshot,
        })
    })
}

fn is_hex(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The mark one of the daemon's snapshot descriptions carries.
fn described(description: &str) -> Option<SnapshotMark> {
    let (workspace_id, kind) = if let Some(id) = description.strip_prefix(SNAPSHOT_PREFIX) {
        (id, SnapshotMarkKind::Snapshot)
    } else if let Some(rest) = description.strip_prefix(FINAL_PREFIX) {
        let (id, reason) = rest.split_once(' ')?;
        let reason = reason.strip_prefix('(')?.strip_suffix(')')?;
        if reason.is_empty() {
            return None;
        }
        (id, SnapshotMarkKind::Final)
    } else {
        let (id, change) = description.strip_prefix(MOVE_PREFIX)?.split_once(" to ")?;
        if change.is_empty() {
            return None;
        }
        (id, SnapshotMarkKind::Move)
    };
    is_hex(workspace_id).then(|| SnapshotMark {
        workspace_id: workspace_id.to_owned(),
        kind,
    })
}

/// Failure to read the operation log.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpLogError {
    /// An operation could not be read from the op store.
    #[error("failed to read the operation log")]
    OpStore(#[from] OpStoreError),
    /// An operation's view named a commit that could not be read.
    #[error("failed to read a commit named by the operation log")]
    Commit(#[from] BackendError),
    /// An operation id is not one of this store's.
    #[error("{0} is not an operation id of this repository")]
    InvalidOperationId(OperationIdHex),
}

impl Classify for OpLogError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::OpStore(error) => classify_by_source(error),
            Self::Commit(error) => classify_by_source(error),
            Self::InvalidOperationId(_) => ErrorKind::InvalidArgument,
        }
    }
}

impl Repo {
    /// Up to `limit` operations from the loaded head back through its
    /// ancestors, newest first by end time.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn op_log(&self, limit: usize) -> Result<Vec<OpEntry>, OpLogError> {
        let mut entries = Vec::new();
        self.walk_op_log_unlogged(limit, |entry| entries.push(entry))
            .await
            .inspect_err(|error| crate::log_failure!(error))?;
        Ok(entries)
    }

    /// Calls `visit` with each of up to `limit` operations from the loaded
    /// head back through its ancestors, newest first by end time, and says
    /// whether the limit cut the walk short. Streams, so a long log is never
    /// held in memory.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn walk_op_log(
        &self,
        limit: usize,
        visit: impl FnMut(OpEntry),
    ) -> Result<OpWalk, OpLogError> {
        self.walk_op_log_unlogged(limit, visit)
            .await
            .inspect_err(|error| crate::log_failure!(error))
    }

    async fn walk_op_log_unlogged(
        &self,
        limit: usize,
        mut visit: impl FnMut(OpEntry),
    ) -> Result<OpWalk, OpLogError> {
        // One past the limit, to tell a log of exactly `limit` operations
        // from a longer one.
        let mut ops = std::pin::pin!(
            walk_ancestors(std::slice::from_ref(self.repo.operation()))
                .take(limit.saturating_add(1))
        );
        let mut walked = 0;
        while let Some(op) = ops.next().await {
            let op = op?;
            if walked == limit {
                return Ok(OpWalk {
                    walked: u64::try_from(walked).unwrap_or(u64::MAX),
                    truncated: true,
                });
            }
            walked += 1;
            visit(entry_of(&op));
        }
        Ok(OpWalk {
            walked: u64::try_from(walked).unwrap_or(u64::MAX),
            truncated: false,
        })
    }

    /// The working-copy commit of the jj workspace `workspace` in operation
    /// `op`'s view, and its change; `None` if the view has none for it.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), op_id = %op))]
    pub async fn wc_commit_at(
        &self,
        op: &OperationIdHex,
        workspace: &str,
    ) -> Result<Option<(CommitIdHex, ChangeIdHex)>, OpLogError> {
        self.wc_commit_at_unlogged(op, workspace)
            .await
            .inspect_err(|error| crate::log_failure!(error))
    }

    async fn wc_commit_at_unlogged(
        &self,
        op: &OperationIdHex,
        workspace: &str,
    ) -> Result<Option<(CommitIdHex, ChangeIdHex)>, OpLogError> {
        let id = OperationId::try_from_hex(op.as_str())
            .ok_or_else(|| OpLogError::InvalidOperationId(op.clone()))?;
        let operation = self.repo.loader().load_operation(&id).await?;
        let view = operation.view().await?;
        let Some(commit_id) = view.get_wc_commit_id(WorkspaceName::new(workspace)) else {
            return Ok(None);
        };
        let commit = self.repo.store().get_commit_async(commit_id).await?;
        Ok(Some((
            CommitIdHex::of(commit.id()),
            ChangeIdHex::of(commit.change_id()),
        )))
    }
}

fn entry_of(op: &Operation) -> OpEntry {
    let metadata = op.metadata();
    OpEntry {
        id: OperationIdHex::of(op.id()),
        parents: op.parent_ids().iter().map(OperationIdHex::of).collect(),
        description: metadata.description.clone(),
        start: system_time(metadata.time.start),
        end: system_time(metadata.time.end),
        hostname: metadata.hostname.clone(),
        username: metadata.username.clone(),
        is_snapshot: metadata.is_snapshot,
        workspace: metadata
            .workspace_name
            .as_ref()
            .map(|name| name.as_str().to_owned()),
    }
}

/// A jj timestamp as a [`SystemTime`].
pub(crate) fn system_time(timestamp: Timestamp) -> SystemTime {
    let millis = timestamp.timestamp.0;
    let offset = Duration::from_millis(millis.unsigned_abs());
    if millis >= 0 {
        UNIX_EPOCH + offset
    } else {
        UNIX_EPOCH - offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef";

    fn entry(description: &str, is_snapshot: bool) -> OpEntry {
        in_workspace(description, is_snapshot, "default")
    }

    fn in_workspace(description: &str, is_snapshot: bool, workspace: &str) -> OpEntry {
        OpEntry {
            id: "00".parse().unwrap(),
            parents: Vec::new(),
            description: description.to_owned(),
            start: UNIX_EPOCH,
            end: UNIX_EPOCH,
            hostname: String::new(),
            username: String::new(),
            is_snapshot,
            workspace: Some(workspace.to_owned()),
        }
    }

    fn mark(description: &str) -> Option<SnapshotMarkKind> {
        let mark = is_reef_snapshot(&entry(description, true))?;
        assert_eq!(mark.workspace_id, ID, "{description}");
        Some(mark.kind)
    }

    #[test]
    fn snapshot_descriptions_are_recognized() {
        assert_eq!(
            mark(&format!("reef: snapshot workspace {ID}")),
            Some(SnapshotMarkKind::Snapshot)
        );
        for reason in ["released", "lease expired", "shutdown"] {
            assert_eq!(
                mark(&format!(
                    "reef: final snapshot of workspace {ID} ({reason})"
                )),
                Some(SnapshotMarkKind::Final),
                "{reason}"
            );
        }
        assert_eq!(
            mark(&format!("reef: move workspace {ID} to 0a1b2c3d")),
            Some(SnapshotMarkKind::Move)
        );
    }

    #[test]
    fn other_operations_are_not_reef_snapshots() {
        for description in [
            "snapshot working copy",
            "reef: create workspace 0123456789abcdef",
            "reef: release workspace 0123456789abcdef",
            "reef: snapshot workspace ",
            "reef: snapshot workspace 0123456789abcdef extra",
            "reef: final snapshot of workspace 0123456789abcdef",
            "reef: final snapshot of workspace 0123456789abcdef ()",
            "reef: move workspace 0123456789abcdef to ",
            "reef: move workspace 0123456789abcdef",
            "custom description",
        ] {
            assert_eq!(
                is_reef_snapshot(&entry(description, true)),
                None,
                "{description:?}"
            );
        }
        // The right description on an operation that is not a snapshot.
        let description = format!("reef: snapshot workspace {ID}");
        assert_eq!(is_reef_snapshot(&entry(&description, false)), None);
        // Not a Reef workspace's name.
        for workspace in ["reef-", "reef-xyz", "agent-0123456789abcdef"] {
            let op = in_workspace("custom description", true, workspace);
            assert_eq!(is_reef_snapshot(&op), None, "{workspace}");
        }
    }

    // A custom description given through the `Snapshot` RPC, and `jj` run
    // inside a Reef workspace: the workspace name marks them.
    #[test]
    fn reef_workspace_snapshots_are_recognized_by_name() {
        let name = format!("reef-{ID}");
        for description in ["wip before refactor", "snapshot working copy"] {
            let mark = is_reef_snapshot(&in_workspace(description, true, &name)).unwrap();
            assert_eq!(mark.workspace_id, ID);
            assert_eq!(mark.kind, SnapshotMarkKind::Snapshot);
        }
        assert_eq!(
            is_reef_snapshot(&in_workspace("wip", false, &name)),
            None,
            "not a snapshot"
        );
        // The description's own form still sets the kind.
        let fin = format!("reef: final snapshot of workspace {ID} (released)");
        let mark = is_reef_snapshot(&in_workspace(&fin, true, &name)).unwrap();
        assert_eq!(mark.kind, SnapshotMarkKind::Final);
    }
}
