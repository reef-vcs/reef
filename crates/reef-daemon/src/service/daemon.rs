//! `reef.api.v1.DaemonService`: the daemon's own status.

use std::sync::Arc;

use reef_api::v1::daemon_service_server::DaemonService;
use reef_api::v1::{BackendProbe, GetDaemonStatusRequest, GetDaemonStatusResponse};
use tonic::{Request, Response, Status};

use super::convert::backend;
use crate::backend::{Availability, probe};
use crate::daemon::AppState;
use crate::db::{META_DAEMON_INSTANCE_ID, SCHEMA_VERSION};
use crate::error::status;
use crate::model::WorkspaceState;

/// Serves `DaemonService`.
#[derive(Debug)]
pub struct DaemonStatusService {
    state: Arc<AppState>,
}

impl DaemonStatusService {
    /// A service answering from `state`.
    pub fn new(state: Arc<AppState>) -> DaemonStatusService {
        DaemonStatusService { state }
    }
}

/// The daemon's status, probing the backends unless `skip_backends`: what
/// both the v1 `GetDaemonStatus` and the frozen v0 `Daemon/Status` report.
pub(crate) async fn daemon_status(
    state: &AppState,
    skip_backends: bool,
) -> Result<GetDaemonStatusResponse, Status> {
    let paths = &state.config.paths;
    let repositories = state.db.repositories().map_err(status)?.len();
    let active_workspaces = state
        .db
        .workspaces(None, &[WorkspaceState::Active])
        .map_err(status)?
        .len();
    let instance_id = state
        .db
        .meta(META_DAEMON_INSTANCE_ID)
        .map_err(status)?
        .unwrap_or_default();
    let backends = if skip_backends {
        Vec::new()
    } else {
        // Workspace roots default to `<state>/repos/<repo-id>/ws`, beside
        // the base caches, so this reports what a default workspace gets.
        // Probing touches the filesystem, so it runs off the async workers.
        let repos = paths.repos.clone();
        let overlay = state.config.file.workspace.overlay();
        let probes = tokio::task::spawn_blocking(move || probe(&repos, None, &overlay))
            .await
            .map_err(|e| Status::internal(format!("backend probe failed: {e}")))?;
        probes
            .into_iter()
            .map(|probe| {
                let (available, reason) = match probe.availability {
                    Availability::Available => (true, String::new()),
                    Availability::Unavailable(reason) => (false, reason),
                };
                BackendProbe {
                    backend: backend(probe.kind).into(),
                    available,
                    reason,
                    filesystem: probe.filesystem,
                }
            })
            .collect()
    };
    Ok(GetDaemonStatusResponse {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        api_revision: reef_api::API_REVISION,
        instance_id,
        pid: std::process::id(),
        uptime_ms: u64::try_from(state.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        state_dir: paths.state_dir.display().to_string(),
        socket: paths.socket.display().to_string(),
        schema_version: SCHEMA_VERSION,
        repository_count: saturate(repositories),
        active_workspace_count: saturate(active_workspaces),
        backends,
    })
}

#[tonic::async_trait]
impl DaemonService for DaemonStatusService {
    async fn get_daemon_status(
        &self,
        request: Request<GetDaemonStatusRequest>,
    ) -> Result<Response<GetDaemonStatusResponse>, Status> {
        let skip_backends = request.into_inner().skip_backends;
        Ok(Response::new(
            daemon_status(&self.state, skip_backends).await?,
        ))
    }
}

fn saturate(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}
