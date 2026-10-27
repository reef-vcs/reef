//! `reef.api.v1.PresenceService` (R2.10, ADR 0037): reports, clears and
//! subscriptions over [`crate::presence::PresenceHub`]. Requests are
//! validated and their references resolved here; the hub decides ids,
//! caps, fixed fields, and who may touch an entry.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use reef_api::v1::presence_service_server::PresenceService;
use reef_api::v1::{
    ClearPresenceRequest, ClearPresenceResponse, PresenceKind, ReportPresenceRequest,
    ReportPresenceResponse, SubscribePresenceRequest, SubscribePresenceResponse,
};
use reef_lib::jj::diff::{PathError, check_paths};
use tonic::{Request, Response, Status};

use super::convert::{self, known};
use super::operations::{InvalidChangeFilter, change_prefix};
use crate::daemon::AppState;
use crate::error::status;
use crate::model::{PresenceId, RepoId, WorkspaceId, WorkspaceState};
use crate::presence::{
    MAX_ACTIVITY_BYTES, MAX_LABEL_BYTES, MAX_PATHS, PresenceError, PresenceFilter, Report,
    clamp_ttl, session,
};
use crate::workspace::ManagerError;

/// A change id's length in hex.
const CHANGE_ID_HEX_LEN: usize = 32;

/// Serves `PresenceService`.
#[derive(Debug)]
pub struct PresenceHandler {
    state: Arc<AppState>,
}

impl PresenceHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> PresenceHandler {
        PresenceHandler { state }
    }

    /// The repository and workspace a report names: a workspace, which
    /// must be active, gives its repository.
    fn resolve(
        &self,
        repo: &str,
        workspace: &str,
    ) -> Result<(Option<RepoId>, Option<WorkspaceId>), Status> {
        let manager = &self.state.manager;
        if !workspace.trim().is_empty() {
            let record = manager.resolve_workspace(repo, workspace).map_err(status)?;
            if record.state != WorkspaceState::Active {
                return Err(status(ManagerError::NotActive {
                    id: record.id,
                    repo: record.repo_id,
                    state: record.state,
                }));
            }
            return Ok((Some(record.repo_id), Some(record.id)));
        }
        if !repo.trim().is_empty() {
            return Ok((Some(manager.resolve_repo(repo).map_err(status)?.id), None));
        }
        Ok((None, None))
    }
}

type SubscribePresenceStream =
    Pin<Box<dyn Stream<Item = Result<SubscribePresenceResponse, Status>> + Send>>;

/// `text` as a presence id, or `INVALID_PRESENCE_ID`.
fn presence_id(text: &str) -> Result<PresenceId, PresenceError> {
    text.trim()
        .parse()
        .map_err(|_| PresenceError::InvalidId(text.to_owned()))
}

/// A field over its bound.
fn too_large(field: &'static str, limit: &str) -> Status {
    status(PresenceError::TooLarge {
        field,
        limit: limit.to_owned(),
    })
}

/// A malformed request with no reason.
fn invalid(message: &str) -> Status {
    status(PresenceError::Invalid(message.to_owned()))
}

/// Checks what a report carries against its kind and the bounds.
fn check_report(request: &ReportPresenceRequest, kind: PresenceKind) -> Result<(), Status> {
    if !request.activity.is_empty() && kind != PresenceKind::Activity {
        return Err(invalid("activity is for PRESENCE_KIND_ACTIVITY only"));
    }
    if !request.paths.is_empty() && kind != PresenceKind::Editing {
        return Err(invalid("paths are for PRESENCE_KIND_EDITING only"));
    }
    if request.progress.is_some() && kind != PresenceKind::Progress {
        return Err(invalid("progress is for PRESENCE_KIND_PROGRESS only"));
    }
    if request.activity.len() > MAX_ACTIVITY_BYTES {
        return Err(too_large("activity", "1,024 bytes"));
    }
    if request.paths.len() > MAX_PATHS {
        return Err(too_large("paths", "64"));
    }
    if let Some(empty) = request.paths.iter().find(|path| path.is_empty()) {
        return Err(status(PathError {
            path: empty.clone(),
        }));
    }
    check_paths(&request.paths).map_err(status)?;
    if let Some(progress) = &request.progress {
        if progress.phase.len() > MAX_LABEL_BYTES {
            return Err(too_large("progress.phase", "64 bytes"));
        }
        if progress.unit.len() > MAX_LABEL_BYTES {
            return Err(too_large("progress.unit", "64 bytes"));
        }
    }
    if let Some(ttl) = &request.ttl
        && (ttl.seconds < 0 || ttl.nanos < 0)
    {
        return Err(invalid("ttl must not be negative"));
    }
    Ok(())
}

/// A report's `change` as full hex; empty for none.
fn change_id(request: &ReportPresenceRequest) -> Result<String, Status> {
    let change = request.change.trim();
    if change.is_empty() {
        return Ok(String::new());
    }
    if request.repo.trim().is_empty() && request.workspace.trim().is_empty() {
        return Err(invalid("change needs repo or workspace"));
    }
    match change_prefix(change) {
        Some(hex) if hex.len() == CHANGE_ID_HEX_LEN => Ok(hex),
        _ => Err(status(InvalidChangeFilter {
            change: change.to_owned(),
        })),
    }
}

#[tonic::async_trait]
impl PresenceService for PresenceHandler {
    type SubscribePresenceStream = SubscribePresenceStream;

    async fn report_presence(
        &self,
        request: Request<ReportPresenceRequest>,
    ) -> Result<Response<ReportPresenceResponse>, Status> {
        let request = request.into_inner();
        let agent = request.agent.trim();
        if agent.is_empty()
            || agent.len() > reef_api::AGENT_MAX_BYTES
            || !agent.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
        {
            return Err(status(PresenceError::InvalidAgent));
        }
        let replace = if request.presence_id.trim().is_empty() {
            None
        } else {
            Some(presence_id(&request.presence_id).map_err(status)?)
        };
        let kind: PresenceKind = known("kind", request.kind).map_err(status)?;
        if kind == PresenceKind::Unspecified {
            return Err(invalid("kind is required"));
        }
        if kind == PresenceKind::Lease {
            return Err(status(PresenceError::DaemonKind));
        }
        check_report(&request, kind)?;
        let change_id = change_id(&request)?;
        let (repo, workspace) = self.resolve(&request.repo, &request.workspace)?;
        let report = Report {
            agent: request.agent.trim().to_owned(),
            repo,
            workspace,
            change_id,
            kind,
            activity: request.activity,
            paths: request.paths,
            progress: request.progress,
            lease: None,
            ttl: clamp_ttl(convert::request_duration(request.ttl.as_ref())),
        };
        let presence = self
            .state
            .presence
            .report(replace, report)
            .map_err(status)?;
        Ok(Response::new(ReportPresenceResponse {
            presence: Some(presence),
        }))
    }

    async fn clear_presence(
        &self,
        request: Request<ClearPresenceRequest>,
    ) -> Result<Response<ClearPresenceResponse>, Status> {
        let id = presence_id(&request.into_inner().presence_id).map_err(status)?;
        self.state.presence.clear(id).map_err(status)?;
        Ok(Response::new(ClearPresenceResponse {}))
    }

    async fn subscribe_presence(
        &self,
        request: Request<SubscribePresenceRequest>,
    ) -> Result<Response<Self::SubscribePresenceStream>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        let mut filter = PresenceFilter::default();
        if !request.repos.is_empty() {
            let mut repos = HashSet::new();
            for repo in &request.repos {
                repos.insert(state.manager.resolve_repo(repo).map_err(status)?.id);
            }
            filter.repos = Some(repos);
        }
        if !request.workspaces.is_empty() {
            let mut workspaces = HashSet::new();
            for workspace in &request.workspaces {
                let record = state
                    .manager
                    .resolve_workspace("", workspace)
                    .map_err(status)?;
                workspaces.insert(record.id);
            }
            filter.workspaces = Some(workspaces);
        }
        if !request.agents.is_empty() {
            filter.agents = Some(request.agents.iter().cloned().collect());
        }
        if !request.kinds.is_empty() {
            let mut kinds = HashSet::new();
            for value in &request.kinds {
                let kind: PresenceKind = known("kinds", *value).map_err(status)?;
                if kind == PresenceKind::Unspecified {
                    return Err(invalid("kinds holds PRESENCE_KIND_UNSPECIFIED"));
                }
                kinds.insert(*value);
            }
            filter.kinds = Some(kinds);
        }
        let slot = state
            .subscriptions
            .admit(state.config.subscribe.max_subscriptions)
            .map_err(status)?;
        let frames = session::spawn(
            Arc::clone(&state.presence),
            filter,
            state.shutdown.clone(),
            slot,
        );
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(frames),
        )))
    }
}
