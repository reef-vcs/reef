//! Workspaces as of an operation (R2.12, ADR 0046): the working copies in
//! the operation's view, joined to their rows, the lease each held then,
//! and the snapshot index up to the operation's end. What the operation log
//! does not record stays unset: attention, mounted stores, the watch, and
//! the lease's expiry, last heartbeat, state and end reason, which renewals
//! and the lease manager overwrite.

use std::sync::Arc;

use reef_api::v1::{self, Workspace};
use reef_lib::error::{Classify, ErrorKind};
use reef_lib::jj::ReadonlyRepo;
use reef_lib::jj::oplog;
use reef_lib::jj::rewrite::{self, RewriteError};
use tonic::Status;

use super::convert::{WorkspaceView, proto_duration, timestamp};
use crate::daemon::AppState;
use crate::error::{Advice, Advise, status};
use crate::model::{LeaseRecord, RepoId, Timestamp, WorkspaceId, WorkspaceRecord};
use crate::repo::RepoHandle;
use crate::snapshot::index;

/// Operations a lookup by time examines at most.
pub(crate) const MAX_LOCATE_EXAMINED: usize = 100_000;

/// [`MAX_LOCATE_EXAMINED`], lowered by tests that need a lookup to run out
/// (nextest runs each test in its own process).
#[cfg(feature = "testing")]
pub static LOCATE_BUDGET: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(MAX_LOCATE_EXAMINED);

/// Operations a lookup by time examines at most.
pub(crate) fn locate_budget() -> usize {
    #[cfg(feature = "testing")]
    return LOCATE_BUDGET.load(std::sync::atomic::Ordering::Relaxed);
    #[cfg(not(feature = "testing"))]
    MAX_LOCATE_EXAMINED
}

/// Why a read at an operation, or a lookup by time, cannot answer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReplayError {
    /// A walk from an operation reached ancestors garbage collection
    /// removed: reason `OPERATION_LOG_REWRITTEN`.
    #[error("the operation log of repository {repo} was rewritten since this operation was read")]
    Rewritten {
        /// The repository.
        repo: RepoId,
    },
    /// A time before the oldest operation the log keeps: reason
    /// `OPERATION_NOT_RETAINED`.
    #[error("no operation the log keeps ended at or before this time")]
    NotRetained {
        /// When the oldest kept operation ended.
        oldest_at: Timestamp,
    },
    /// The lookup examined its budget without reaching the time: reason
    /// `TOO_FAR_BACK`.
    #[error("more than {examined} operations of repository {repo} ended after this time")]
    TooFarBack {
        /// The repository.
        repo: RepoId,
        /// When the last operation examined ended.
        examined_to: Timestamp,
        /// How many were examined.
        examined: usize,
    },
    /// `at_operation` without a repository to read it in.
    #[error("at_operation needs repo: an operation belongs to one repository")]
    NeedsRepo,
    /// `snapshot_first` with `at_operation`.
    #[error("a read at an operation never snapshots")]
    SnapshotFirst,
}

impl Classify for ReplayError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Rewritten { .. } | Self::NotRetained { .. } | Self::TooFarBack { .. } => {
                ErrorKind::Conflict
            }
            Self::NeedsRepo | Self::SnapshotFirst => ErrorKind::InvalidArgument,
        }
    }
}

impl Advise for ReplayError {
    fn advice(&self) -> Option<Advice> {
        Some(Advice::hint(match self {
            Self::Rewritten { .. } => "Garbage collection rewrote this repository's operation log \
                                       since this operation was read; look the point up again by \
                                       time with GetOperation's ended_at_or_before."
                .to_owned(),
            Self::NotRetained { oldest_at } => format!(
                "The operation log starts at {}: older operations were garbage-collected \
                 (gc.undo_window), or the repository is newer; pick a time at or after it.",
                oldest_at.to_rfc3339()
            ),
            Self::TooFarBack { examined, .. } => format!(
                "More than {examined} operations ended after this time; look it up from a nearer \
                 operation with operation_id, or page ListOperations with at_operation."
            ),
            Self::NeedsRepo => {
                "at_operation needs repo: an operation belongs to one repository.".to_owned()
            }
            Self::SnapshotFirst => {
                "A read at an operation never snapshots; drop snapshot_first.".to_owned()
            }
        }))
    }
}

/// A walk's lookup error as the RPC's: [`ReplayError::Rewritten`] for
/// ancestors GC removed.
pub(crate) fn walk_status(repo: RepoId, error: oplog::OpLookupError) -> Status {
    match error {
        oplog::OpLookupError::Collected => status(ReplayError::Rewritten { repo }),
        error => status(error),
    }
}

/// A Reef workspace's working copy in an operation's view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CopyAt {
    /// The workspace, from the jj workspace's name.
    pub(crate) id: WorkspaceId,
    /// Its working-copy commit.
    pub(crate) commit_id: String,
    /// That commit's change.
    pub(crate) change_id: String,
    /// The change has more than one visible commit in the view.
    pub(crate) divergent: bool,
}

/// An operation's view as the workspace reads need it.
#[derive(Clone, Debug)]
pub(crate) struct ViewAt {
    /// The operation's full id.
    pub(crate) id: String,
    /// When it ended.
    pub(crate) end: Timestamp,
    /// The Reef workspaces' working copies in its view.
    pub(crate) copies: Vec<CopyAt>,
}

/// The view of `repo` at the operation `text` names, read through
/// [`crate::read::Reads::at`].
pub(crate) async fn view_at(
    state: &AppState,
    handle: &RepoHandle,
    repo: RepoId,
    text: &str,
) -> Result<ViewAt, Status> {
    let head = state.reads.current(handle).await.map_err(status)?;
    let reads = Arc::clone(&state.reads);
    let text = text.to_owned();
    state
        .reads
        .run(handle, move || async move {
            let at = reads.at(repo, &head, &text).await.map_err(status)?;
            let entry = oplog::entry(at.operation());
            let copies = copies_at(&at).await.map_err(status)?;
            Ok::<_, Status>(ViewAt {
                id: entry.id.to_string(),
                end: entry.end.into(),
                copies,
            })
        })
        .await
        .map_err(status)?
}

/// The working copies of Reef workspaces (`reef-<id>`) in `repo`'s view,
/// in no particular order. Runs on the blocking side.
pub(crate) async fn copies_at(repo: &ReadonlyRepo) -> Result<Vec<CopyAt>, RewriteError> {
    Ok(rewrite::working_copies(repo)
        .await?
        .into_iter()
        .filter_map(|copy| {
            Some(CopyAt {
                id: copy.workspace.strip_prefix("reef-")?.parse().ok()?,
                commit_id: copy.commit_id.to_string(),
                change_id: copy.change_id.to_string(),
                divergent: copy.divergent,
            })
        })
        .collect())
}

/// Each copy with its row, if the row is still there: GC deletes rows only
/// of workspaces released before the undo window, which no retained view
/// holds, so a missing row means a restore or another state directory.
pub(crate) fn rows_at(
    state: &AppState,
    copies: Vec<CopyAt>,
) -> Result<Vec<(CopyAt, Option<WorkspaceRecord>)>, Status> {
    copies
        .into_iter()
        .map(|copy| match state.db.workspace(copy.id) {
            Ok(record) => Ok((copy, Some(record))),
            Err(crate::db::DbError::NotFound { .. }) => Ok((copy, None)),
            Err(error) => Err(status(error)),
        })
        .collect()
}

/// When a workspace's row says it was created; a missing row sorts last.
pub(crate) fn created_at(row: &(CopyAt, Option<WorkspaceRecord>)) -> i64 {
    row.1
        .as_ref()
        .map_or(i64::MIN, |record| record.created_at.0)
}

/// The lease a workspace held at `end`: the newest issued by then and not
/// ended by then, or, when every lease was issued after it, the first if it
/// had not ended by then (a create's lease is issued just after its
/// operation ends).
pub(crate) fn lease_at(leases: &[LeaseRecord], end: Timestamp) -> Option<&LeaseRecord> {
    let held = |lease: &&LeaseRecord| lease.ended_at.is_none_or(|ended| ended > end);
    leases
        .iter()
        .rev()
        .filter(|lease| lease.issued_at <= end)
        .find(held)
        .or_else(|| {
            leases
                .first()
                .filter(|lease| lease.issued_at > end)
                .filter(held)
        })
}

/// `copies` and their rows as messages, as of an operation of `repo` that
/// ended at `end`.
pub(crate) fn workspaces_at(
    state: &AppState,
    repo: RepoId,
    rows: &[(CopyAt, Option<WorkspaceRecord>)],
    end: Timestamp,
) -> Result<Vec<Workspace>, Status> {
    let local_counts = index::local_counts_at(&state.db, repo, end).map_err(status)?;
    rows.iter()
        .map(|(copy, record)| {
            let Some(record) = record else {
                return Ok(Workspace {
                    id: copy.id.as_hex(),
                    repo_id: repo.as_hex(),
                    change_id: copy.change_id.clone(),
                    commit_id: copy.commit_id.clone(),
                    state: v1::WorkspaceState::Active.into(),
                    divergent: copy.divergent,
                    local_snapshot_count: local_counts.get(&copy.id).copied().unwrap_or(0),
                    ..Workspace::default()
                });
            };
            let leases = state.db.leases_of(record.id).map_err(status)?;
            let last = index::recent_at(&state.db, record.id, end, 1)
                .map_err(status)?
                .pop();
            let then = WorkspaceRecord {
                change_id: copy.change_id.clone(),
                commit_id: copy.commit_id.clone(),
                state: crate::model::WorkspaceState::Active,
                divergent: copy.divergent,
                attention: None,
                last_snapshot_at: last.as_ref().map(|entry| entry.taken_at),
                last_snapshot_op: last.as_ref().map(|entry| entry.op_id.to_string()),
                ..record.clone()
            };
            let mut workspace = Workspace::from(WorkspaceView {
                record: &then,
                lease: lease_at(&leases, end).map(|lease| lease_message(state, lease)),
                local_snapshot_count: local_counts.get(&record.id).copied().unwrap_or(0),
                stores: &[],
                watch_pending: None,
                loaded: false,
            });
            workspace.attention = v1::Attention::Unspecified.into();
            Ok(workspace)
        })
        .collect()
}

/// A lease as of an operation: what its row keeps from the issue.
fn lease_message(state: &AppState, lease: &LeaseRecord) -> v1::Lease {
    v1::Lease {
        id: lease.id.as_hex(),
        owner: lease.owner.clone(),
        issued_at: Some(timestamp(lease.issued_at)),
        length: Some(proto_duration(lease.length)),
        grace: Some(proto_duration(lease.grace)),
        heartbeat_every: Some(proto_duration(
            state.manager.leases().heartbeat_every(lease.length),
        )),
        ..v1::Lease::default()
    }
}

/// The ids of `copies` on `change` other than `id`, sorted.
pub(crate) fn others_on_change(copies: &[CopyAt], id: WorkspaceId, change: &str) -> Vec<String> {
    let mut others: Vec<String> = copies
        .iter()
        .filter(|copy| copy.id != id && copy.change_id == change)
        .map(|copy| copy.id.as_hex())
        .collect();
    others.sort();
    others
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// An id of 16 hex digits, `byte` repeated.
    fn id<T: std::str::FromStr<Err: std::fmt::Debug>>(byte: u8) -> T {
        format!("{byte:02x}").repeat(8).parse().unwrap()
    }

    fn lease(issued: i64, ended: Option<i64>) -> LeaseRecord {
        LeaseRecord {
            id: id(u8::try_from(issued % 200).unwrap()),
            repo_id: id(1),
            workspace_id: id(2),
            owner: "a@test".to_owned(),
            issued_at: Timestamp(issued),
            expires_at: Timestamp(issued + 1000),
            last_heartbeat_at: Timestamp(issued),
            length: Duration::from_secs(1),
            grace: Duration::from_secs(1),
            ended_at: ended.map(Timestamp),
            end_reason: None,
        }
    }

    #[test]
    fn the_lease_at_a_point() {
        let leases = [lease(100, Some(200)), lease(300, None)];
        assert_eq!(lease_at(&leases, Timestamp(150)), Some(&leases[0]));
        // Ended by then, and the next not issued yet.
        assert_eq!(lease_at(&leases, Timestamp(250)), None);
        assert_eq!(lease_at(&leases, Timestamp(300)), Some(&leases[1]));
        // Issued just after the create's operation ended.
        assert_eq!(lease_at(&leases, Timestamp(99)), Some(&leases[0]));
        let ended_early = [lease(100, Some(90))];
        assert_eq!(lease_at(&ended_early, Timestamp(95)), None);
        assert_eq!(lease_at(&[], Timestamp(95)), None);
    }

    #[test]
    fn others_on_a_change_leave_the_workspace_out() {
        let copy = |byte: u8, change: &str| CopyAt {
            id: id(byte),
            commit_id: String::new(),
            change_id: change.to_owned(),
            divergent: false,
        };
        let copies = [copy(3, "c"), copy(1, "c"), copy(2, "d"), copy(4, "c")];
        assert_eq!(
            others_on_change(&copies, id(1), "c"),
            vec![id::<WorkspaceId>(3).as_hex(), id::<WorkspaceId>(4).as_hex()]
        );
    }
}
