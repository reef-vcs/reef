//! Garbage collection (R1.17): explicit only (`Maintenance.Gc`, `reef ws
//! gc`); nothing runs it on a timer in Phase 1.
//!
//! Four steps per repository, in order, with creates of the repository held
//! off throughout (its create lock, exclusively):
//!
//! 1. **jj**: abandon the operations older than `keep_newer`, collect the
//!    operation store, rebuild the index, and run `git gc`
//!    ([`Manager::gc_jj`], which holds the repository's working copies
//!    still and moves them to the new head). A failure (`git` missing) is
//!    reported in [`GcReport::jj_error`] and the other steps still run.
//! 2. **Base cache**: entries no `active` or `releasing` workspace uses,
//!    least recently used first, down to `base_cache.max_entries`; one
//!    unused and last used before `keep_newer` goes regardless
//!    ([`crate::backend::BaseCache::evict`]).
//! 3. **Workspace rows**: `released` and `reclaimed` workspaces whose last
//!    reclaim is before `keep_newer`: their `wc/<id>` state directory, their
//!    leases, and the row. The `reclaims` log stays; `snapshots` rows are
//!    left to R1.16's reindex, which runs after GC.
//! 4. **Stores** (R1.20): sealed stores this repository built first that no
//!    loaded workspace mounts, that the `reef.toml` at no registered
//!    repository's default start revision (`main`, else the `default`
//!    workspace's change) names with its current lockfile, and that were
//!    built before `keep_newer`. If any repository's spec cannot be read,
//!    none are removed. Stores are shared by every repository, so the first
//!    report of a run also removes what no registered repository owns:
//!    unused stores a forgotten repository built, and `.failed-*` and
//!    `.replaced-*` directories from before `keep_newer`.
//!    Creates in every repository wait while stores are removed.
//!
//! `keep_newer` is `now - max(gc.undo_window, backup.retention while the
//! last backup is within it)` ([`keep_newer`]); every age cutoff uses it, so
//! nothing younger than the undo window, or than the last backup's
//! retention, is deleted. GC never touches a workspace directory or a base
//! an active workspace uses, and writes nothing of the user's jj workspaces
//! but the operation their working copy names (ADR 0040).

use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use reef_lib::telemetry::elapsed_ms;

use crate::backend::BaseCacheError;
use crate::config::{BackupSection, GcSection};
use crate::daemon::AppState;
use crate::db::{DbError, META_LAST_BACKUP_AT};
use crate::error::{Advice, Advise};
use crate::model::{RepoId, RepositoryRecord, Timestamp, WorkspaceState};
use crate::snapshot::index;
use crate::store::{FAILED, REPLACED, StoreKey};
use crate::workspace::{JjStep, Manager, measure};

/// What GC did to one repository; see `maintenance.proto`'s `GcReport`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcReport {
    /// The repository.
    pub repo_id: RepoId,
    /// Nothing newer than this was deleted.
    pub keep_newer: Timestamp,
    /// The operation store and git objects, before the jj step.
    pub jj_bytes_before: u64,
    /// The same, after (equal in a dry run).
    pub jj_bytes_after: u64,
    /// Rebuilding jj's index.
    pub reindex_ms: u64,
    /// Why the jj step stopped short, if it did.
    pub jj_error: Option<String>,
    /// Base cache entries removed.
    pub base_entries_removed: u32,
    /// What they had allocated.
    pub base_bytes_freed: u64,
    /// Ended workspace rows deleted.
    pub workspace_rows_removed: u32,
    /// What their `wc/<id>` state directories had allocated.
    pub wc_bytes_freed: u64,
    /// Stores removed (R1.20).
    pub stores_removed: u32,
    /// What they had allocated.
    pub store_bytes_freed: u64,
    /// The whole run for this repository.
    pub elapsed_ms: u64,
    /// Nothing was deleted; the counts are what would have gone.
    pub dry_run: bool,
}

/// Failure of a GC run, other than the jj step's (which is reported).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GcRunError {
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// The base cache could not be evicted from.
    #[error("evict from the base cache")]
    BaseCache(#[from] BaseCacheError),
    /// A workspace's state directory could not be removed.
    #[error("remove {}", path.display())]
    RemoveStateDir {
        /// The directory.
        path: std::path::PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A store directory could not be removed (R1.20).
    #[error("remove store {}", path.display())]
    RemoveStore {
        /// The directory.
        path: std::path::PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A blocking task panicked.
    #[error("a GC task failed")]
    Task(#[from] tokio::task::JoinError),
}

/// Only the database's advice; nothing else here has a known fix.
impl Advise for GcRunError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Db(error) => error.advice(),
            _ => None,
        }
    }
}

impl Classify for GcRunError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Db(error) => error.kind(),
            Self::BaseCache(error) => error.kind(),
            Self::RemoveStateDir { source, .. } | Self::RemoveStore { source, .. } => {
                reef_lib::error::classify_by_source(source)
            }
            Self::Task(_) => ErrorKind::Internal,
        }
    }
}

/// `now - max(undo_window, retention if last_backup_at + retention > now
/// else 0)`: the undo window, widened to the backup retention while the
/// last backup is within it (R1.18 writes `last_backup_at`).
pub fn keep_newer(
    gc: &GcSection,
    backup: &BackupSection,
    last_backup_at: Option<SystemTime>,
    now: SystemTime,
) -> SystemTime {
    let backup_recent = last_backup_at.is_some_and(|at| at + backup.retention > now);
    let window = if backup_recent {
        gc.undo_window.max(backup.retention)
    } else {
        gc.undo_window
    };
    // Not before the epoch, which jj's `git gc` call cannot express.
    now.checked_sub(window)
        .unwrap_or(UNIX_EPOCH)
        .max(UNIX_EPOCH)
}

/// `meta.last_backup_at`, if a backup was ever taken. A value that does not
/// parse is ignored with a warning.
fn last_backup_at(state: &AppState) -> Result<Option<SystemTime>, DbError> {
    let Some(text) = state.db.meta(META_LAST_BACKUP_AT)? else {
        return Ok(None);
    };
    if let Ok(millis) = text.parse::<u64>() {
        Ok(Some(UNIX_EPOCH + Duration::from_millis(millis)))
    } else {
        tracing::warn!(value = %text, "meta.last_backup_at is not unix milliseconds; ignored");
        Ok(None)
    }
}

/// Runs GC on `repo`, or on every registered repository, in registration
/// order. Holds nothing but each repository's create lock; the caller holds
/// the maintenance lock.
pub async fn run(
    state: &AppState,
    repo: Option<RepoId>,
    dry_run: bool,
) -> Result<Vec<GcReport>, GcRunError> {
    let records = match repo {
        Some(id) => vec![state.db.repository(id)?],
        None => state.db.repositories()?,
    };
    let last_backup = last_backup_at(state)?;
    let mut reports = Vec::with_capacity(records.len());
    for (index, record) in records.iter().enumerate() {
        let leftovers = index == 0;
        reports.push(run_repo(state, record, last_backup, dry_run, leftovers).await?);
    }
    Ok(reports)
}

#[tracing::instrument(skip_all, fields(repo = %record.id, dry_run = dry_run))]
async fn run_repo(
    state: &AppState,
    record: &RepositoryRecord,
    last_backup: Option<SystemTime>,
    dry_run: bool,
    store_leftovers: bool,
) -> Result<GcReport, GcRunError> {
    let begun = Instant::now();
    let id = record.id;
    let config = &state.config;
    let keep_newer = keep_newer(
        &config.file.gc,
        &config.file.backup,
        last_backup,
        SystemTime::now(),
    );
    let manager: &Manager = &state.manager;
    let create_lock = manager.repo_lock(id);
    let _no_creates = create_lock.write().await;

    // 1.
    let handle = state.repos.get(id);
    let jj = match &handle {
        None => JjStep {
            error: Some(format!("repository {id} is not open")),
            ..JjStep::default()
        },
        Some(handle) if dry_run => {
            let bytes = match handle.call(|head| head.repo.gc_measured_dirs()).await {
                Ok(dirs) => measure(&dirs).await,
                Err(_) => 0,
            };
            JjStep {
                bytes_before: bytes,
                bytes_after: bytes,
                ..JjStep::default()
            }
        }
        Some(handle) => manager.gc_jj(id, handle, keep_newer).await,
    };
    if let Some(error) = &jj.error {
        tracing::error!(error = %error, "the jj step of GC stopped short");
    }

    // 2.
    let keep: HashSet<String> = state
        .db
        .workspaces(
            Some(id),
            &[WorkspaceState::Active, WorkspaceState::Releasing],
        )?
        .into_iter()
        .map(|ws| ws.base_tree_id)
        .collect();
    let cache = manager.base_cache(id);
    let max_entries = config.file.base_cache.max_entries;
    let evicted =
        tokio::task::spawn_blocking(move || cache.evict(&keep, max_entries, keep_newer, dry_run))
            .await??;

    // 3.
    let (workspace_rows_removed, wc_bytes_freed) =
        remove_ended_rows(state, id, keep_newer, dry_run).await?;

    // 4.
    let (stores_removed, store_bytes_freed) =
        remove_stores(state, id, keep_newer, dry_run, store_leftovers).await?;

    if !dry_run && let Some(handle) = &handle {
        reindex(state, handle, id).await;
    }
    let report = GcReport {
        repo_id: id,
        keep_newer: Timestamp::from(keep_newer),
        jj_bytes_before: jj.bytes_before,
        jj_bytes_after: jj.bytes_after,
        reindex_ms: jj.reindex_ms,
        jj_error: jj.error,
        base_entries_removed: u32::try_from(evicted.len()).unwrap_or(u32::MAX),
        base_bytes_freed: evicted.iter().map(|entry| entry.bytes).sum(),
        workspace_rows_removed,
        wc_bytes_freed,
        stores_removed,
        store_bytes_freed,
        elapsed_ms: elapsed_ms(begun),
        dry_run,
    };
    tracing::info!(
        keep_newer = %report.keep_newer,
        abandoned_count = jj.operations_abandoned,
        jj_bytes_before = report.jj_bytes_before,
        jj_bytes_after = report.jj_bytes_after,
        reindex_ms = report.reindex_ms,
        base_entries_removed = report.base_entries_removed,
        base_bytes_freed = report.base_bytes_freed,
        workspace_rows_removed = report.workspace_rows_removed,
        wc_bytes_freed = report.wc_bytes_freed,
        stores_removed = report.stores_removed,
        store_bytes_freed = report.store_bytes_freed,
        elapsed_ms = report.elapsed_ms,
        "garbage collection finished"
    );
    Ok(report)
}

/// Step 3: deletes the rows of workspaces that ended before `keep_newer`,
/// with their leases and state directories. Returns the rows removed and
/// the bytes their state directories had allocated.
async fn remove_ended_rows(
    state: &AppState,
    id: RepoId,
    keep_newer: SystemTime,
    dry_run: bool,
) -> Result<(u32, u64), GcRunError> {
    let wc_root = state.config.paths.repo_dir(id).join("wc");
    let mut removed = 0u32;
    let mut bytes = 0;
    for row in state
        .db
        .ended_workspaces_before(id, Timestamp::from(keep_newer))?
    {
        // Only a state directory where the daemon puts them; anything else
        // the row names is left alone.
        if row.state_dir.starts_with(&wc_root) {
            bytes += measure(std::slice::from_ref(&row.state_dir)).await;
            if !dry_run {
                remove_dir(&row.state_dir).await?;
            }
        } else {
            tracing::warn!(
                workspace_id = %row.id,
                path = %row.state_dir.display(),
                "state directory is outside the daemon's; not removed"
            );
        }
        if !dry_run {
            state.db.delete_ended_workspace(row.id)?;
        }
        removed += 1;
    }
    Ok((removed, bytes))
}

/// Step 4: removes the stores `id` built first that nothing keeps, and with
/// `leftovers` the failed and replaced build directories older than
/// `keep_newer`. Returns how many went and the bytes they had allocated.
async fn remove_stores(
    state: &AppState,
    id: RepoId,
    keep_newer: SystemTime,
    dry_run: bool,
    leftovers: bool,
) -> Result<(u32, u64), GcRunError> {
    let manager: &Manager = &state.manager;
    let stores = manager.stores();
    // Read before the lock, which holds every create off.
    let mut keep: HashSet<StoreKey> = HashSet::new();
    for repo in state.db.repositories()? {
        let Some(handle) = state.repos.get(repo.id) else {
            tracing::warn!(other_repo = %repo.id, "repository is not open; keeping every store");
            return Ok((0, 0));
        };
        match manager.default_store_keys(&handle).await {
            Ok(keys) => keep.extend(keys),
            Err(error) => {
                tracing::warn!(other_repo = %repo.id, error = %ErrorChain(&error), "cannot read the repository's stores; keeping every store");
                return Ok((0, 0));
            }
        }
    }
    let _no_mounts = stores.mounting.write().await;
    keep.extend(manager.stores_in_use().into_keys());
    let cutoff = Timestamp::from(keep_newer);
    let registered: HashSet<RepoId> = state
        .db
        .repositories()?
        .into_iter()
        .map(|repo| repo.id)
        .collect();
    let mut removed = 0u32;
    let mut bytes = 0;
    for row in state.db.stores()? {
        let key = StoreKey {
            name: row.name.clone(),
            lockfile_hash: row.lockfile_hash.clone(),
        };
        let old = row.built_at.is_some_and(|at| at < cutoff);
        // A store whose first builder was forgotten is the first report's.
        let ours = row.repo_id == id || (leftovers && !registered.contains(&row.repo_id));
        if !ours || !row.sealed || !old || keep.contains(&key) || stores.is_building(&key) {
            continue;
        }
        // The whole store directory: `row.path` is its contents, one level
        // down (or, for a store sealed before that layout, the directory).
        let dir = stores.dir(&key);
        bytes += measure(std::slice::from_ref(&dir)).await;
        if !dry_run {
            state.db.delete_store(&key.name, &key.lockfile_hash)?;
            remove_store_dir(&dir).await?;
        }
        tracing::info!(store = %key.name, lockfile_hash = %key.lockfile_hash, "unused store removed");
        removed += 1;
    }
    if leftovers {
        let cutoff_ms = keep_newer
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis());
        for (_, file, path) in stores.leftovers() {
            let stamp = file
                .split_once(&format!(".{FAILED}"))
                .or_else(|| file.split_once(&format!(".{REPLACED}")))
                .and_then(|(_, millis)| millis.parse::<u128>().ok());
            if stamp.is_none_or(|millis| millis >= cutoff_ms) {
                continue;
            }
            bytes += measure(std::slice::from_ref(&path)).await;
            if !dry_run {
                remove_store_dir(&path).await?;
            }
            removed += 1;
        }
    }
    Ok((removed, bytes))
}

/// Removes a store directory, whose sealed files and directories are
/// read-only, tolerating one already gone.
async fn remove_store_dir(dir: &Path) -> Result<(), GcRunError> {
    match crate::backend::fs_util::remove_tree_async(dir).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(GcRunError::RemoveStore {
            path: dir.to_owned(),
            source,
        }),
    }
}

/// Removes a state directory, tolerating one already gone.
async fn remove_dir(dir: &Path) -> Result<(), GcRunError> {
    let path = dir.to_owned();
    let removed = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&path)).await?;
    match removed {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(GcRunError::RemoveStateDir {
            path: dir.to_owned(),
            source,
        }),
    }
}

/// R1.16's reindex after the jj step rewrote the operation ids its rows
/// name. A failure is logged; the next daemon start reindexes again. Runs
/// inside [`run_repo`]'s span, which carries the repository.
async fn reindex(state: &AppState, handle: &crate::repo::RepoHandle, id: RepoId) {
    let limit = usize::try_from(state.config.file.gc.reindex_limit).unwrap_or(usize::MAX);
    match index::reindex(&state.db, handle, id, limit).await {
        Ok(report) => tracing::info!(
            added_count = report.added,
            removed_count = report.removed,
            truncated = report.truncated,
            "snapshot index reconciled after GC"
        ),
        Err(error) => tracing::error!(
            error = %ErrorChain(&error),
            "cannot reconcile the snapshot index after GC"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_hours(24);

    fn sections(undo: Duration, retention: Duration) -> (GcSection, BackupSection) {
        let gc = GcSection {
            undo_window: undo,
            ..GcSection::default()
        };
        let backup = BackupSection {
            retention,
            ..BackupSection::default()
        };
        (gc, backup)
    }

    #[test]
    fn keep_newer_is_the_undo_window_without_a_recent_backup() {
        let now = UNIX_EPOCH + 100 * DAY;
        let (gc, backup) = sections(7 * DAY, 30 * DAY);
        assert_eq!(keep_newer(&gc, &backup, None, now), now - 7 * DAY);
        let long_ago = Some(now - 31 * DAY);
        assert_eq!(keep_newer(&gc, &backup, long_ago, now), now - 7 * DAY);
    }

    #[test]
    fn a_recent_backup_widens_it_to_the_retention() {
        let now = UNIX_EPOCH + 100 * DAY;
        let (gc, backup) = sections(7 * DAY, 30 * DAY);
        let yesterday = Some(now - DAY);
        assert_eq!(keep_newer(&gc, &backup, yesterday, now), now - 30 * DAY);
        // A retention shorter than the undo window never narrows it.
        let (gc, backup) = sections(7 * DAY, DAY / 2);
        let hour_ago = Some(now - Duration::from_hours(1));
        assert_eq!(keep_newer(&gc, &backup, hour_ago, now), now - 7 * DAY);
    }

    #[test]
    fn zero_window_is_now_and_huge_windows_saturate() {
        let now = UNIX_EPOCH + DAY;
        let (gc, backup) = sections(Duration::ZERO, 30 * DAY);
        assert_eq!(keep_newer(&gc, &backup, None, now), now);
        let (gc, backup) = sections(1000 * DAY, 30 * DAY);
        assert_eq!(keep_newer(&gc, &backup, None, now), UNIX_EPOCH);
    }
}
