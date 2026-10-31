//! `reef.api.v1.StatusService`: `GetStatus` (R2.02). The repository's
//! state and, when a workspace is named, what it has changed against its
//! parents, read from one view after the workspace's snapshot. R2.23
//! answers the workspace part from an index instead; the shape stays.
//!
//! With `at_operation` (R2.12) both parts are read from that operation's
//! view, and its active workspaces are the working copies the view holds
//! (`at_operation::copies_at`), not the rows active now. Nothing is
//! snapshotted.

use std::sync::Arc;
use std::time::Instant;

use reef_api::v1::status_service_server::StatusService;
use reef_api::v1::{self, GetStatusRequest, GetStatusResponse};
use reef_lib::error::{Classify, ErrorKind};
use reef_lib::jj::diff::{self, DiffBase, DiffFormat, DiffOptions};
use reef_lib::jj::effects::OpEffects;
use reef_lib::jj::log::{self, Describer};
use reef_lib::jj::oplog::{self, OpEntry};
use reef_lib::jj::revset;
use tonic::{Request, Response, Status};

use super::at_operation::{CopyAt, copies_at, created_at, rows_at, workspaces_at};
use super::convert;
use super::reading::{Records, op_id, snapshot_first, target};
use super::workspaces::listed;
use crate::daemon::AppState;
use crate::error::status;
use crate::model::{Timestamp, WorkspaceState};
use crate::workspace::ManagerError;

/// Changed files when the request says 0.
const DEFAULT_MAX_FILES: usize = 100;
/// The most changed files a status returns.
const MAX_MAX_FILES: usize = 1000;
/// Workspaces, heads and change ids a repository status lists at most.
const MAX_LISTED: usize = 100;
/// Conflicted paths a workspace status lists at most.
const MAX_CONFLICTED_PATHS: usize = 1000;

/// Serves `StatusService`.
#[derive(Debug)]
pub struct StatusHandler {
    state: Arc<AppState>,
}

impl StatusHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> StatusHandler {
        StatusHandler { state }
    }
}

/// A negative `max_files`.
#[derive(Debug, thiserror::Error)]
#[error("max_files must not be negative (got {0})")]
pub struct NegativeMaxFiles(pub i32);

impl Classify for NegativeMaxFiles {
    fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidArgument
    }
}

/// What the blocking half reads from one view.
struct Read {
    at: String,
    trunk: Option<(v1::Change, String)>,
    heads: Vec<v1::Change>,
    head_count: usize,
    conflicted: (Vec<String>, usize),
    divergent: (Vec<String>, usize),
    last: (OpEntry, Option<OpEffects>),
    workspace: Option<WorkspaceRead>,
    /// The working copies in the view read at an operation; `None` at
    /// head.
    copies: Option<Vec<CopyAt>>,
}

struct WorkspaceRead {
    working_copy: v1::Change,
    parents: Vec<v1::Change>,
    changed: Vec<v1::FileChange>,
    changed_count: usize,
    conflicted: Vec<String>,
    conflicted_count: usize,
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[tonic::async_trait]
impl StatusService for StatusHandler {
    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn get_status(
        &self,
        request: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        let arrived = Instant::now();
        let agent = super::agent_of(request.metadata())?;
        let request = request.into_inner();
        let max_files = match usize::try_from(request.max_files) {
            Err(_) => return Err(status(NegativeMaxFiles(request.max_files))),
            Ok(0) => DEFAULT_MAX_FILES,
            Ok(n) => n.min(MAX_MAX_FILES),
        };
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let at_operation = request.at_operation.trim().to_owned();
        let at = !at_operation.is_empty();
        if let Some(record) = &target.workspace
            && record.state != WorkspaceState::Active
            && !at
        {
            return Err(status(ManagerError::NotActive {
                id: record.id,
                repo: record.repo_id,
                state: record.state,
            }));
        }
        let snapshot = if request.skip_snapshot || at {
            None
        } else {
            Some(snapshot_first(state, &target, target.workspace.is_some(), arrived, agent).await?)
        };
        let head = state.reads.current(&target.handle).await.map_err(status)?;
        let reads = Arc::clone(&state.reads);
        let (stop, _guard) = state.reads.stop();
        let read_target = target.clone();
        let read = state
            .reads
            .run(&target.handle, move || async move {
                let target = read_target;
                let repo = &if at {
                    reads
                        .at(target.repo.id, &head, &at_operation)
                        .await
                        .map_err(status)?
                } else {
                    Arc::clone(&head)
                };
                let copies = if at {
                    Some(copies_at(repo).await.map_err(status)?)
                } else {
                    None
                };
                let scope = target.scope();
                let describer = Describer::new(repo, &scope).map_err(status)?;
                let trunk = match revset::trunk(repo) {
                    Some(trunk) => {
                        let info = describer.describe(&trunk.commit_id).await.map_err(status)?;
                        Some((convert::change(&info), trunk.bookmark))
                    }
                    None => None,
                };
                let parsed = revset::parse(repo, "heads(mutable())", &scope).map_err(status)?;
                let (ids, head_count) =
                    log::commits(repo, &parsed, usize::MAX, &|| stop.requested())
                        .await
                        .map_err(status)?;
                let mut heads = Vec::with_capacity(ids.len());
                for id in &ids {
                    heads.push(describer.describe(id).await.map_err(status)?);
                }
                // Newest committer time first, ties by commit id descending.
                heads.sort_by(|a, b| {
                    (b.committer.at, &b.commit_id).cmp(&(a.committer.at, &a.commit_id))
                });
                heads.truncate(MAX_LISTED);
                let changes = async |text: &str| {
                    let parsed = revset::parse(repo, text, &scope).map_err(status)?;
                    let (ids, total) = log::change_ids(repo, &parsed, MAX_LISTED)
                        .await
                        .map_err(status)?;
                    Ok::<_, Status>((ids.iter().map(ToString::to_string).collect(), total))
                };
                let conflicted = changes("mutable() & conflicts()").await?;
                let divergent = changes("divergent()").await?;
                let operation = repo.operation();
                let last = (
                    oplog::entry(operation),
                    reads.effects(target.repo.id, &head, operation).await,
                );
                let workspace = if target.jj_workspace.is_some() {
                    let wc = revset::resolve_revision(repo, "@", &scope)
                        .await
                        .map_err(status)?;
                    let info = describer.describe(&wc.commit_id).await.map_err(status)?;
                    let mut parents = Vec::with_capacity(info.parent_commit_ids.len());
                    for parent in &info.parent_commit_ids {
                        let parent = describer.describe(parent).await.map_err(status)?;
                        parents.push(convert::change(&parent));
                    }
                    let options = DiffOptions {
                        format: DiffFormat::Summary,
                        context: 0,
                        max_lines: 0,
                        max_hunk_bytes: 0,
                        max_file_bytes: 0,
                        paths: Vec::new(),
                        after: None,
                        limit: max_files,
                        pairs: false,
                    };
                    let changed =
                        diff::diff_page(repo, &DiffBase::Parents, &wc.commit_id, &options, &|| {
                            false
                        })
                        .await
                        .map_err(status)?;
                    let (conflicted, conflicted_count) =
                        diff::conflicted_paths(repo, &wc.commit_id, MAX_CONFLICTED_PATHS)
                            .map_err(status)?;
                    Some(WorkspaceRead {
                        working_copy: convert::change(&info),
                        parents,
                        changed: changed.files.iter().map(convert::file_change).collect(),
                        changed_count: changed.total,
                        conflicted,
                        conflicted_count,
                    })
                } else {
                    None
                };
                Ok::<_, Status>(Read {
                    at: op_id(repo),
                    trunk,
                    heads: heads.iter().map(convert::change).collect(),
                    head_count,
                    conflicted,
                    divergent,
                    last,
                    workspace,
                    copies,
                })
            })
            .await
            .map_err(status)??;

        let (last_entry, last_effects) = read.last;
        let end = Timestamp::from(last_entry.end);
        let (active_workspaces, active_count, workspace_then) = if let Some(copies) = read.copies {
            let mut rows = rows_at(state, copies)?;
            let then = target.workspace.as_ref().and_then(|record| {
                rows.iter()
                    .find(|(copy, _)| copy.id == record.id)
                    .map(|(copy, _)| (copy.clone(), Some(record.clone())))
            });
            let count = rows.len();
            rows.sort_by(|a, b| {
                (created_at(b), b.0.id.as_hex()).cmp(&(created_at(a), a.0.id.as_hex()))
            });
            rows.truncate(MAX_LISTED);
            let workspaces = workspaces_at(state, target.repo.id, &rows, end)?;
            let then = match then {
                Some(row) => workspaces_at(state, target.repo.id, &[row], end)?.pop(),
                None => None,
            };
            (workspaces, count, then)
        } else {
            let mut rows = state
                .manager
                .list(Some(target.repo.id), &[WorkspaceState::Active], None)
                .map_err(status)?;
            let count = rows.len();
            rows.sort_by(|(a, _), (b, _)| {
                (b.created_at.0, b.id.as_hex()).cmp(&(a.created_at.0, a.id.as_hex()))
            });
            rows.truncate(MAX_LISTED);
            (listed(state, Some(target.repo.id), &rows)?, count, None)
        };
        let records = Records::of(state, std::slice::from_ref(&last_entry))?;
        let mut last_operation = records.operation(&last_entry, target.repo.id);
        last_operation.effects = last_effects.as_ref().map(convert::effects);
        let (trunk, trunk_bookmark) = read
            .trunk
            .map_or((None, String::new()), |(change, bookmark)| {
                (Some(change), bookmark)
            });
        let repository_status = v1::RepositoryStatus {
            trunk,
            trunk_bookmark,
            active_workspaces,
            active_workspace_count: to_u32(active_count),
            heads: read.heads,
            head_count: to_u32(read.head_count),
            conflicted_change_ids: read.conflicted.0,
            divergent_change_ids: read.divergent.0,
            last_operation: Some(last_operation),
        };
        let workspace_status = match (target.workspace.as_ref(), read.workspace) {
            (Some(_), Some(found)) if at => Some(v1::WorkspaceStatus {
                workspace: workspace_then,
                working_copy: Some(found.working_copy),
                parents: found.parents,
                changed_files: found.changed,
                changed_file_count: to_u32(found.changed_count),
                conflicted_paths: found.conflicted,
                source: v1::StatusSource::Operation.into(),
                snapshot: None,
                conflicted_path_count: to_u32(found.conflicted_count),
                walk_reason: v1::StatusWalkReason::Unspecified.into(),
            }),
            (Some(record), Some(found)) => {
                let inspected = state
                    .manager
                    .inspect(record.id, false, false, None)
                    .await
                    .map_err(status)?;
                let workspace = listed(
                    state,
                    Some(target.repo.id),
                    &[(inspected.record.clone(), inspected.lease.clone())],
                )?
                .pop();
                let (source, walk_reason) = match &snapshot {
                    Some(taken) => taken.status_source(),
                    None => (
                        v1::StatusSource::LastSnapshot,
                        v1::StatusWalkReason::Unspecified,
                    ),
                };
                Some(v1::WorkspaceStatus {
                    workspace,
                    working_copy: Some(found.working_copy),
                    parents: found.parents,
                    changed_files: found.changed,
                    changed_file_count: to_u32(found.changed_count),
                    conflicted_paths: found.conflicted,
                    source: source.into(),
                    snapshot: snapshot.and_then(|taken| taken.operation),
                    conflicted_path_count: to_u32(found.conflicted_count),
                    walk_reason: walk_reason.into(),
                })
            }
            _ => None,
        };
        Ok(Response::new(GetStatusResponse {
            repository: Some(v1::Repository::from(&target.repo)),
            at_operation: read.at,
            repository_status: Some(repository_status),
            workspace_status,
        }))
    }
}
