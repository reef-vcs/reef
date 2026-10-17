//! [`Manager::reclaim`]: the one pipeline that ends a workspace's life, for
//! `Release` (R1.05), lease expiry (R1.15), and restore (R1.18). Final
//! snapshot, forget the jj workspace, tear down, and keep the row as
//! `released` or `reclaimed`.

use std::sync::{Arc, PoisonError};
use std::time::Instant;

use reef_lib::error::ErrorChain;
use reef_lib::telemetry::elapsed_ms;

use super::manager::{forget_jj_workspace, remove_state_dir};
use super::{Manager, ManagerError, ReclaimOutcome, ReleaseSpec, WorkspaceEntry};
use crate::backend::BackendError;
use crate::backend::fs_util::allocated_bytes;
use crate::model::{
    Attention, LeaseEnd, ReclaimReason, ReclaimRecord, Timestamp, WorkspaceId, WorkspaceState,
};
use crate::snapshot::{SnapshotKind, description};

/// The `reclaims.detail` of a workspace whose root was gone when it was
/// reclaimed, so no final snapshot could be taken.
const ROOT_MISSING: &str = "workspace root missing";

/// The `reclaims.detail` when the backend could not measure the workspace's
/// disk usage, so `freed_bytes` is unknown.
const DISK_USAGE_UNKNOWN: &str = "disk usage unknown";

/// The parenthesized reason in the final snapshot's description.
fn final_reason(reason: ReclaimReason) -> &'static str {
    match reason {
        ReclaimReason::Released => "released",
        ReclaimReason::Expired => "lease expired",
        ReclaimReason::Gc => "gc",
        ReclaimReason::Restore => "restore",
    }
}

/// The state a workspace ends in, and why its lease ended.
fn ending(reason: ReclaimReason) -> (WorkspaceState, LeaseEnd) {
    match reason {
        ReclaimReason::Released => (WorkspaceState::Released, LeaseEnd::Released),
        ReclaimReason::Expired | ReclaimReason::Gc => {
            (WorkspaceState::Reclaimed, LeaseEnd::Expired)
        }
        ReclaimReason::Restore => (WorkspaceState::Reclaimed, LeaseEnd::Restored),
    }
}

impl Manager {
    /// Ends an `active` workspace's life:
    ///
    /// 1. `releasing` (for [`ReclaimReason::Expired`], only if the lease is
    ///    still past its grace: a heartbeat since the tick wins), and
    ///    untracked so no watcher snapshot races the final one.
    /// 2. A final snapshot (unless `spec.final_snapshot` is off), described
    ///    `reef: final snapshot of workspace <id> (<reason>)`. If the root is
    ///    gone there is nothing to snapshot: skipped, and the reclaim row
    ///    says so.
    /// 3. A failed final snapshot stops here: the workspace goes back to
    ///    `active` with `attention = final-snapshot-failed`, its directory
    ///    and lease intact, unless `spec.force`. So does failing to forget
    ///    the jj workspace.
    /// 4. The disk usage measured; `forget_workspace` on the repository
    ///    thread (`reef: release workspace <id>`); backend tear-down; the
    ///    state directory removed; then, in one transaction, the lease ended,
    ///    a `reclaims` row, and the terminal state.
    ///
    /// A failed tear-down does not abort: the jj workspace is already gone,
    /// so the reclaim completes and the reclaim row's detail records the
    /// error.
    #[tracing::instrument(skip_all, fields(workspace_id = %id, reason = reason.as_str(), force = spec.force))]
    pub async fn reclaim(
        &self,
        id: WorkspaceId,
        reason: ReclaimReason,
        spec: ReleaseSpec,
    ) -> Result<ReclaimOutcome, ManagerError> {
        let begun = Instant::now();
        let entry = self.active_entry(id)?;
        let _scan = entry.scan_lock.lock().await;
        let record = entry.record();
        if record.state != WorkspaceState::Active {
            return Err(ManagerError::NotActive {
                id,
                state: record.state,
            });
        }

        // 1.
        let expired_by = (reason == ReclaimReason::Expired).then(Timestamp::now);
        if !self.db.begin_reclaim(id, expired_by)? {
            let state = self.db.workspace(id)?.state;
            return Err(if state == WorkspaceState::Active {
                ManagerError::LeaseRenewed(id)
            } else {
                ManagerError::NotActive { id, state }
            });
        }
        entry.update_record(|record| record.state = WorkspaceState::Releasing);
        let lease_id = record
            .lease_id
            .map(|lease| lease.as_hex())
            .unwrap_or_default();
        if reason == ReclaimReason::Expired {
            tracing::info!(%lease_id, repo = %record.repo_id, "lease expired");
        }
        self.scheduler.untrack(id);

        // 2 and 3.
        let mut details = Vec::new();
        let root_exists = tokio::fs::symlink_metadata(&entry.paths.root).await.is_ok();
        let (snapshot_taken, final_op_id) = self
            .final_snapshot(&entry, reason, spec, root_exists, &mut details)
            .await?;

        // 4.
        let freed_bytes = self.freed_bytes(&entry).await;
        if freed_bytes.is_none() {
            details.push(DISK_USAGE_UNKNOWN.to_owned());
        }
        if let Err(error) =
            forget_jj_workspace(&entry.repo, id, "reef: release workspace {id}").await
        {
            if !spec.force {
                self.back_to_active(&entry, None);
                return Err(error);
            }
            tracing::warn!(error = %ErrorChain(&error), "cannot forget the jj workspace; releasing anyway");
        }
        // The handle is useless without its jj workspace.
        entry
            .jj
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();

        self.tear_down(&entry, root_exists, &mut details).await?;
        remove_state_dir(&entry.paths.state_dir).await;

        let (state, lease_end) = ending(reason);
        let reclaim = ReclaimRecord {
            id: 0,
            repo_id: record.repo_id,
            workspace_id: id,
            reason,
            final_op_id: final_op_id.clone(),
            snapshot_taken,
            freed_bytes,
            at: Timestamp::now(),
            detail: (!details.is_empty()).then(|| details.join("; ")),
        };
        self.db.finish_workspace(id, state, lease_end, &reclaim)?;
        entry.update_record(|record| {
            record.state = state;
            record.lease_id = None;
        });
        self.remove_entry(id);
        let message = if state == WorkspaceState::Released {
            "workspace released"
        } else {
            "workspace reclaimed"
        };
        tracing::info!(
            %lease_id,
            repo = %record.repo_id,
            op_id = final_op_id.as_deref().unwrap_or(""),
            snapshot_taken,
            freed_bytes,
            elapsed_ms = elapsed_ms(begun),
            "{message}"
        );
        Ok(ReclaimOutcome {
            snapshot_taken,
            final_op_id,
            freed_bytes,
        })
    }

    /// Steps 2 and 3 of [`Manager::reclaim`]: whether a final snapshot ran
    /// and the operation it recorded. A failure that stops the reclaim has
    /// already rolled the workspace back.
    async fn final_snapshot(
        &self,
        entry: &Arc<WorkspaceEntry>,
        reason: ReclaimReason,
        spec: ReleaseSpec,
        root_exists: bool,
        details: &mut Vec<String>,
    ) -> Result<(bool, Option<String>), ManagerError> {
        let id = entry.paths.id;
        if !root_exists {
            tracing::warn!(
                path = %entry.paths.root.display(),
                "workspace root is missing; reclaiming it without a final snapshot"
            );
            details.push(ROOT_MISSING.to_owned());
            return Ok((false, None));
        }
        if !spec.final_snapshot {
            return Ok((false, None));
        }
        let kind = SnapshotKind::Final;
        let description = description(kind, id, Some(final_reason(reason)));
        match self.snapshot_locked(entry, kind, description).await {
            Ok(snapshot) => Ok((true, snapshot.changed.then(|| snapshot.op_id.to_string()))),
            Err(error) if spec.force => {
                tracing::warn!(error = %ErrorChain(&error), "final snapshot failed; releasing anyway");
                Ok((false, None))
            }
            Err(error) => {
                self.back_to_active(entry, Some(Attention::FinalSnapshotFailed));
                Err(ManagerError::FinalSnapshotFailed {
                    id,
                    source: Box::new(error),
                })
            }
        }
    }

    /// The backend's tear-down, on the blocking pool. A failure is logged
    /// and recorded in `details`, not returned: the jj workspace is gone.
    async fn tear_down(
        &self,
        entry: &Arc<WorkspaceEntry>,
        root_exists: bool,
        details: &mut Vec<String>,
    ) -> Result<(), ManagerError> {
        let torn = {
            let entry = Arc::clone(entry);
            tokio::task::spawn_blocking(move || entry.backend.tear_down(&entry.paths)).await?
        };
        match torn {
            Ok(()) => {}
            // Already recorded as ROOT_MISSING.
            Err(BackendError::RootMissing(_)) if !root_exists => {}
            Err(BackendError::RootMissing(path)) => {
                tracing::warn!(path = %path.display(), "workspace directory was already gone");
                details.push("directory already gone at tear-down".to_owned());
            }
            Err(error) => {
                let message = ErrorChain(&error).to_string();
                tracing::error!(error = %message, "cannot remove the workspace directory");
                details.push(format!("tear-down failed: {message}"));
            }
        }
        Ok(())
    }

    /// Rolls an aborted release or reclaim back to `active`, setting
    /// `attention` if given. Failures are logged; the next daemon start
    /// also reverts a `releasing` row.
    fn back_to_active(&self, entry: &WorkspaceEntry, attention: Option<Attention>) {
        let id = entry.paths.id;
        if let Err(error) = self.db.set_workspace_state(id, WorkspaceState::Active) {
            tracing::error!(error = %ErrorChain(&error), "cannot roll the release back");
            return;
        }
        entry.update_record(|record| record.state = WorkspaceState::Active);
        self.scheduler.track(id);
        if let Some(attention) = attention {
            match self.db.set_workspace_attention(id, Some(attention), None) {
                Ok(()) => entry.update_record(|record| record.attention = Some(attention)),
                Err(error) => {
                    tracing::error!(error = %ErrorChain(&error), "cannot mark the workspace");
                }
            }
        }
    }

    /// The bytes the workspace has allocated, estimated before tear-down:
    /// the backend's disk usage beyond the base plus the working-copy state.
    /// `None` when the backend could not measure it.
    async fn freed_bytes(&self, entry: &Arc<WorkspaceEntry>) -> Option<u64> {
        let record = entry.record();
        let usage = self.disk_usage(Arc::clone(entry), &record).await?;
        let state_dir = entry.paths.state_dir.clone();
        let state = tokio::task::spawn_blocking(move || allocated_bytes(&state_dir))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(0);
        Some(usage.allocated_bytes + state)
    }
}
