//! `reef.daemon.v1.Workspaces`: create, list, inspect, release, snapshot,
//! heartbeat, move; status is R2.02's.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reef_api::page::{self, PageError};
use reef_lib::error::{Classify, ErrorKind};
use tonic::{Request, Response, Status};

use super::convert::{WorkspaceView, lease, proto_duration, timestamp};
use super::stubs::stub;
use crate::backend::BackendChoice;
use crate::daemon::AppState;
use crate::db::WorkspaceFilter;
use crate::error::status;
use crate::model::{
    LeaseId, LeaseRecord, ReclaimReason, RepoId, Timestamp, WorkspaceId, WorkspaceRecord,
    WorkspaceState,
};
use crate::snapshot::{SnapshotKind, index};
use crate::workspace::{CreateSpec, MissingStores, ReleaseSpec, Start};
use reef_api::proto::v1::workspaces_server::Workspaces;
use reef_api::proto::v1::{
    self, CreateRequest, CreateResponse, HeartbeatRequest, HeartbeatResponse, InspectRequest,
    InspectResponse, ListWorkspacesRequest, ListWorkspacesResponse, MoveRequest, MoveResponse,
    PageInfo, ReleaseRequest, ReleaseResponse, SnapshotRequest, SnapshotResponse, Workspace,
    WorkspaceStatusRequest, WorkspaceStatusResponse, create_request, move_request,
};

/// A request field that does not parse.
#[derive(Debug, thiserror::Error)]
#[error("unknown workspace state {0:?}; expected active, releasing, released, or reclaimed")]
pub struct UnknownState(pub String);

impl Classify for UnknownState {
    fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidArgument
    }
}

/// Serves `Workspaces`.
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

fn record_ids(repo: impl std::fmt::Display, workspace: impl std::fmt::Display) {
    let span = tracing::Span::current();
    span.record("repo", tracing::field::display(repo));
    span.record("workspace_id", tracing::field::display(workspace));
}

fn non_empty(text: String) -> Option<String> {
    if text.is_empty() { None } else { Some(text) }
}

fn optional_millis(ms: u64) -> Option<Duration> {
    (ms != 0).then(|| Duration::from_millis(ms))
}

/// The hash binding a `Workspaces.List` page token to its filters.
fn list_filter_hash(repo: Option<RepoId>, states: &[WorkspaceState], owner: Option<&str>) -> u64 {
    let mut states: Vec<&str> = states.iter().map(|state| state.as_str()).collect();
    states.sort_unstable();
    states.dedup();
    let repo = repo.map(|id| id.as_hex()).unwrap_or_default();
    page::filter_hash(
        ["workspaces", &repo, owner.unwrap_or_default()]
            .into_iter()
            .chain(states),
    )
}

/// A `Workspaces.List` cursor: the last row's `<created_at>:<id>`.
fn cursor_of(record: &WorkspaceRecord) -> String {
    format!("{}:{}", record.created_at.0, record.id.as_hex())
}

fn parse_cursor(cursor: &str) -> Result<(Timestamp, WorkspaceId), PageError> {
    let (at, id) = cursor.split_once(':').ok_or(PageError::Malformed)?;
    let at = at.parse().map_err(|_| PageError::Malformed)?;
    let id = id.parse().map_err(|_| PageError::Malformed)?;
    Ok((Timestamp(at), id))
}

fn missing_stores(value: create_request::MissingStores) -> MissingStores {
    match value {
        create_request::MissingStores::Fail => MissingStores::Fail,
        create_request::MissingStores::Skip => MissingStores::Skip,
    }
}

#[tonic::async_trait]
impl Workspaces for WorkspacesService {
    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn create(
        &self,
        request: Request<CreateRequest>,
    ) -> Result<Response<CreateResponse>, Status> {
        let request = request.into_inner();
        let manager = &self.state.manager;
        let repo = manager.resolve_repo(&request.repo).map_err(status)?;
        let missing_stores = missing_stores(request.missing_stores());
        let start = match request.start {
            Some(create_request::Start::Change(text)) => Start::Change(text),
            Some(create_request::Start::NewOn(text)) => Start::NewOn(text),
            None => Start::Default,
        };
        let backend = non_empty(request.backend)
            .map(|text| text.parse::<BackendChoice>())
            .transpose()
            .map_err(status)?;
        let created = manager
            .create(CreateSpec {
                repo: repo.id,
                start,
                owner: request.owner,
                purpose: request.purpose,
                backend,
                path: non_empty(request.path).map(PathBuf::from),
                lease: optional_millis(request.lease_ms),
                missing_stores,
                skip_store: None,
            })
            .await
            .map_err(status)?;
        record_ids(repo.id, created.record.id);
        Ok(Response::new(CreateResponse {
            workspace: Some(Workspace::from(WorkspaceView {
                record: &created.record,
                lease: self.lease_message(Some(&created.lease), Timestamp::now()),
                local_snapshot_count: 0,
                stores: &created.stores,
                watch_pending: manager.scheduler().watch_pending(created.record.id),
            })),
            op_id: created.op_id,
            shared_change: created.shared_change,
            materialize_ms: created
                .timings
                .base_ms
                .saturating_add(created.timings.materialize_ms),
            base_was_cached: created.base_was_cached,
            timings: Some(v1::CreateTimings::from(&created.timings)),
            start: created.default_start.unwrap_or_default(),
            // R2.09 assigns event sequences.
            seq: 0,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn list(
        &self,
        request: Request<ListWorkspacesRequest>,
    ) -> Result<Response<ListWorkspacesResponse>, Status> {
        let request = request.into_inner();
        let manager = &self.state.manager;
        let repo = if request.repo.trim().is_empty() {
            None
        } else {
            let record = manager.resolve_repo(&request.repo).map_err(status)?;
            tracing::Span::current().record("repo", tracing::field::display(record.id));
            Some(record.id)
        };
        let states = request
            .states
            .iter()
            .map(|text| WorkspaceState::parse(text).ok_or_else(|| UnknownState(text.clone())))
            .collect::<Result<Vec<_>, _>>()
            .map_err(status)?;
        let owner = non_empty(request.owner);
        let page = request.page.unwrap_or_default();
        let limit = page::clamp_page_size(page.page_size);
        let hash = list_filter_hash(repo, &states, owner.as_deref());
        let after = page::decode_token(&page.page_token, hash)
            .and_then(|cursor| cursor.as_deref().map(parse_cursor).transpose())
            .map_err(status)?;
        let filter = WorkspaceFilter {
            repo,
            states: &states,
            owner: owner.as_deref(),
        };
        // One more than a page, to know whether another follows.
        let mut rows = manager
            .list_page(&filter, after, limit.saturating_add(1))
            .map_err(status)?;
        let next_page_token = if rows.len() > limit {
            rows.truncate(limit);
            rows.last()
                .map(|(last, _)| page::encode_token(&cursor_of(last), hash))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let now = Timestamp::now();
        let local_counts = index::local_counts(&self.state.db, repo).map_err(status)?;
        let workspaces = rows
            .iter()
            .map(|(record, lease)| {
                let stores = manager
                    .entry(record.id)
                    .filter(|_| record.state == WorkspaceState::Active)
                    .map(|entry| entry.stores())
                    .unwrap_or_default();
                Workspace::from(WorkspaceView {
                    record,
                    lease: self.lease_message(lease.as_ref(), now),
                    local_snapshot_count: local_counts.get(&record.id).copied().unwrap_or(0),
                    stores: &stores,
                    watch_pending: manager.scheduler().watch_pending(record.id),
                })
            })
            .collect();
        Ok(Response::new(ListWorkspacesResponse {
            workspaces,
            page: Some(PageInfo {
                next_page_token,
                page_size: u32::try_from(limit).unwrap_or(u32::MAX),
                restarted: false,
            }),
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn inspect(
        &self,
        request: Request<InspectRequest>,
    ) -> Result<Response<InspectResponse>, Status> {
        let request = request.into_inner();
        let manager = &self.state.manager;
        let record = manager
            .resolve_workspace(&request.repo, &request.workspace)
            .map_err(status)?;
        record_ids(record.repo_id, record.id);
        let inspected = manager
            .inspect(record.id, request.snapshot_first, !request.skip_disk_usage)
            .await
            .map_err(status)?;
        Ok(Response::new(InspectResponse {
            workspace: Some(Workspace::from(WorkspaceView {
                record: &inspected.record,
                lease: self.lease_message(inspected.lease.as_ref(), Timestamp::now()),
                local_snapshot_count: inspected.local_snapshot_count,
                stores: &inspected.stores,
                watch_pending: manager.scheduler().watch_pending(inspected.record.id),
            })),
            disk_usage: inspected.disk_usage.as_ref().map(Into::into),
            other_workspaces_on_change: inspected
                .other_workspaces_on_change
                .iter()
                .map(WorkspaceId::as_hex)
                .collect(),
            op_id: inspected.op_id.unwrap_or_default(),
            snapshot_count: inspected.snapshot_count,
            local_snapshot_count: inspected.local_snapshot_count,
            recent_snapshots: inspected
                .recent_snapshots
                .iter()
                .map(v1::SnapshotEntry::from)
                .collect(),
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn release(
        &self,
        request: Request<ReleaseRequest>,
    ) -> Result<Response<ReleaseResponse>, Status> {
        let request = request.into_inner();
        let manager = &self.state.manager;
        let record = manager
            .resolve_workspace(&request.repo, &request.workspace)
            .map_err(status)?;
        record_ids(record.repo_id, record.id);
        let released = manager
            .leases()
            .reclaim(
                record.id,
                ReclaimReason::Released,
                ReleaseSpec {
                    force: request.force,
                    final_snapshot: !request.skip_final_snapshot,
                    detail: None,
                },
            )
            .await
            .map_err(status)?;
        Ok(Response::new(ReleaseResponse {
            final_op_id: released.final_op_id.unwrap_or_default(),
            snapshot_taken: released.snapshot_taken,
            freed_bytes: released.freed_bytes.unwrap_or(0),
            workspace_id: record.id.as_hex(),
            seq: 0,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn snapshot(
        &self,
        request: Request<SnapshotRequest>,
    ) -> Result<Response<SnapshotResponse>, Status> {
        let request = request.into_inner();
        let manager = &self.state.manager;
        let record = manager
            .resolve_workspace(&request.repo, &request.workspace)
            .map_err(status)?;
        record_ids(record.repo_id, record.id);
        let snapshot = manager
            .snapshot(record.id, SnapshotKind::Explicit, &request.description)
            .await
            .map_err(status)?;
        Ok(Response::new(SnapshotResponse {
            op_id: if snapshot.changed {
                snapshot.op_id.to_string()
            } else {
                String::new()
            },
            changed: snapshot.changed,
            commit_id: snapshot.commit_id.to_string(),
            scan_ms: snapshot.scan_ms,
            seq: 0,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
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
        let leases = manager.leases();
        let renewed = leases.heartbeat(record.id, lease_id).map_err(status)?;
        Ok(Response::new(HeartbeatResponse {
            lease_id: renewed.id.as_hex(),
            expires_at: Some(timestamp(renewed.expires_at)),
            heartbeat_every: Some(proto_duration(leases.heartbeat_every(renewed.length))),
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn r#move(
        &self,
        request: Request<MoveRequest>,
    ) -> Result<Response<MoveResponse>, Status> {
        let request = request.into_inner();
        let manager = &self.state.manager;
        let record = manager
            .resolve_workspace(&request.repo, &request.workspace)
            .map_err(status)?;
        record_ids(record.repo_id, record.id);
        let missing_stores = missing_stores(request.missing_stores());
        let start = match request.target {
            Some(move_request::Target::Change(text)) => Start::Change(text),
            Some(move_request::Target::NewOn(text)) => Start::NewOn(text),
            None => Start::Default,
        };
        let moved = manager
            .move_to(record.id, start, missing_stores)
            .await
            .map_err(status)?;
        let (_, local_snapshot_count) = index::counts(&self.state.db, record.id).map_err(status)?;
        Ok(Response::new(MoveResponse {
            workspace: Some(Workspace::from(WorkspaceView {
                record: &moved.record,
                lease: self.lease_message(moved.lease.as_ref(), Timestamp::now()),
                local_snapshot_count,
                stores: &moved.stores,
                watch_pending: manager.scheduler().watch_pending(moved.record.id),
            })),
            op_id: moved.op_id.unwrap_or_default(),
            final_op_id: moved.final_op_id.unwrap_or_default(),
            files_updated: moved.checkout.updated,
            files_added: moved.checkout.added,
            files_removed: moved.checkout.removed,
            tree_replaced: moved.tree_replaced,
            seq: 0,
        }))
    }

    async fn status(
        &self,
        _: Request<WorkspaceStatusRequest>,
    ) -> Result<Response<WorkspaceStatusResponse>, Status> {
        Err(stub("reef.daemon.v1.Workspaces/Status"))
    }
}
