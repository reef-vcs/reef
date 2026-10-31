//! `reef.api.v1.WorkspaceService`: create, list, get, release, snapshot,
//! renew, move.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reef_api::v1::workspace_service_server::WorkspaceService;
use reef_api::v1::{
    self, CreateWorkspaceRequest, CreateWorkspaceResponse, GetWorkspaceRequest,
    GetWorkspaceResponse, ListWorkspacesRequest, ListWorkspacesResponse, MoveWorkspaceRequest,
    MoveWorkspaceResponse, OperationKind, ReleaseWorkspaceRequest, ReleaseWorkspaceResponse,
    RenewLeaseRequest, RenewLeaseResponse, SnapshotWorkspaceRequest, SnapshotWorkspaceResponse,
    Workspace, create_workspace_request, move_workspace_request,
};
use reef_lib::jj::diff::{self, DiffBase, DiffFormat, DiffOptions};
use reef_lib::jj::revset::{self, RevisionError, RevsetScope};
use tonic::{Request, Response, Status};

use super::at_operation::{self, ReplayError};
use super::convert::{
    WorkspaceView, Written, backend_choice, known, lease, missing_stores, request_duration,
    state_of,
};
use super::page::{PageRequest, newest_first};
use super::written_operation;
use crate::daemon::AppState;
use crate::error::status;
use crate::model::{
    LeaseId, LeaseRecord, ReclaimReason, RepoId, Timestamp, WorkspaceRecord, WorkspaceState,
};
use crate::snapshot::{SnapshotKind, index};
use crate::workspace::{CreateSpec, ReleaseSpec, Start};

/// Serves `WorkspaceService`.
#[derive(Debug)]
pub struct WorkspacesService {
    state: Arc<AppState>,
}

impl WorkspacesService {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> WorkspacesService {
        WorkspacesService { state }
    }

    /// `ListWorkspaces` at an operation (R2.12): the workspaces whose
    /// working copy its view holds, as of then.
    async fn list_workspaces_at(
        &self,
        request: ListWorkspacesRequest,
    ) -> Result<Response<ListWorkspacesResponse>, Status> {
        let page = PageRequest::new(
            "/reef.api.v1.WorkspaceService/ListWorkspaces",
            request.page_size,
            &request.page_token,
            &ListWorkspacesRequest {
                page_size: 0,
                page_token: String::new(),
                changed_file_counts: false,
                ..request.clone()
            },
        )
        .map_err(status)?;
        if request.repo.trim().is_empty() {
            return Err(status(ReplayError::NeedsRepo));
        }
        let target = super::reading::target(&self.state, &request.repo, "")?;
        let states: Vec<WorkspaceState> = request
            .states
            .iter()
            .map(|state| known("states", *state))
            .collect::<Result<Vec<_>, _>>()
            .map_err(status)?
            .into_iter()
            .filter_map(state_of)
            .collect();
        // Only active workspaces have a working copy in a view.
        let active = states.is_empty() || states.contains(&WorkspaceState::Active);
        let owner = non_empty(request.owner);
        let view = at_operation::view_at(
            &self.state,
            &target.handle,
            target.repo.id,
            &request.at_operation,
        )
        .await?;
        let rows: Vec<_> = at_operation::rows_at(&self.state, view.copies)?
            .into_iter()
            .filter(|(_, record)| {
                active
                    && owner.as_ref().is_none_or(|owner| {
                        record.as_ref().is_some_and(|record| record.owner == *owner)
                    })
            })
            .collect();
        let (rows, next_page_token) = page.page(rows, |row| {
            newest_first(at_operation::created_at(row), &row.0.id.as_hex())
        });
        let mut workspaces =
            at_operation::workspaces_at(&self.state, target.repo.id, &rows, view.end)?;
        if request.changed_file_counts {
            // Each working copy at the operation, against its parents.
            let names = rows
                .iter()
                .map(|(copy, _)| copy.id.jj_workspace_name())
                .collect();
            let at = Some(view.id.clone());
            match tokio::time::timeout(
                COUNT_TIMEOUT,
                changed_counts(&self.state, target.repo.id, names, at),
            )
            .await
            {
                Ok(Ok(counts)) => {
                    for (workspace, count) in workspaces.iter_mut().zip(counts) {
                        workspace.changed_file_count = count;
                    }
                }
                Ok(Err(error)) => tracing::warn!(
                    error = %error.message(),
                    "could not count workspaces' changed files"
                ),
                Err(_) => tracing::warn!(
                    timeout_ms = COUNT_TIMEOUT.as_millis(),
                    "counting workspaces' changed files timed out; listed without counts"
                ),
            }
        }
        Ok(Response::new(ListWorkspacesResponse {
            workspaces,
            next_page_token,
            at_operation: view.id,
        }))
    }

    /// `GetWorkspace` at an operation (R2.12): `record` as of then.
    async fn get_workspace_at(
        &self,
        record: WorkspaceRecord,
        text: &str,
    ) -> Result<Response<GetWorkspaceResponse>, Status> {
        let state = &self.state;
        let handle = state.manager.handle(record.repo_id).map_err(status)?;
        let view = at_operation::view_at(state, &handle, record.repo_id, text).await?;
        let Some(copy) = view
            .copies
            .iter()
            .find(|copy| copy.id == record.id)
            .cloned()
        else {
            return Err(status(RevisionError::WorkingCopyNotFound {
                workspace: record.id.jj_workspace_name(),
                at_operation: view.id,
            }));
        };
        let others = at_operation::others_on_change(&view.copies, record.id, &copy.change_id);
        let (snapshot_count, _) =
            index::counts_at(&state.db, record.id, view.end).map_err(status)?;
        let recent = index::recent_at(&state.db, record.id, view.end, 10).map_err(status)?;
        let repo = record.repo_id;
        let workspace =
            at_operation::workspaces_at(state, repo, &[(copy, Some(record))], view.end)?.pop();
        Ok(Response::new(GetWorkspaceResponse {
            workspace,
            disk_usage: None,
            other_workspaces_on_change: others,
            snapshot: None,
            snapshot_count,
            recent_snapshots: recent.iter().map(v1::SnapshotEntry::from).collect(),
            at_operation: view.id,
        }))
    }

    /// `record` as a message, its lease state computed at `now`.
    fn lease_message(&self, record: Option<&LeaseRecord>, now: Timestamp) -> Option<v1::Lease> {
        record.map(|record| lease(record, self.state.manager.leases(), now))
    }
}

/// Workspace rows and their leases as messages, as `ListWorkspaces` lists
/// them; `repo` scopes the local snapshot counts read.
pub(super) fn listed(
    state: &AppState,
    repo: Option<RepoId>,
    rows: &[(WorkspaceRecord, Option<LeaseRecord>)],
) -> Result<Vec<Workspace>, Status> {
    let manager = &state.manager;
    let now = Timestamp::now();
    let local_counts = index::local_counts(&state.db, repo).map_err(status)?;
    Ok(rows
        .iter()
        .map(|(record, lease_record)| {
            let entry = manager.entry(record.id);
            let stores = entry
                .as_ref()
                .filter(|_| record.state == WorkspaceState::Active)
                .map(|entry| entry.stores())
                .unwrap_or_default();
            Workspace::from(WorkspaceView {
                record,
                lease: lease_record
                    .as_ref()
                    .map(|record| lease(record, manager.leases(), now)),
                local_snapshot_count: local_counts.get(&record.id).copied().unwrap_or(0),
                stores: &stores,
                watch_pending: manager.scheduler().watch_pending(record.id),
                loaded: entry.is_some(),
            })
        })
        .collect())
}

/// How long `ListWorkspaces` waits for one repository's changed-file
/// counts before listing its workspaces without them.
const COUNT_TIMEOUT: Duration = Duration::from_secs(5);

/// Fills each active or releasing workspace's `changed_file_count` from
/// `rows`, which `workspaces` was built from in order (`ListWorkspaces` with
/// `changed_file_counts`, issue #114): one job per repository on its read
/// thread, at its head, counting what `@` changes against its parents as
/// `GetStatus` does; repositories at once. Best effort: `ws list` is a
/// recovery tool, so a repository whose job fails, or takes longer than
/// [`COUNT_TIMEOUT`] (behind a long snapshot or GC on its thread), leaves
/// its counts unset and the list stands.
async fn count_changed_files(
    state: &AppState,
    rows: &[(WorkspaceRecord, Option<LeaseRecord>)],
    workspaces: &mut [Workspace],
) {
    let mut by_repo: BTreeMap<RepoId, Vec<usize>> = BTreeMap::new();
    for (index, (record, _)) in rows.iter().enumerate() {
        if matches!(
            record.state,
            WorkspaceState::Active | WorkspaceState::Releasing
        ) {
            by_repo.entry(record.repo_id).or_default().push(index);
        }
    }
    let jobs = by_repo.into_iter().map(|(repo, indexes)| async move {
        let names = indexes
            .iter()
            .map(|&index| rows[index].0.id.jj_workspace_name())
            .collect();
        let counted =
            tokio::time::timeout(COUNT_TIMEOUT, changed_counts(state, repo, names, None)).await;
        (repo, indexes, counted)
    });
    for (repo, indexes, counted) in futures::future::join_all(jobs).await {
        match counted {
            Ok(Ok(counts)) => {
                for (index, count) in indexes.into_iter().zip(counts) {
                    workspaces[index].changed_file_count = count;
                }
            }
            Ok(Err(error)) => tracing::warn!(
                repo = %repo,
                error = %error.message(),
                "could not count workspaces' changed files"
            ),
            Err(_) => tracing::warn!(
                repo = %repo,
                timeout_ms = COUNT_TIMEOUT.as_millis(),
                "counting workspaces' changed files timed out; listed without counts"
            ),
        }
    }
}

/// How many paths `@` changes in each of the jj workspaces `names` of
/// `repo`, in order, at head or at the operation `at` names (R2.12); `None`
/// for one whose `@` does not resolve.
async fn changed_counts(
    state: &AppState,
    repo: RepoId,
    names: Vec<String>,
    at: Option<String>,
) -> Result<Vec<Option<u32>>, Status> {
    let repo_id = repo;
    let reads = Arc::clone(&state.reads);
    let root = state.db.repository(repo).map_err(status)?.root;
    let handle = state.manager.handle(repo).map_err(status)?;
    let head = state.reads.current(&handle).await.map_err(status)?;
    let user_email = state.settings.user_email().to_owned();
    // Dropped with this future, at the timeout too, which stops the diffs.
    let (stop, _guard) = state.reads.stop();
    state
        .reads
        .run(&handle, move || async move {
            let repo = &match at {
                Some(text) => reads.at(repo_id, &head, &text).await.map_err(status)?,
                None => head,
            };
            let options = DiffOptions {
                format: DiffFormat::Summary,
                context: 0,
                max_lines: 0,
                max_hunk_bytes: 0,
                max_file_bytes: 0,
                paths: Vec::new(),
                after: None,
                // `total` counts every file whatever the limit.
                limit: 1,
                pairs: false,
            };
            let mut counts = Vec::with_capacity(names.len());
            for name in &names {
                let scope = RevsetScope {
                    workspace: Some(name),
                    user_email: &user_email,
                    root: &root,
                };
                let count = match revset::resolve_revision(repo, "@", &scope).await {
                    Ok(wc) => {
                        diff::diff_page(repo, &DiffBase::Parents, &wc.commit_id, &options, &|| {
                            stop.requested()
                        })
                        .await
                        .ok()
                        .map(|page| u32::try_from(page.total).unwrap_or(u32::MAX))
                    }
                    Err(_) => None,
                };
                counts.push(count);
            }
            Ok(counts)
        })
        .await
        .map_err(status)?
}

fn record_ids(repo: impl std::fmt::Display, workspace: impl std::fmt::Display) {
    let span = tracing::Span::current();
    span.record("repo", tracing::field::display(repo));
    span.record("workspace_id", tracing::field::display(workspace));
}

fn non_empty(text: String) -> Option<String> {
    if text.is_empty() { None } else { Some(text) }
}

#[tonic::async_trait]
impl WorkspaceService for WorkspacesService {
    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn create_workspace(
        &self,
        request: Request<CreateWorkspaceRequest>,
    ) -> Result<Response<CreateWorkspaceResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let request = request.into_inner();
        let manager = &self.state.manager;
        let repo = manager.resolve_repo(&request.repo).map_err(status)?;
        let missing_stores =
            missing_stores(known("missing_stores", request.missing_stores).map_err(status)?);
        let backend = backend_choice(known("backend", request.backend).map_err(status)?);
        let start = match request.start {
            Some(create_workspace_request::Start::Change(text)) => Start::Change(text),
            Some(create_workspace_request::Start::NewOn(text)) => Start::NewOn(text),
            None => Start::Default,
        };
        let created = manager
            .create(CreateSpec {
                repo: repo.id,
                start,
                owner: request.owner,
                purpose: request.purpose,
                backend,
                path: non_empty(request.path).map(PathBuf::from),
                lease: request_duration(request.lease_length.as_ref()),
                missing_stores,
                skip_store: None,
                agent: agent.clone(),
            })
            .await
            .map_err(status)?;
        record_ids(repo.id, created.record.id);
        let workspace_id = created.record.id.as_hex();
        let operation = written_operation(
            &self.state,
            repo.id,
            Some(&created.op_id),
            Written {
                kind: OperationKind::CreateWorkspace,
                snapshot: None,
                workspace: Some(&workspace_id),
                agent: agent.as_deref().unwrap_or(&created.record.owner),
            },
        )
        .await?;
        Ok(Response::new(CreateWorkspaceResponse {
            workspace: Some(Workspace::from(WorkspaceView {
                record: &created.record,
                lease: self.lease_message(Some(&created.lease), Timestamp::now()),
                local_snapshot_count: 0,
                stores: &created.stores,
                watch_pending: manager.scheduler().watch_pending(created.record.id),
                loaded: true,
            })),
            operation,
            shared_change: created.shared_change,
            base_was_cached: created.base_was_cached,
            timings: Some(v1::CreateTimings::from(&created.timings)),
            start: created.default_start.unwrap_or_default(),
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn list_workspaces(
        &self,
        request: Request<ListWorkspacesRequest>,
    ) -> Result<Response<ListWorkspacesResponse>, Status> {
        let request = request.into_inner();
        if !request.at_operation.trim().is_empty() {
            return self.list_workspaces_at(request).await;
        }
        let page = PageRequest::new(
            "/reef.api.v1.WorkspaceService/ListWorkspaces",
            request.page_size,
            &request.page_token,
            &ListWorkspacesRequest {
                page_size: 0,
                page_token: String::new(),
                changed_file_counts: false,
                ..request.clone()
            },
        )
        .map_err(status)?;
        let manager = &self.state.manager;
        let repo = if request.repo.trim().is_empty() {
            None
        } else {
            let record = manager.resolve_repo(&request.repo).map_err(status)?;
            tracing::Span::current().record("repo", tracing::field::display(record.id));
            Some(record.id)
        };
        let states: Vec<WorkspaceState> = request
            .states
            .iter()
            .map(|state| known("states", *state))
            .collect::<Result<Vec<_>, _>>()
            .map_err(status)?
            .into_iter()
            .filter_map(state_of)
            .collect();
        let owner = non_empty(request.owner);
        let (rows, next_page_token) = page.page(
            manager
                .list(repo, &states, owner.as_deref())
                .map_err(status)?,
            |(record, _)| newest_first(record.created_at.0, &record.id.as_hex()),
        );
        let mut workspaces = listed(&self.state, repo, &rows)?;
        if request.changed_file_counts {
            count_changed_files(&self.state, &rows, &mut workspaces).await;
        }
        Ok(Response::new(ListWorkspacesResponse {
            workspaces,
            next_page_token,
            at_operation: String::new(),
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn get_workspace(
        &self,
        request: Request<GetWorkspaceRequest>,
    ) -> Result<Response<GetWorkspaceResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let request = request.into_inner();
        let manager = &self.state.manager;
        let record = manager
            .resolve_workspace(&request.repo, &request.workspace)
            .map_err(status)?;
        record_ids(record.repo_id, record.id);
        if !request.at_operation.trim().is_empty() {
            if request.snapshot_first {
                return Err(status(ReplayError::SnapshotFirst));
            }
            return self.get_workspace_at(record, &request.at_operation).await;
        }
        let workspace_id = record.id.as_hex();
        let inspected = manager
            .inspect(
                record.id,
                request.snapshot_first,
                !request.skip_disk_usage,
                agent.clone(),
            )
            .await
            .map_err(status)?;
        let snapshot = written_operation(
            &self.state,
            record.repo_id,
            inspected.op_id.as_deref(),
            Written {
                kind: OperationKind::Snapshot,
                snapshot: Some(SnapshotKind::Explicit),
                workspace: Some(&workspace_id),
                agent: agent.as_deref().unwrap_or(&record.owner),
            },
        )
        .await?;
        Ok(Response::new(GetWorkspaceResponse {
            workspace: Some(Workspace::from(WorkspaceView {
                record: &inspected.record,
                lease: self.lease_message(inspected.lease.as_ref(), Timestamp::now()),
                local_snapshot_count: inspected.local_snapshot_count,
                stores: &inspected.stores,
                watch_pending: manager.scheduler().watch_pending(inspected.record.id),
                loaded: manager.entry(inspected.record.id).is_some(),
            })),
            disk_usage: inspected.disk_usage.as_ref().map(Into::into),
            other_workspaces_on_change: inspected
                .other_workspaces_on_change
                .iter()
                .map(crate::model::WorkspaceId::as_hex)
                .collect(),
            snapshot,
            snapshot_count: inspected.snapshot_count,
            recent_snapshots: inspected
                .recent_snapshots
                .iter()
                .map(v1::SnapshotEntry::from)
                .collect(),
            at_operation: String::new(),
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn release_workspace(
        &self,
        request: Request<ReleaseWorkspaceRequest>,
    ) -> Result<Response<ReleaseWorkspaceResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let request = request.into_inner();
        let manager = &self.state.manager;
        let record = manager
            .resolve_workspace(&request.repo, &request.workspace)
            .map_err(status)?;
        record_ids(record.repo_id, record.id);
        let workspace_id = record.id.as_hex();
        let released = manager
            .leases()
            .reclaim(
                record.id,
                ReclaimReason::Released,
                ReleaseSpec {
                    force: request.force,
                    final_snapshot: !request.skip_final_snapshot,
                    detail: None,
                    agent: agent.clone(),
                },
            )
            .await
            .map_err(status)?;
        let operation = written_operation(
            &self.state,
            record.repo_id,
            released.release_op_id.as_deref(),
            Written {
                kind: OperationKind::ReleaseWorkspace,
                snapshot: None,
                workspace: Some(&workspace_id),
                agent: agent.as_deref().unwrap_or(&record.owner),
            },
        )
        .await?;
        let final_snapshot = written_operation(
            &self.state,
            record.repo_id,
            released.final_op_id.as_deref(),
            Written {
                kind: OperationKind::Snapshot,
                snapshot: Some(SnapshotKind::Final),
                workspace: Some(&workspace_id),
                agent: agent.as_deref().unwrap_or(&record.owner),
            },
        )
        .await?;
        Ok(Response::new(ReleaseWorkspaceResponse {
            workspace_id: record.id.as_hex(),
            operation,
            final_snapshot,
            snapshot_taken: released.snapshot_taken,
            freed_bytes: released.freed_bytes.unwrap_or(0),
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn snapshot_workspace(
        &self,
        request: Request<SnapshotWorkspaceRequest>,
    ) -> Result<Response<SnapshotWorkspaceResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let request = request.into_inner();
        let manager = &self.state.manager;
        let record = manager
            .resolve_workspace(&request.repo, &request.workspace)
            .map_err(status)?;
        record_ids(record.repo_id, record.id);
        let workspace_id = record.id.as_hex();
        let snapshot = manager
            .snapshot(
                record.id,
                SnapshotKind::Explicit,
                &request.description,
                agent.clone(),
            )
            .await
            .map_err(status)?;
        let op_id = snapshot.op_id.to_string();
        let operation = written_operation(
            &self.state,
            record.repo_id,
            snapshot.changed.then_some(op_id.as_str()),
            Written {
                kind: OperationKind::Snapshot,
                snapshot: Some(SnapshotKind::Explicit),
                workspace: Some(&workspace_id),
                agent: agent.as_deref().unwrap_or(&record.owner),
            },
        )
        .await?;
        Ok(Response::new(SnapshotWorkspaceResponse {
            operation,
            changed: snapshot.changed,
            commit_id: snapshot.commit_id.to_string(),
            scan_ms: snapshot.scan_ms,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn renew_lease(
        &self,
        request: Request<RenewLeaseRequest>,
    ) -> Result<Response<RenewLeaseResponse>, Status> {
        let request = request.into_inner();
        let manager = &self.state.manager;
        let record = manager
            .resolve_workspace(&request.repo, &request.workspace)
            .map_err(status)?;
        record_ids(record.repo_id, record.id);
        let lease_id: LeaseId = request.lease_id.trim().parse().map_err(status)?;
        // A quiesce holds the database still (R1.18); its resume extends
        // every lease, so the holder loses nothing by waiting.
        self.state.quiesce.check().map_err(status)?;
        let renewed = manager
            .leases()
            .heartbeat(record.id, lease_id)
            .map_err(status)?;
        Ok(Response::new(RenewLeaseResponse {
            lease: self.lease_message(Some(&renewed), Timestamp::now()),
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn move_workspace(
        &self,
        request: Request<MoveWorkspaceRequest>,
    ) -> Result<Response<MoveWorkspaceResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let request = request.into_inner();
        let manager = &self.state.manager;
        let record = manager
            .resolve_workspace(&request.repo, &request.workspace)
            .map_err(status)?;
        record_ids(record.repo_id, record.id);
        let workspace_id = record.id.as_hex();
        let missing_stores =
            missing_stores(known("missing_stores", request.missing_stores).map_err(status)?);
        let start = match request.target {
            Some(move_workspace_request::Target::Change(text)) => Start::Change(text),
            Some(move_workspace_request::Target::NewOn(text)) => Start::NewOn(text),
            None => Start::Default,
        };
        let moved = manager
            .move_to(record.id, start, missing_stores, agent.clone())
            .await
            .map_err(status)?;
        let (_, local_snapshot_count) = index::counts(&self.state.db, record.id).map_err(status)?;
        let operation = written_operation(
            &self.state,
            record.repo_id,
            moved.op_id.as_deref(),
            Written {
                kind: OperationKind::MoveWorkspace,
                snapshot: None,
                workspace: Some(&workspace_id),
                agent: agent.as_deref().unwrap_or(&record.owner),
            },
        )
        .await?;
        let final_snapshot = written_operation(
            &self.state,
            record.repo_id,
            moved.final_op_id.as_deref(),
            Written {
                kind: OperationKind::Snapshot,
                snapshot: Some(SnapshotKind::Move),
                workspace: Some(&workspace_id),
                agent: agent.as_deref().unwrap_or(&record.owner),
            },
        )
        .await?;
        Ok(Response::new(MoveWorkspaceResponse {
            workspace: Some(Workspace::from(WorkspaceView {
                record: &moved.record,
                lease: self.lease_message(moved.lease.as_ref(), Timestamp::now()),
                local_snapshot_count,
                stores: &moved.stores,
                watch_pending: manager.scheduler().watch_pending(moved.record.id),
                loaded: true,
            })),
            operation,
            final_snapshot,
            files_updated: moved.checkout.updated,
            files_added: moved.checkout.added,
            files_removed: moved.checkout.removed,
            tree_replaced: moved.tree_replaced,
        }))
    }
}
