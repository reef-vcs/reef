//! `reef.api.v1.MaintenanceService`: garbage collection (R1.17), backup and
//! quiesce (R1.18).

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::Stream;
use reef_api::v1::maintenance_service_server::MaintenanceService;
use reef_api::v1::{
    self, BackupRequest, BackupResponse, BackupStep, CollectGarbageRequest, CollectGarbageResponse,
    QuiesceRequest, QuiesceResponse, ResumeRequest, ResumeResponse,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::Instrument as _;

use super::convert::{request_duration, timestamp};
use crate::daemon::AppState;
use crate::error::status;
use crate::maintenance::gc::{self, GcReport};
use crate::maintenance::{Task, backup, quiesce};
use crate::model::RepoId;

/// Progress messages buffered for a slow client; more are dropped.
const PROGRESS_BUFFER: usize = 64;

type BackupStream = Pin<Box<dyn Stream<Item = Result<BackupResponse, Status>> + Send>>;

/// Serves `MaintenanceService`.
#[derive(Debug)]
pub struct MaintenanceHandler {
    state: Arc<AppState>,
}

impl MaintenanceHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> MaintenanceHandler {
        MaintenanceHandler { state }
    }
}

impl From<&GcReport> for v1::GcReport {
    fn from(report: &GcReport) -> Self {
        Self {
            repo_id: report.repo_id.as_hex(),
            keep_newer: Some(timestamp(report.keep_newer)),
            jj_bytes_before: report.jj_bytes_before,
            jj_bytes_after: report.jj_bytes_after,
            reindex_ms: report.reindex_ms,
            jj_error: report.jj_error.clone().unwrap_or_default(),
            base_entries_removed: report.base_entries_removed,
            base_bytes_freed: report.base_bytes_freed,
            workspace_rows_removed: report.workspace_rows_removed,
            wc_bytes_freed: report.wc_bytes_freed,
            stores_removed: report.stores_removed,
            store_bytes_freed: report.store_bytes_freed,
            elapsed_ms: report.elapsed_ms,
            dry_run: report.dry_run,
            base_bytes_unshared: report.base_bytes_unshared,
        }
    }
}

impl MaintenanceHandler {
    /// A request's `repo`: `None` when empty, else resolved.
    fn repo(&self, text: &str) -> Result<Option<RepoId>, Status> {
        if text.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(
            self.state.manager.resolve_repo(text).map_err(status)?.id,
        ))
    }
}

#[tonic::async_trait]
impl MaintenanceService for MaintenanceHandler {
    type BackupStream = BackupStream;

    #[tracing::instrument(skip_all, fields(repo = %request.get_ref().repo, dry_run = request.get_ref().dry_run))]
    async fn collect_garbage(
        &self,
        request: Request<CollectGarbageRequest>,
    ) -> Result<Response<CollectGarbageResponse>, Status> {
        let request = request.into_inner();
        let state = Arc::clone(&self.state);
        // Its own task: a client that stops waiting must not cut GC between
        // abandoning operations and moving the working copies to the new
        // head.
        let task = async move {
            let _maintenance = state.maintenance.try_acquire(Task::Gc).map_err(status)?;
            let repo = if request.repo.trim().is_empty() {
                None
            } else {
                Some(
                    state
                        .manager
                        .resolve_repo(&request.repo)
                        .map_err(status)?
                        .id,
                )
            };
            gc::run(&state, repo, request.dry_run).await.map_err(status)
        };
        let reports = tokio::spawn(task.instrument(tracing::Span::current()))
            .await
            .map_err(|error| Status::internal(format!("the GC task failed: {error}")))??;
        Ok(Response::new(CollectGarbageResponse {
            reports: reports.iter().map(v1::GcReport::from).collect(),
        }))
    }

    #[tracing::instrument(skip_all, fields(repo = %request.get_ref().repo, dest = %request.get_ref().dest))]
    async fn backup(
        &self,
        request: Request<BackupRequest>,
    ) -> Result<Response<Self::BackupStream>, Status> {
        let request = request.into_inner();
        let repo = self.repo(&request.repo)?;
        let dest = PathBuf::from(request.dest);
        let manifest_path = dest.join(backup::MANIFEST_FILE).display().to_string();
        let state = Arc::clone(&self.state);
        let maintenance = state
            .maintenance
            .try_acquire(Task::Backup)
            .map_err(status)?;
        backup::prepare_dest(&state, &dest).map_err(status)?;
        let (events, stream) = mpsc::channel(PROGRESS_BUFFER);
        let (progress, mut steps) = mpsc::channel(PROGRESS_BUFFER);
        // Its own task: a client that disconnects does not cut the backup
        // between pausing a repository thread and resuming it, or leave a
        // manifest half written.
        let job = tokio::spawn(
            async move {
                let _maintenance = maintenance;
                backup::run(&state, &dest, repo, progress).await
            }
            .instrument(tracing::Span::current()),
        );
        tokio::spawn(async move {
            while let Some(step) = steps.recv().await {
                // A client with no room misses a step; the last message is
                // sent with `send`, waiting for room.
                let _ = events.try_send(Ok(step));
            }
            let last = match job.await {
                Ok(Ok(manifest)) => Ok(BackupResponse {
                    repo_id: String::new(),
                    step: BackupStep::Done.into(),
                    files: manifest.repos.iter().map(|repo| repo.files).sum(),
                    bytes: manifest.repos.iter().map(|repo| repo.bytes).sum::<u64>()
                        + manifest.sqlite.bytes,
                    manifest_path,
                }),
                Ok(Err(error)) => {
                    backup::log_failure(&error);
                    Err(status(error))
                }
                Err(error) => Err(Status::internal(format!("the backup task failed: {error}"))),
            };
            let _ = events.send(last).await;
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(stream))))
    }

    #[tracing::instrument(skip_all, fields(repo = %request.get_ref().repo))]
    async fn quiesce(
        &self,
        request: Request<QuiesceRequest>,
    ) -> Result<Response<QuiesceResponse>, Status> {
        let request = request.into_inner();
        let repo = self.repo(&request.repo)?;
        let state = Arc::clone(&self.state);
        let max = request_duration(request.max.as_ref()).unwrap_or(Duration::ZERO);
        // Its own task: a client that stops waiting must not leave the
        // threads paused without a token anyone knows; the auto-resume
        // still ends it.
        let quiesced = tokio::spawn(
            async move { quiesce::quiesce(&state, repo, max).await }
                .instrument(tracing::Span::current()),
        )
        .await
        .map_err(|error| Status::internal(format!("the quiesce task failed: {error}")))?
        .map_err(status)?;
        Ok(Response::new(QuiesceResponse {
            token: quiesced.token,
            quiesced_at: Some(timestamp(quiesced.quiesced_at)),
            auto_resume_at: Some(timestamp(quiesced.auto_resume_at)),
        }))
    }

    #[tracing::instrument(skip_all)]
    async fn resume(
        &self,
        request: Request<ResumeRequest>,
    ) -> Result<Response<ResumeResponse>, Status> {
        let request = request.into_inner();
        let paused = quiesce::resume(&self.state, request.token.trim()).map_err(status)?;
        Ok(Response::new(ResumeResponse {
            paused_ms: u64::try_from(paused.as_millis()).unwrap_or(u64::MAX),
        }))
    }
}
