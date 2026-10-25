//! `reef.daemon.v1.Daemon`: the daemon's own status.

use std::sync::Arc;

use tonic::{Request, Response, Status};

use crate::backend::{Availability, probe};
use crate::daemon::AppState;
use crate::db::SCHEMA_VERSION;
use crate::error::status;
use crate::model::WorkspaceState;
use reef_api::proto::v1::daemon_server::Daemon;
use reef_api::proto::v1::{BackendProbe, DaemonStatusRequest, DaemonStatusResponse};

/// Serves `Daemon`.
#[derive(Debug)]
pub struct DaemonService {
    state: Arc<AppState>,
}

impl DaemonService {
    /// A service answering from `state`.
    pub fn new(state: Arc<AppState>) -> DaemonService {
        DaemonService { state }
    }
}

#[tonic::async_trait]
impl Daemon for DaemonService {
    async fn status(
        &self,
        _request: Request<DaemonStatusRequest>,
    ) -> Result<Response<DaemonStatusResponse>, Status> {
        let state = &self.state;
        let paths = &state.config.paths;
        let repositories = state.db.repositories().map_err(status)?.len();
        let active_workspaces = state
            .db
            .workspaces(None, &[WorkspaceState::Active])
            .map_err(status)?
            .len();
        // Workspace roots default to `<state>/repos/<repo-id>/ws`, beside
        // the base caches, so this reports what a default workspace gets.
        // Probing touches the filesystem, so it runs off the async workers.
        let repos = paths.repos.clone();
        let overlay = state.config.file.workspace.overlay();
        let probes = tokio::task::spawn_blocking(move || probe(&repos, None, &overlay))
            .await
            .map_err(|e| Status::internal(format!("backend probe failed: {e}")))?;
        let backends = probes
            .into_iter()
            .map(|probe| {
                let (available, reason) = match probe.availability {
                    Availability::Available => (true, String::new()),
                    Availability::Unavailable(reason) => (false, reason),
                };
                BackendProbe {
                    kind: probe.kind.as_str().to_owned(),
                    available,
                    reason,
                    filesystem: probe.filesystem,
                }
            })
            .collect();
        Ok(Response::new(DaemonStatusResponse {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            pid: std::process::id(),
            uptime_ms: u64::try_from(state.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            state_dir: paths.state_dir.display().to_string(),
            socket: paths.socket.display().to_string(),
            schema_version: SCHEMA_VERSION,
            repositories: saturate(repositories),
            backends,
            active_workspaces: saturate(active_workspaces),
            ..DaemonStatusResponse::default()
        }))
    }
}

fn saturate(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}
