//! `reef.daemon.v0.Maintenance`: garbage collection (R1.17).

use std::sync::Arc;

use tonic::{Request, Response, Status};
use tracing::Instrument as _;

use super::convert::timestamp;
use crate::daemon::AppState;
use crate::error::status;
use crate::maintenance::Busy;
use crate::maintenance::gc::{self, GcReport};
use crate::proto::v0::maintenance_server::Maintenance;
use crate::proto::v0::{self, GcRequest, GcResponse};

/// Serves `Maintenance`.
#[derive(Debug)]
pub struct MaintenanceService {
    state: Arc<AppState>,
}

impl MaintenanceService {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> MaintenanceService {
        MaintenanceService { state }
    }
}

impl From<&GcReport> for v0::GcReport {
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
        }
    }
}

#[tonic::async_trait]
impl Maintenance for MaintenanceService {
    #[tracing::instrument(skip_all, fields(repo = %request.get_ref().repo, dry_run = request.get_ref().dry_run))]
    async fn gc(&self, request: Request<GcRequest>) -> Result<Response<GcResponse>, Status> {
        let request = request.into_inner();
        let state = Arc::clone(&self.state);
        // Its own task: a client that stops waiting must not cut GC between
        // abandoning operations and moving the working copies to the new
        // head.
        let task = async move {
            let Some(_maintenance) = state.maintenance.try_acquire() else {
                return Err(status(Busy));
            };
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
        Ok(Response::new(GcResponse {
            reports: reports.iter().map(v0::GcReport::from).collect(),
        }))
    }
}
