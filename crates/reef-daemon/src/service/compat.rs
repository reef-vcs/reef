//! `reef.daemon.v0.Daemon`, frozen at 0.1.0: the one v0 method this daemon
//! serves, so a 0.1 CLI can still run `reef daemon status` and `reef daemon
//! stop` against it (R2.01). Every other v0 method is answered by the
//! server's `CompatLayer` with an upgrade message.

use std::sync::Arc;

use reef_api::v0::daemon_server::Daemon;
use reef_api::v0::{BackendProbe, StatusRequest, StatusResponse};
use tonic::{Request, Response, Status};

use super::daemon::daemon_status;
use crate::backend::BackendKind;
use crate::daemon::AppState;

/// Serves the frozen v0 `Daemon`.
#[derive(Debug)]
pub struct V0DaemonService {
    state: Arc<AppState>,
}

impl V0DaemonService {
    /// A service answering from `state`.
    pub fn new(state: Arc<AppState>) -> V0DaemonService {
        V0DaemonService { state }
    }
}

/// A v1 backend as 0.1 named it.
fn v0_backend_name(backend: reef_api::v1::Backend) -> &'static str {
    use reef_api::v1::Backend;
    match backend {
        Backend::Copy => BackendKind::Copy.as_str(),
        Backend::Apfs => BackendKind::ApfsClone.as_str(),
        Backend::Reflink => BackendKind::Reflink.as_str(),
        Backend::Overlayfs => BackendKind::Overlayfs.as_str(),
        Backend::FuseOverlayfs => BackendKind::FuseOverlayfs.as_str(),
        Backend::Auto | Backend::Unspecified => "",
    }
}

#[tonic::async_trait]
impl Daemon for V0DaemonService {
    async fn status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let status = daemon_status(&self.state, false).await?;
        Ok(Response::new(StatusResponse {
            version: status.version,
            pid: status.pid,
            uptime_ms: status.uptime_ms,
            state_dir: status.state_dir,
            socket: status.socket,
            schema_version: status.schema_version,
            repositories: status.repository_count,
            backends: status
                .backends
                .iter()
                .map(|probe| BackendProbe {
                    kind: v0_backend_name(probe.backend()).to_owned(),
                    available: probe.available,
                    reason: probe.reason.clone(),
                    filesystem: probe.filesystem.clone(),
                })
                .collect(),
            active_workspaces: status.active_workspace_count,
        }))
    }
}
