//! [`Manager::snapshot`]: an explicit snapshot of one workspace. R1.13
//! replaces the internals with its scan-then-commit split and staleness
//! policy; until then the workspace's own jj handle snapshots (on the
//! blocking pool) and the repository thread is told to reload at head,
//! merging the operation with any written meanwhile.

use std::sync::{Arc, PoisonError};

use reef_lib::error::ErrorChain;

use super::{Manager, ManagerError, Snapshotted, WorkspaceEntry};
use crate::model::{Timestamp, WorkspaceId, WorkspaceState};
use crate::snapshot::SnapshotKind;

/// The operation description for a snapshot of `kind` (README, "Operation
/// descriptions").
pub(super) fn description(kind: SnapshotKind, id: WorkspaceId) -> String {
    match kind {
        SnapshotKind::Final => format!("reef: final snapshot of workspace {id} (released)"),
        SnapshotKind::Shutdown => format!("reef: final snapshot of workspace {id} (shutdown)"),
        SnapshotKind::Watcher | SnapshotKind::Explicit | SnapshotKind::Move => {
            format!("reef: snapshot workspace {id}")
        }
    }
}

impl Manager {
    /// Snapshots an `active` workspace's files into its working-copy commit.
    /// `description` names the operation; empty for the default of `kind`.
    #[tracing::instrument(skip_all, fields(workspace_id = %id, kind = kind.as_str()))]
    pub async fn snapshot(
        &self,
        id: WorkspaceId,
        kind: SnapshotKind,
        description: &str,
    ) -> Result<Snapshotted, ManagerError> {
        let entry = self.active_entry(id)?;
        let _scan = entry.scan_lock.lock().await;
        // A release may have finished while this waited.
        let state = entry.record().state;
        if state != WorkspaceState::Active {
            return Err(ManagerError::NotActive { id, state });
        }
        self.snapshot_locked(&entry, kind, description).await
    }

    /// [`Manager::snapshot`] with the entry's scan lock already held.
    pub(super) async fn snapshot_locked(
        &self,
        entry: &Arc<WorkspaceEntry>,
        kind: SnapshotKind,
        description: &str,
    ) -> Result<Snapshotted, ManagerError> {
        let record = entry.record();
        let id = record.id;
        let description = if description.is_empty() {
            self::description(kind, id)
        } else {
            description.to_owned()
        };
        // Never closed, so this only waits.
        let _permit = Arc::clone(&self.scans).acquire_owned().await;
        let jj = Arc::clone(&entry.jj);
        let outcome = tokio::task::spawn_blocking(move || {
            let mut guard = jj.lock().unwrap_or_else(PoisonError::into_inner);
            let repo = guard.as_mut().ok_or(ManagerError::NotLoaded(id))?;
            Ok::<_, ManagerError>(pollster::block_on(repo.snapshot(&description))?)
        })
        .await??;
        let commit_id = outcome.wc_commit.to_string();
        if !outcome.changed {
            return Ok(Snapshotted {
                op_id: None,
                changed: false,
                commit_id,
            });
        }
        let op_id = outcome.op_id.to_string();
        // The operation is committed whatever happens next; the repository
        // thread reloads (merging it with any sibling) after this job.
        if let Err(error) = entry.repo.call(crate::repo::RepoHead::note_committed).await {
            tracing::warn!(error = %ErrorChain(&error), "cannot tell the repository thread to reload");
        }
        let now = Timestamp::now();
        self.db
            .set_workspace_commit(id, &record.change_id, &commit_id, None)?;
        self.db.record_snapshot(id, &op_id, now)?;
        entry.update_record(|record| {
            record.commit_id.clone_from(&commit_id);
            record.last_snapshot_op = Some(op_id.clone());
            record.last_snapshot_at = Some(now);
        });
        Ok(Snapshotted {
            op_id: Some(op_id),
            changed: true,
            commit_id,
        })
    }
}
