//! [`Manager::reclaim`]: the one pipeline that ends a workspace's life, for
//! `Release` (R1.05), lease expiry (R1.15), and restore (R1.18). Final
//! snapshot, forget the jj workspace, tear down, and keep the row as
//! `released` or `reclaimed`.

use std::sync::{Arc, PoisonError};
use std::time::Instant;

use reef_lib::error::ErrorChain;
use reef_lib::telemetry::elapsed_ms;

use super::manager::{forget_jj_workspace, jj_workspace_exists, remove_state_dir};
use super::{Manager, ManagerError, ReclaimOutcome, ReleaseSpec, WorkspaceEntry};
use crate::backend::BackendError;
use crate::backend::fs_util::allocated_bytes;
use crate::db::META_REMATERIALIZE_PENDING;
use crate::model::{
    Attention, LeaseEnd, ReclaimReason, ReclaimRecord, RepoId, Timestamp, WorkspaceId,
    WorkspaceState,
};
use crate::snapshot::{SnapshotKind, TrackReason, description};

/// The `reclaims.detail` of a workspace whose root was gone when it was
/// reclaimed, so no final snapshot could be taken.
const ROOT_MISSING: &str = "workspace root missing";

/// The `reclaims.detail` of a workspace whose repository's directory is
/// gone, so no final snapshot could be recorded.
const REPOSITORY_GONE: &str = "repository directory gone; no final snapshot";

/// The `reclaims.detail` of a workspace whose root had been replaced by a
/// symlink, which a final snapshot would have scanned through (RX.01); the
/// tear-down unlinks it.
const ROOT_SYMLINK: &str = "workspace root replaced by a symlink";

/// The `reclaims.detail` of a workspace an earlier reclaim had already
/// forgotten the jj workspace of, so there was nothing to record a final
/// snapshot into.
const JJ_WORKSPACE_FORGOTTEN: &str = "jj workspace already forgotten";

/// The `reclaims.detail` of an overlay workspace whose upper directory was
/// gone when it was reclaimed (R1.09).
const UPPER_MISSING: &str = "overlay upper directory missing";

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
pub(super) fn ending(reason: ReclaimReason) -> (WorkspaceState, LeaseEnd) {
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
    ///    gone, or an overlay workspace's upper directory (`attention =
    ///    upper-missing`, R1.09), there is nothing to snapshot: skipped, and
    ///    the reclaim row says so.
    /// 3. A failed final snapshot stops here: the workspace goes back to
    ///    `active` with `attention = final-snapshot-failed` (an overlay left
    ///    unmounted keeps `mount-failed`, which says more), its directory
    ///    and lease intact, unless `spec.force`. So does failing to forget
    ///    the jj workspace. If going back cannot be recorded (a full disk),
    ///    the workspace stays `releasing` until the next reclaim of it.
    /// 4. The disk usage measured; `forget_workspace` on the repository
    ///    thread (`reef: release workspace <id>`); the shared stores
    ///    unmounted (R1.20); backend tear-down; the state directory removed;
    ///    then, in one transaction, the lease ended, a `reclaims` row (with
    ///    `spec.detail`, if any), and the terminal state.
    ///
    /// A failed tear-down does not abort: the jj workspace is already gone,
    /// so the reclaim completes and the reclaim row's detail records the
    /// error.
    ///
    /// A workspace an earlier reclaim left `releasing` (its rollback or
    /// `finish_workspace` failed, it panicked, or the daemon died in it) is
    /// finished from where that one stopped, never set `active` first: it
    /// may be past its forget and tear-down (#311). It skips step 1, and if
    /// its jj workspace is gone, the final snapshot and the forget too
    /// (#312, #315). With no entry, a row whose directory is gone, which a
    /// start's `reclaim_missing` could not end, is ended by
    /// `reclaim_unloaded` (#313).
    ///
    /// The final snapshot and the forget carry `spec.agent`, else the
    /// owner, as `reef.agent` (R2.03).
    #[tracing::instrument(skip_all, fields(workspace_id = %id, reason = reason.as_str(), force = spec.force))]
    pub async fn reclaim(
        &self,
        id: WorkspaceId,
        reason: ReclaimReason,
        spec: ReleaseSpec,
    ) -> Result<ReclaimOutcome, ManagerError> {
        let begun = Instant::now();
        let Some(entry) = self.entry(id) else {
            return self.reclaim_unloaded(id, reason, &spec).await;
        };
        let (_scan, left_releasing) = self.lock_for_reclaim(&entry).await?;
        let record = entry.record();

        // 1.
        let forgotten = if left_releasing {
            !jj_workspace_exists(&entry.repo, id).await?
        } else {
            if record.state != WorkspaceState::Active {
                return Err(ManagerError::NotActive {
                    id,
                    repo: record.repo_id,
                    state: record.state,
                });
            }
            self.begin_reclaim(id, record.repo_id, reason)?;
            entry.update_record(|record| record.state = WorkspaceState::Releasing);
            false
        };
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
            .final_snapshot(&entry, reason, &spec, root_exists, forgotten, &mut details)
            .await?;

        // 4.
        if let Some(detail) = spec.detail {
            details.push(detail.to_owned());
        }
        let freed_bytes = self.freed_bytes(&entry, root_exists).await;
        if freed_bytes.is_none() {
            details.push(DISK_USAGE_UNKNOWN.to_owned());
        }
        let release_op_id = if forgotten {
            None
        } else {
            self.forget(&entry, &spec, root_exists).await?
        };
        // The handle is useless without its jj workspace.
        entry
            .jj
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();

        self.unmount_stores(&entry, &mut details).await;
        self.tear_down(&entry, root_exists, &mut details).await;
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
        self.disk.forget(id);
        self.end_sessions(id);
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
            release_op_id,
        })
    }

    /// Steps 2 and 3 of [`Manager::reclaim`]: whether a final snapshot ran
    /// and the operation it recorded. None if the jj workspace is
    /// `forgotten`. A failure that stops the reclaim has already rolled the
    /// workspace back.
    async fn final_snapshot(
        &self,
        entry: &Arc<WorkspaceEntry>,
        reason: ReclaimReason,
        spec: &ReleaseSpec,
        root_exists: bool,
        forgotten: bool,
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
        if forgotten {
            tracing::warn!(
                "an earlier reclaim forgot the jj workspace; reclaiming it without a final snapshot"
            );
            details.push(JJ_WORKSPACE_FORGOTTEN.to_owned());
            return Ok((false, None));
        }
        if tokio::fs::symlink_metadata(&entry.paths.root)
            .await
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            tracing::warn!(
                path = %entry.paths.root.display(),
                "workspace root is a symlink; reclaiming it without a final snapshot"
            );
            details.push(ROOT_SYMLINK.to_owned());
            return Ok((false, None));
        }
        let attention = entry.record().attention;
        if attention == Some(Attention::UpperMissing) {
            tracing::warn!(
                path = %entry.paths.upper.display(),
                "the overlay's upper directory is missing; reclaiming it without a final snapshot"
            );
            details.push(UPPER_MISSING.to_owned());
            return Ok((false, None));
        }
        if !spec.final_snapshot {
            return Ok((false, None));
        }
        let kind = SnapshotKind::Final;
        let description = description(kind, id, Some(final_reason(reason)));
        match self
            .snapshot_locked(entry, kind, description, spec.agent.clone())
            .await
        {
            Ok(snapshot) => Ok((true, snapshot.changed.then(|| snapshot.op_id.to_string()))),
            Err(error) if spec.force => {
                tracing::warn!(error = %ErrorChain(&error), "final snapshot failed; releasing anyway");
                Ok((false, None))
            }
            Err(error) => {
                let attention = (attention != Some(Attention::MountFailed))
                    .then_some(Attention::FinalSnapshotFailed);
                self.back_to_active(entry, attention).await;
                Err(ManagerError::FinalSnapshotFailed {
                    id,
                    source: Box::new(error),
                })
            }
        }
    }

    /// Step 4's `forget_workspace`: the operation it recorded, if any. A
    /// failure rolls the workspace back unless `spec.force`, or unless its
    /// root is gone (an earlier forced reclaim tore it down): then it stays
    /// `releasing`, for the next reclaim, rather than `active` with no
    /// directory.
    async fn forget(
        &self,
        entry: &WorkspaceEntry,
        spec: &ReleaseSpec,
        root_exists: bool,
    ) -> Result<Option<String>, ManagerError> {
        let id = entry.paths.id;
        let owner = entry.record().owner;
        let agent = spec.agent.as_deref().unwrap_or(&owner);
        match forget_jj_workspace(&entry.repo, id, "reef: release workspace {id}", agent).await {
            Ok(op_id) => Ok(op_id),
            Err(error) if !spec.force => {
                if root_exists {
                    self.back_to_active(entry, None).await;
                }
                Err(error)
            }
            Err(error) => {
                tracing::warn!(error = %ErrorChain(&error), "cannot forget the jj workspace; releasing anyway");
                Ok(None)
            }
        }
    }

    /// The backend's tear-down, on the blocking pool. A failure is logged
    /// and recorded in `details`, not returned: the jj workspace is gone, so
    /// the reclaim must still be recorded.
    async fn tear_down(
        &self,
        entry: &Arc<WorkspaceEntry>,
        root_exists: bool,
        details: &mut Vec<String>,
    ) {
        let torn = {
            let entry = Arc::clone(entry);
            tokio::task::spawn_blocking(move || entry.backend.tear_down(&entry.paths)).await
        };
        let torn = match torn {
            Ok(torn) => torn,
            Err(error) => {
                tracing::error!(%error, "the tear-down task failed");
                details.push(format!("tear-down task failed: {error}"));
                return;
            }
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
    }

    /// Rolls an aborted release or reclaim back to `active`, setting
    /// `attention` if given. Failures are logged; the workspace stays
    /// `releasing`, and the next reclaim of it finishes it.
    async fn back_to_active(&self, entry: &WorkspaceEntry, attention: Option<Attention>) {
        let id = entry.paths.id;
        if let Err(error) = self.db.set_workspace_state(id, WorkspaceState::Active) {
            tracing::error!(error = %ErrorChain(&error), "cannot roll the release back");
            return;
        }
        entry.update_record(|record| record.state = WorkspaceState::Active);
        self.scheduler.track(id, TrackReason::Restored).await;
        if let Some(attention) = attention {
            match self.db.set_workspace_attention(id, Some(attention), None) {
                Ok(()) => entry.update_record(|record| record.attention = Some(attention)),
                Err(error) => {
                    tracing::error!(error = %ErrorChain(&error), "cannot mark the workspace");
                }
            }
        }
    }

    /// Ends the reclaimed workspace `id`'s presence and its provenance
    /// session (R3.04), after its final snapshot recorded.
    fn end_sessions(&self, id: WorkspaceId) {
        self.presence.end_workspace(id);
        if let Some(provenance) = self.provenance.get() {
            provenance.end(id);
        }
    }

    /// The scan lock for [`Manager::reclaim`], and whether an earlier
    /// reclaim left the workspace `releasing`. A reclaim in flight holds the
    /// lock while the workspace is `releasing`: refused at once then, rather
    /// than queued behind it. So one found `releasing` under the lock was
    /// left so: [`Manager::back_to_active`] could not record `active` (on a
    /// full disk, SQLite cannot), `finish_workspace` failed, a reclaim
    /// panicked, or a start found a release the daemon died in.
    async fn lock_for_reclaim<'a>(
        &self,
        entry: &'a WorkspaceEntry,
    ) -> Result<(tokio::sync::MutexGuard<'a, ()>, bool), ManagerError> {
        let scan = if entry.record().state == WorkspaceState::Releasing {
            entry
                .scan_lock
                .try_lock()
                .map_err(|_| ManagerError::NotActive {
                    id: entry.paths.id,
                    repo: entry.record().repo_id,
                    state: WorkspaceState::Releasing,
                })?
        } else {
            entry.scan_lock.lock().await
        };
        let left_releasing = entry.record().state == WorkspaceState::Releasing;
        if left_releasing {
            tracing::info!("an earlier reclaim left the workspace releasing; finishing it");
        }
        Ok((scan, left_releasing))
    }

    /// Step 1 of [`Manager::reclaim`]: `active` to `releasing` in the
    /// database (for [`ReclaimReason::Expired`], only if the lease is still
    /// past its grace), or why not.
    fn begin_reclaim(
        &self,
        id: WorkspaceId,
        repo: RepoId,
        reason: ReclaimReason,
    ) -> Result<(), ManagerError> {
        let expired_by = (reason == ReclaimReason::Expired).then(Timestamp::now);
        if self.db.begin_reclaim(id, expired_by)? {
            return Ok(());
        }
        let state = self.db.workspace(id)?.state;
        Err(if state == WorkspaceState::Active {
            ManagerError::LeaseNotExpired(id)
        } else {
            ManagerError::NotActive { id, repo, state }
        })
    }

    /// [`Manager::reclaim`] of a workspace with no entry: an unfinished row
    /// whose directory is gone is ended as a start ends it
    /// ([`Manager::reclaim_missing`]), but for `reason` and through
    /// `releasing`, as a loaded one is. Without a scan lock to take, two of
    /// these can race; `finish_workspace` lets one end it. Otherwise, or
    /// while a restore's rematerialization is pending (which rebuilds
    /// missing directories), [`ManagerError::NotLoaded`].
    ///
    /// A row whose repository's directory is gone has no handle to forget
    /// its jj workspace with and no repository to snapshot into. When its
    /// directory is also gone there is nothing to lose, so it is ended as
    /// above. When the directory is there, a release that asks for a final
    /// snapshot is refused with
    /// [`ManagerError::FinalSnapshotRepositoryGone`], keeping its edits;
    /// one that skips the snapshot (`--no-snapshot`, or `force`) ends it and
    /// tears the directory down.
    async fn reclaim_unloaded(
        &self,
        id: WorkspaceId,
        reason: ReclaimReason,
        spec: &ReleaseSpec,
    ) -> Result<ReclaimOutcome, ManagerError> {
        let record = self.db.workspace(id)?;
        if !matches!(
            record.state,
            WorkspaceState::Active | WorkspaceState::Releasing
        ) {
            return Err(ManagerError::NotActive {
                id,
                repo: record.repo_id,
                state: record.state,
            });
        }
        let mut repository_gone = None;
        let handle = match self.handle(record.repo_id) {
            Ok(handle) => Some(handle),
            Err(ManagerError::RepoGone { root, .. }) => {
                tracing::warn!(
                    workspace_id = %id,
                    repo = %record.repo_id,
                    path = %root.display(),
                    "the workspace's repository directory is gone; no final snapshot can be recorded"
                );
                repository_gone = Some(root);
                None
            }
            Err(error) => return Err(error),
        };
        let missing = tokio::fs::symlink_metadata(&record.path)
            .await
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        if self.db.meta(META_REMATERIALIZE_PENDING)?.is_some() {
            return Err(ManagerError::NotLoaded(id));
        }
        match &repository_gone {
            Some(root) if !missing && spec.final_snapshot && !spec.force => {
                return Err(ManagerError::FinalSnapshotRepositoryGone {
                    id,
                    repo: record.repo_id,
                    root: root.clone(),
                });
            }
            None if !missing => return Err(ManagerError::NotLoaded(id)),
            _ => {}
        }
        if record.state == WorkspaceState::Active {
            self.begin_reclaim(id, record.repo_id, reason)?;
        }
        let mut details = Vec::new();
        if missing {
            details.push(ROOT_MISSING.to_owned());
        }
        if repository_gone.is_some() {
            details.push(REPOSITORY_GONE.to_owned());
        }
        if let Some(detail) = spec.detail {
            details.push(detail.to_owned());
        }
        let detail = details.join("; ");
        let agent = spec.agent.as_deref().unwrap_or(&record.owner);
        let release_op_id = match self
            .reclaim_missing(&record, handle.as_ref(), reason, &detail, agent, !missing)
            .await
        {
            Ok(release_op_id) => release_op_id,
            Err(error) => {
                // A reclaim racing this one ended it first.
                return Err(match self.db.workspace(id) {
                    Ok(row) if row.state.is_terminal() => ManagerError::NotActive {
                        id,
                        repo: row.repo_id,
                        state: row.state,
                    },
                    _ => error,
                });
            }
        };
        Ok(ReclaimOutcome {
            snapshot_taken: false,
            final_op_id: None,
            freed_bytes: None,
            release_op_id,
        })
    }

    /// Leaves `id` as a reclaim whose [`Manager::back_to_active`] failed
    /// does: `releasing` in the database and in memory, and untracked.
    #[cfg(feature = "testing")]
    pub async fn strand_releasing(&self, id: WorkspaceId) -> Result<(), ManagerError> {
        let entry = self.active_entry(id)?;
        let _scan = entry.scan_lock.lock().await;
        if !self.db.begin_reclaim(id, None)? {
            return Err(ManagerError::NotActive {
                id,
                repo: entry.record().repo_id,
                state: self.db.workspace(id)?.state,
            });
        }
        entry.update_record(|record| record.state = WorkspaceState::Releasing);
        self.scheduler.untrack(id);
        Ok(())
    }

    /// The bytes the workspace has allocated, estimated before tear-down:
    /// the backend's disk usage beyond the base (nothing when the root is
    /// gone) plus the working-copy state. `None` when the backend could not
    /// measure a root that exists.
    async fn freed_bytes(&self, entry: &Arc<WorkspaceEntry>, root_exists: bool) -> Option<u64> {
        let tree = if root_exists {
            let record = entry.record();
            // The kept figure (#272) when it is current, else a walk.
            match self.db.workspace_disk(record.id) {
                Ok(Some(figure)) if !figure.incomplete && !self.disk.is_dirty(record.id) => {
                    figure.cost()
                }
                _ => self
                    .measure_disk(record.id, super::disk::Priority::Request)
                    .await?
                    .figure
                    .cost(),
            }
        } else {
            0
        };
        let state_dir = entry.paths.state_dir.clone();
        let state = tokio::task::spawn_blocking(move || allocated_bytes(&state_dir))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(0);
        Some(tree + state)
    }
}
