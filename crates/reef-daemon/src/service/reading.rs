//! What the read handlers (R2.02) share: the repository and workspace a
//! request names, the snapshot a read takes first, and operations as
//! messages with their kinds and effects.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Instant;

use reef_api::v1;
use reef_lib::jj::OperationIdHex;
use reef_lib::jj::ReadonlyRepo;
use reef_lib::jj::oplog::{self, OpEntry, OpLookupError};
use reef_lib::jj::revset::{self, RevsetScope};
use tonic::Status;

use super::convert::{self, Recorded};
use super::page::{PageError, PageTokenError};
use crate::daemon::AppState;
use crate::error::status;
use crate::model::{RepoId, RepositoryRecord, WorkspaceId, WorkspaceRecord, WorkspaceState};
use crate::read::{PinnedGone, ReadError, Reads};
use crate::repo::RepoHandle;
use crate::snapshot::WalkReason;
use crate::snapshot::index;
use crate::workspace::{ReadSnapshot, ReadSource};

/// The repository and workspace a read names.
#[derive(Clone, Debug)]
pub(super) struct Target {
    pub(super) repo: RepositoryRecord,
    pub(super) workspace: Option<WorkspaceRecord>,
    /// The workspace's jj name, `reef-<id>`.
    pub(super) jj_workspace: Option<String>,
    pub(super) handle: RepoHandle,
    pub(super) user_email: String,
}

impl Target {
    /// What revsets of this read are parsed against.
    pub(super) fn scope(&self) -> RevsetScope<'_> {
        RevsetScope {
            workspace: self.jj_workspace.as_deref(),
            user_email: &self.user_email,
            root: &self.repo.root,
        }
    }

    /// The workspace, when it is active: only an active workspace is
    /// snapshotted before a read.
    pub(super) fn active(&self) -> Option<&WorkspaceRecord> {
        self.workspace
            .as_ref()
            .filter(|record| record.state == WorkspaceState::Active)
    }
}

/// The repository `repo` names, or the one `workspace` is in when it is
/// named; `workspace` in any state.
pub(super) fn target(state: &AppState, repo: &str, workspace: &str) -> Result<Target, Status> {
    let manager = &state.manager;
    let (repo, workspace) = if workspace.trim().is_empty() {
        (manager.resolve_repo(repo).map_err(status)?, None)
    } else {
        let record = manager.resolve_workspace(repo, workspace).map_err(status)?;
        let repo = state.db.repository(record.repo_id).map_err(status)?;
        (repo, Some(record))
    };
    let span = tracing::Span::current();
    span.record("repo", tracing::field::display(repo.id));
    if let Some(record) = &workspace {
        span.record("workspace_id", tracing::field::display(record.id));
    }
    let handle = manager.handle(repo.id).map_err(status)?;
    Ok(Target {
        jj_workspace: workspace
            .as_ref()
            .map(|record| record.id.jj_workspace_name()),
        repo,
        workspace,
        handle,
        user_email: state.settings.user_email().to_owned(),
    })
}

/// What [`snapshot_first`] did.
#[derive(Debug, Default)]
pub(super) struct Snapshotted {
    /// The operation it recorded, with its effects; `None` when it found
    /// nothing new, a scan since the request arrived covered it, nothing
    /// changed since the last scan, or it did not snapshot.
    pub(super) operation: Option<v1::Operation>,
    /// Where the files came from; `None` when it did not snapshot.
    pub(super) source: Option<ReadSource>,
}

impl Snapshotted {
    /// `source` and `walk_reason` for a workspace's status (R2.23).
    pub(super) fn status_source(&self) -> (v1::StatusSource, v1::StatusWalkReason) {
        match self.source {
            None | Some(ReadSource::Snapshot) => (
                v1::StatusSource::Snapshot,
                v1::StatusWalkReason::Unspecified,
            ),
            Some(ReadSource::Index) => (v1::StatusSource::Index, v1::StatusWalkReason::Unspecified),
            Some(ReadSource::Walk(reason)) => (v1::StatusSource::Walk, walk_reason(reason)),
        }
    }
}

/// `reason` in the API.
pub(super) fn walk_reason(reason: WalkReason) -> v1::StatusWalkReason {
    match reason {
        WalkReason::NotWatched => v1::StatusWalkReason::NotWatched,
        WalkReason::WatchRestarted => v1::StatusWalkReason::WatchRestarted,
        WalkReason::EventsLost => v1::StatusWalkReason::EventsLost,
        WalkReason::TooBroad => v1::StatusWalkReason::TooBroad,
        WalkReason::SnapshotFailed => v1::StatusWalkReason::SnapshotFailed,
        WalkReason::NotSynced => v1::StatusWalkReason::NotSynced,
        WalkReason::StaleWorkingCopy => v1::StatusWalkReason::StaleWorkingCopy,
        WalkReason::PathNotNarrowable => v1::StatusWalkReason::PathNotNarrowable,
        WalkReason::CoveredByFullScan => v1::StatusWalkReason::CoveredByFullScan,
    }
}

/// Snapshots `target`'s workspace before a read when `needed` and the
/// workspace is active, for `agent` (the request's `reef-agent`); a
/// failure fails the read.
pub(super) async fn snapshot_first(
    state: &AppState,
    target: &Target,
    needed: bool,
    arrived: Instant,
    agent: Option<String>,
) -> Result<Snapshotted, Status> {
    let Some(record) = target.active().filter(|_| needed) else {
        return Ok(Snapshotted::default());
    };
    let taken = state
        .manager
        .snapshot_for_read(record.id, arrived, agent.clone())
        .await
        .map_err(|error| {
            status(ReadError::Snapshot {
                workspace: record.id,
                source: Box::new(error),
            })
        })?;
    tracing::debug!(
        workspace_id = %record.id,
        source = ?taken.source,
        "read answered"
    );
    let source = Some(taken.source);
    let ReadSnapshot::Taken(outcome) = taken.snapshot else {
        return Ok(Snapshotted {
            operation: None,
            source,
        });
    };
    let op_id = outcome.op_id.to_string();
    let workspace_id = record.id.as_hex();
    let operation = super::written_operation(
        state,
        record.repo_id,
        outcome.changed.then_some(op_id.as_str()),
        convert::Written {
            kind: v1::OperationKind::Snapshot,
            snapshot: Some(crate::snapshot::SnapshotKind::Explicit),
            workspace: Some(&workspace_id),
            agent: agent.as_deref().unwrap_or(&record.owner),
        },
    )
    .await?;
    Ok(Snapshotted { operation, source })
}

/// What the daemon's records say about listed operations: their snapshot
/// index rows and their workspaces' owners. Read on the async side.
#[derive(Debug, Default)]
pub(crate) struct Records {
    kinds: HashMap<OperationIdHex, crate::snapshot::SnapshotKind>,
    owners: HashMap<WorkspaceId, String>,
}

impl Records {
    /// The records of `entries`.
    pub(crate) fn of(state: &AppState, entries: &[OpEntry]) -> Result<Records, Status> {
        let ids: Vec<OperationIdHex> = entries.iter().map(|entry| entry.id.clone()).collect();
        let kinds = index::kinds_of(&state.db, &ids).map_err(status)?;
        let workspaces: BTreeSet<WorkspaceId> = entries
            .iter()
            .filter_map(|entry| oplog::classify(entry).workspace_id)
            .filter_map(|id| id.parse().ok())
            .collect();
        let workspaces: Vec<WorkspaceId> = workspaces.into_iter().collect();
        let owners = state.db.owners(&workspaces).map_err(status)?;
        Ok(Records { kinds, owners })
    }

    /// The owner of the workspace `id` (hex), if it is one of the entries'.
    pub(crate) fn owner(&self, id: &str) -> Option<&str> {
        let id = id.parse::<WorkspaceId>().ok()?;
        self.owners.get(&id).map(String::as_str)
    }

    /// `entry` as a message, classified, without effects.
    pub(crate) fn operation(&self, entry: &OpEntry, repo: RepoId) -> v1::Operation {
        let class = oplog::classify(entry);
        let owner = class
            .workspace_id
            .as_deref()
            .and_then(|id| id.parse::<WorkspaceId>().ok())
            .and_then(|id| self.owners.get(&id))
            .map(String::as_str);
        convert::listed_operation(
            entry,
            repo,
            &class,
            Recorded {
                indexed: self.kinds.get(&entry.id).copied(),
                owner,
            },
        )
    }
}

/// The effects of the operation `id` of `target`'s repository, computed
/// on the blocking pool over the repository as its thread last loaded it,
/// without waiting for a read permit (a write reports them); `None`,
/// logged, when they cannot be.
pub(super) async fn effects_of(
    state: &AppState,
    handle: &RepoHandle,
    head: Option<Arc<ReadonlyRepo>>,
    id: &str,
) -> Option<v1::OperationEffects> {
    let head = match head {
        Some(head) => head,
        None => state.reads.loaded(handle).await.ok()?,
    };
    let reads = Arc::clone(&state.reads);
    let repo = handle.id();
    let id = id.to_owned();
    let found = state
        .reads
        .run_for_write(handle, move || async move {
            let operation = match oplog::resolve_operation(&head, &id).await {
                Ok(operation) => operation,
                Err(error) => {
                    tracing::warn!(
                        op_id = %id,
                        error = %reef_lib::error::ErrorChain(&error),
                        "cannot read the operation for its effects"
                    );
                    return None;
                }
            };
            reads.effects(repo, &head, &operation).await
        })
        .await
        .ok()
        .flatten()?;
    Some(convert::effects(&found))
}

/// A token whose cursor or pinned state does not fit: `INVALID_PAGE_TOKEN`.
pub(super) fn bad_token() -> Status {
    status(PageError::from(PageTokenError::Malformed))
}

/// Whether any of `texts` (revsets or revisions; empty ones skipped) uses
/// `@`, parsed against the repository at head. A text that does not parse
/// fails here with its own error.
pub(super) async fn uses_working_copy(
    state: &AppState,
    target: &Target,
    texts: Vec<String>,
) -> Result<bool, Status> {
    let texts: Vec<String> = texts
        .into_iter()
        .filter(|text| !text.trim().is_empty())
        .collect();
    if texts.is_empty() || target.active().is_none() {
        return Ok(false);
    }
    let head = state.reads.current(&target.handle).await.map_err(status)?;
    let target = target.clone();
    state
        .reads
        .run(&target.handle.clone(), move || async move {
            for text in &texts {
                if revset::parse(&head, text, &target.scope())
                    .map_err(status)?
                    .uses_working_copy()
                {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .await
        .map_err(status)?
}

/// The repository a read answers from: at `pinned` (a page token's
/// operation, which must still exist), else at `at_operation`, else
/// `head`. Runs on the blocking pool.
pub(super) async fn repo_at(
    reads: &Reads,
    repo: RepoId,
    head: &Arc<ReadonlyRepo>,
    pinned: Option<&str>,
    at_operation: &str,
) -> Result<Arc<ReadonlyRepo>, Status> {
    if let Some(pinned) = pinned {
        return reads
            .at(repo, head, pinned)
            .await
            .map_err(|error| match error {
                OpLookupError::NotFound(_) => status(PinnedGone(error)),
                error => status(error),
            });
    }
    if at_operation.trim().is_empty() {
        return Ok(Arc::clone(head));
    }
    reads.at(repo, head, at_operation).await.map_err(status)
}

/// A positional cursor, `<skip>:<last key>`.
pub(super) fn positional(cursor: &str) -> Result<(usize, String), Status> {
    let (skip, last) = cursor.split_once(':').ok_or_else(bad_token)?;
    Ok((skip.parse().map_err(|_| bad_token())?, last.to_owned()))
}

/// The repository's current operation id, as a response reports it.
pub(super) fn op_id(repo: &ReadonlyRepo) -> String {
    oplog::entry(repo.operation()).id.to_string()
}
