//! `reef.daemon.v0.Workspaces`: create, list, inspect, release, snapshot,
//! heartbeat.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reef_lib::error::{Classify, ErrorKind};
use tonic::{Request, Response, Status};

use super::convert::{WorkspaceView, lease, proto_duration, timestamp};
use crate::backend::BackendChoice;
use crate::daemon::AppState;
use crate::error::status;
use crate::model::{LeaseId, LeaseRecord, ReclaimReason, Timestamp, WorkspaceState};
use crate::proto::v0::workspaces_server::Workspaces;
use crate::proto::v0::{
    self, CreateRequest, CreateResponse, HeartbeatRequest, HeartbeatResponse, InspectRequest,
    InspectResponse, ListWorkspacesRequest, ListWorkspacesResponse, ReleaseRequest,
    ReleaseResponse, SnapshotRequest, SnapshotResponse, Workspace, create_request,
};
use crate::snapshot::SnapshotKind;
use crate::workspace::{CreateSpec, ReleaseSpec, Start};

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
    fn lease_message(&self, record: Option<&LeaseRecord>, now: Timestamp) -> Option<v0::Lease> {
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
            })
            .await
            .map_err(status)?;
        record_ids(repo.id, created.record.id);
        Ok(Response::new(CreateResponse {
            workspace: Some(Workspace::from(WorkspaceView {
                record: &created.record,
                lease: self.lease_message(Some(&created.lease), Timestamp::now()),
            })),
            op_id: created.op_id,
            shared_change: created.shared_change,
            materialize_ms: u64::try_from(created.materialize.as_millis()).unwrap_or(u64::MAX),
            base_was_cached: created.base_was_cached,
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
        let now = Timestamp::now();
        let workspaces = manager
            .list(repo, &states, owner.as_deref())
            .map_err(status)?
            .iter()
            .map(|(record, lease)| {
                Workspace::from(WorkspaceView {
                    record,
                    lease: self.lease_message(lease.as_ref(), now),
                })
            })
            .collect();
        Ok(Response::new(ListWorkspacesResponse { workspaces }))
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
            .inspect(record.id, request.snapshot_first)
            .await
            .map_err(status)?;
        Ok(Response::new(InspectResponse {
            workspace: Some(Workspace::from(WorkspaceView {
                record: &inspected.record,
                lease: self.lease_message(inspected.lease.as_ref(), Timestamp::now()),
            })),
            disk_usage: inspected.disk_usage.as_ref().map(Into::into),
            other_workspaces_on_change: inspected
                .other_workspaces_on_change
                .iter()
                .map(crate::model::WorkspaceId::as_hex)
                .collect(),
            op_id: inspected.op_id.unwrap_or_default(),
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
                },
            )
            .await
            .map_err(status)?;
        Ok(Response::new(ReleaseResponse {
            final_op_id: released.final_op_id.unwrap_or_default(),
            snapshot_taken: released.snapshot_taken,
            freed_bytes: released.freed_bytes.unwrap_or(0),
            workspace_id: record.id.as_hex(),
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
            .resolve_workspace(&request.repo, &request.workspace_id)
            .map_err(status)?;
        record_ids(record.repo_id, record.id);
        let lease_id: LeaseId = request.lease_id.trim().parse().map_err(status)?;
        let leases = manager.leases();
        let renewed = leases.heartbeat(record.id, lease_id).map_err(status)?;
        Ok(Response::new(HeartbeatResponse {
            lease_id: renewed.id.as_hex(),
            expires_at: Some(timestamp(renewed.expires_at)),
            heartbeat_every: Some(proto_duration(leases.heartbeat_every(renewed.length))),
        }))
    }
}
