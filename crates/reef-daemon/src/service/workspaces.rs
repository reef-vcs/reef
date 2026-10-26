//! `reef.api.v1.WorkspaceService`: create, list, get, release, snapshot,
//! renew, move.

use std::path::PathBuf;
use std::sync::Arc;

use reef_api::v1::workspace_service_server::WorkspaceService;
use reef_api::v1::{
    self, CreateWorkspaceRequest, CreateWorkspaceResponse, GetWorkspaceRequest,
    GetWorkspaceResponse, ListWorkspacesRequest, ListWorkspacesResponse, MoveWorkspaceRequest,
    MoveWorkspaceResponse, OperationKind, ReleaseWorkspaceRequest, ReleaseWorkspaceResponse,
    RenewLeaseRequest, RenewLeaseResponse, SnapshotWorkspaceRequest, SnapshotWorkspaceResponse,
    Workspace, create_workspace_request, move_workspace_request,
};
use tonic::{Request, Response, Status};

use super::convert::{
    WorkspaceView, Written, backend_choice, known, lease, missing_stores, request_duration,
    state_of,
};
use super::page::{PageRequest, newest_first};
use super::written_operation;
use crate::daemon::AppState;
use crate::error::status;
use crate::model::{
    LeaseId, LeaseRecord, ReclaimReason, Timestamp, WorkspaceRecord, WorkspaceState,
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

    /// `record` as a message, its lease state computed at `now`.
    fn lease_message(&self, record: Option<&LeaseRecord>, now: Timestamp) -> Option<v1::Lease> {
        record.map(|record| lease(record, self.state.manager.leases(), now))
    }
}

/// Workspace rows and their leases as messages, as `ListWorkspaces` lists
/// them; `repo` scopes the local snapshot counts read.
pub(super) fn listed(
    state: &AppState,
    repo: Option<crate::model::RepoId>,
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
        let page = PageRequest::new(
            "/reef.api.v1.WorkspaceService/ListWorkspaces",
            request.page_size,
            &request.page_token,
            &ListWorkspacesRequest {
                page_size: 0,
                page_token: String::new(),
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
        let workspaces = listed(&self.state, repo, &rows)?;
        Ok(Response::new(ListWorkspacesResponse {
            workspaces,
            next_page_token,
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
