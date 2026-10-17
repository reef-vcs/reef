//! [`Manager::release`]: final snapshot, forget the jj workspace, tear
//! down, and keep the row as `released`.

use std::sync::{Arc, PoisonError};

use reef_lib::error::ErrorChain;

use super::manager::{forget_jj_workspace, remove_state_dir};
use super::{Manager, ManagerError, ReleaseSpec, Released, WorkspaceEntry};
use crate::backend::BackendError;
use crate::backend::fs_util::allocated_bytes;
use crate::model::{
    Attention, LeaseEnd, ReclaimReason, ReclaimRecord, Timestamp, WorkspaceId, WorkspaceState,
};
use crate::snapshot::{SnapshotKind, description};

impl Manager {
    /// Releases an `active` workspace: `releasing`; a final snapshot
    /// (unless `spec.final_snapshot` is off); `forget_workspace` on the
    /// repository thread; backend tear-down; the state directory removed;
    /// then, in one transaction, the lease ended (`released`), a `reclaims`
    /// row (`released`), and `released`.
    ///
    /// A failed final snapshot aborts the release, leaving the workspace
    /// `active` with `attention = final-snapshot-failed` and its directory
    /// intact, unless `spec.force`. So does failing to forget the jj
    /// workspace. A failed tear-down does not abort: the jj workspace is
    /// already gone, so the release completes and the reclaim row's detail
    /// records the error.
    #[tracing::instrument(skip_all, fields(workspace_id = %id, force = spec.force))]
    pub async fn release(
        &self,
        id: WorkspaceId,
        spec: ReleaseSpec,
    ) -> Result<Released, ManagerError> {
        let entry = self.active_entry(id)?;
        let _scan = entry.scan_lock.lock().await;
        let record = entry.record();
        if record.state != WorkspaceState::Active {
            return Err(ManagerError::NotActive {
                id,
                state: record.state,
            });
        }
        self.db.set_workspace_state(id, WorkspaceState::Releasing)?;
        entry.update_record(|record| record.state = WorkspaceState::Releasing);
        // No watcher snapshot races the final one, or the tear-down.
        self.scheduler.untrack(id);

        let (snapshot_taken, final_op_id) = if spec.final_snapshot {
            let kind = SnapshotKind::Final;
            match self
                .snapshot_locked(&entry, kind, description(kind, id, None))
                .await
            {
                Ok(snapshot) => (true, snapshot.changed.then(|| snapshot.op_id.to_string())),
                Err(error) if spec.force => {
                    tracing::warn!(error = %ErrorChain(&error), "final snapshot failed; releasing anyway");
                    (false, None)
                }
                Err(error) => {
                    self.back_to_active(&entry, Some(Attention::FinalSnapshotFailed));
                    return Err(ManagerError::FinalSnapshotFailed {
                        id,
                        source: Box::new(error),
                    });
                }
            }
        } else {
            (false, None)
        };

        let freed_bytes = self.freed_bytes(&entry).await;
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

        let torn = {
            let entry = Arc::clone(&entry);
            tokio::task::spawn_blocking(move || entry.backend.tear_down(&entry.paths)).await?
        };
        let detail = match torn {
            Ok(()) => None,
            Err(BackendError::RootMissing(path)) => {
                tracing::warn!(path = %path.display(), "workspace directory was already gone");
                Some("directory already gone at release".to_owned())
            }
            Err(error) => {
                let message = ErrorChain(&error).to_string();
                tracing::error!(error = %message, "cannot remove the workspace directory");
                Some(format!("tear-down failed: {message}"))
            }
        };
        remove_state_dir(&entry.paths.state_dir).await;

        let reclaim = ReclaimRecord {
            id: 0,
            repo_id: record.repo_id,
            workspace_id: id,
            reason: ReclaimReason::Released,
            final_op_id: final_op_id.clone(),
            snapshot_taken,
            freed_bytes: Some(freed_bytes),
            at: Timestamp::now(),
            detail,
        };
        self.db
            .finish_workspace(id, WorkspaceState::Released, LeaseEnd::Released, &reclaim)?;
        entry.update_record(|record| {
            record.state = WorkspaceState::Released;
            record.lease_id = None;
        });
        self.remove_entry(id);
        tracing::info!(
            snapshot_taken,
            final_op_id = final_op_id.as_deref().unwrap_or(""),
            freed_bytes,
            "workspace released"
        );
        Ok(Released {
            final_op_id,
            snapshot_taken,
            freed_bytes,
        })
    }

    /// Rolls an aborted release back to `active`, setting `attention` if
    /// given. Failures are logged; the next daemon start also reverts a
    /// `releasing` row.
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
    async fn freed_bytes(&self, entry: &Arc<WorkspaceEntry>) -> u64 {
        let record = entry.record();
        let usage = self.disk_usage(Arc::clone(entry), &record).await;
        let state_dir = entry.paths.state_dir.clone();
        let state = tokio::task::spawn_blocking(move || allocated_bytes(&state_dir))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(0);
        usage.map_or(0, |usage| usage.allocated_bytes) + state
    }
}
