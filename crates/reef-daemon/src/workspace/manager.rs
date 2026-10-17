//! The [`Manager`]: the loaded workspaces, lookups, list and inspect,
//! reloading at daemon start, and repository registration.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use futures::FutureExt as _;
use reef_lib::error::ErrorChain;
use reef_lib::jj::{ForgetWorkspaceError, UserSettings};
use tokio::sync::Semaphore;

use super::{Inspected, ManagerError, WorkspaceEntry};
use crate::backend::fs_util::remove_tree_async;
use crate::backend::{BackendKind, BaseCache, WorkspacePaths, backend_for};
use crate::config::Config;
use crate::db::Db;
use crate::model::{
    LeaseEnd, LeaseRecord, ReclaimReason, ReclaimRecord, RepoId, RepositoryRecord, Timestamp,
    WorkspaceId, WorkspaceRecord, WorkspaceState,
};
use crate::repo::{RepoHandle, RepoRegistry};
use crate::snapshot::SnapshotKind;

/// The `reclaims.detail` of a workspace whose directory was gone at start.
pub(super) const DIRECTORY_MISSING: &str = "directory missing at start";

/// Owns every loaded workspace and runs the workspace operations. One per
/// daemon, in [`crate::AppState::manager`].
pub struct Manager {
    pub(super) config: Config,
    pub(super) db: Arc<Db>,
    pub(super) repos: Arc<RepoRegistry>,
    pub(super) settings: UserSettings,
    /// One base cache per repository, so concurrent creates share its
    /// per-tree locks.
    bases: Mutex<HashMap<RepoId, Arc<BaseCache>>>,
    /// One lock per repository: creates hold it shared from before they
    /// read the repository's row until their own row is written, and
    /// forgetting the repository holds it exclusively, so a forget never
    /// runs under a create that has no row yet.
    repo_locks: Mutex<HashMap<RepoId, Arc<tokio::sync::RwLock<()>>>>,
    entries: RwLock<HashMap<WorkspaceId, Arc<WorkspaceEntry>>>,
    /// Bounds concurrent scans at `watcher.max_concurrent_scans`.
    pub(super) scans: Arc<Semaphore>,
}

impl std::fmt::Debug for Manager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Manager")
            .field("loaded", &self.read_entries().len())
            .finish_non_exhaustive()
    }
}

impl Manager {
    /// A manager with nothing loaded yet; [`Manager::load_active`] loads.
    pub fn new(
        config: Config,
        db: Arc<Db>,
        repos: Arc<RepoRegistry>,
        settings: UserSettings,
    ) -> Manager {
        let scans = Arc::new(Semaphore::new(config.max_concurrent_scans().max(1)));
        Manager {
            config,
            db,
            repos,
            settings,
            bases: Mutex::new(HashMap::new()),
            repo_locks: Mutex::new(HashMap::new()),
            entries: RwLock::new(HashMap::new()),
            scans,
        }
    }

    /// The loaded workspace with this id: every `active` workspace whose
    /// repository is open.
    pub fn entry(&self, id: WorkspaceId) -> Option<Arc<WorkspaceEntry>> {
        self.read_entries().get(&id).cloned()
    }

    /// How many workspaces are loaded.
    pub fn loaded(&self) -> usize {
        self.read_entries().len()
    }

    fn read_entries(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, HashMap<WorkspaceId, Arc<WorkspaceEntry>>> {
        self.entries.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn insert_entry(&self, id: WorkspaceId, entry: Arc<WorkspaceEntry>) {
        self.entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, entry);
    }

    pub(super) fn remove_entry(&self, id: WorkspaceId) {
        self.entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }

    /// The repository's base cache.
    pub fn base_cache(&self, repo: RepoId) -> Arc<BaseCache> {
        let mut bases = self.bases.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(bases.entry(repo).or_insert_with(|| {
            Arc::new(BaseCache::new(
                Arc::clone(&self.db),
                &self.config.paths.repo_dir(repo),
                repo,
            ))
        }))
    }

    /// The repository's create/forget lock (see `repo_locks`).
    pub(super) fn repo_lock(&self, repo: RepoId) -> Arc<tokio::sync::RwLock<()>> {
        let mut locks = self
            .repo_locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::clone(locks.entry(repo).or_default())
    }

    /// The repository's thread.
    pub(super) fn handle(&self, repo: RepoId) -> Result<RepoHandle, ManagerError> {
        self.repos.get(repo).ok_or(ManagerError::RepoNotOpen(repo))
    }

    /// The loaded entry of an `active` workspace, or why there is none.
    pub(super) fn active_entry(
        &self,
        id: WorkspaceId,
    ) -> Result<Arc<WorkspaceEntry>, ManagerError> {
        if let Some(entry) = self.entry(id) {
            let state = entry.record().state;
            if state != WorkspaceState::Active {
                return Err(ManagerError::NotActive { id, state });
            }
            return Ok(entry);
        }
        let record = self.db.workspace(id)?;
        if record.state != WorkspaceState::Active {
            return Err(ManagerError::NotActive {
                id,
                state: record.state,
            });
        }
        if self.repos.get(record.repo_id).is_some() {
            // Skipped at start (an unknown backend name), or a create that
            // has written its row and not yet its entry.
            return Err(ManagerError::NotLoaded(id));
        }
        Err(ManagerError::RepoNotOpen(record.repo_id))
    }

    /// The repository a request's `repo` field names: a repo id, a unique
    /// id prefix, or an absolute path inside a registered root.
    pub fn resolve_repo(&self, text: &str) -> Result<RepositoryRecord, ManagerError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(ManagerError::RepoRequired);
        }
        let path = Path::new(text);
        if path.is_absolute() {
            // Registered roots are canonical; a path that does not exist is
            // compared as given.
            let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
            return self.db.repository_containing(&path)?.ok_or_else(|| {
                crate::db::DbError::NotFound {
                    what: "repository containing",
                    id: path.display().to_string(),
                }
                .into()
            });
        }
        Ok(self.db.repository_by_prefix(text)?)
    }

    /// The workspace a request names by id or unique prefix, in the
    /// repository `repo` names, or in any repository when `repo` is empty.
    pub fn resolve_workspace(
        &self,
        repo: &str,
        workspace: &str,
    ) -> Result<WorkspaceRecord, ManagerError> {
        let workspace = workspace.trim();
        if workspace.is_empty() {
            return Err(ManagerError::WorkspaceRequired);
        }
        let repo = if repo.trim().is_empty() {
            None
        } else {
            Some(self.resolve_repo(repo)?.id)
        };
        Ok(self.db.workspace_by_prefix(repo, workspace)?)
    }

    /// Registers the repository containing `path`; see
    /// [`RepoRegistry::register`].
    pub async fn register_repository(
        &self,
        path: PathBuf,
    ) -> Result<(RepositoryRecord, bool), ManagerError> {
        let repos = Arc::clone(&self.repos);
        let db = Arc::clone(&self.db);
        let settings = self.settings.clone();
        Ok(tokio::task::spawn_blocking(move || repos.register(&db, &path, &settings)).await??)
    }

    /// Forgets a repository (refused while any of its workspaces is active),
    /// stops its thread, and removes its state directory (base cache,
    /// leftover workspace trash) in the background.
    pub async fn forget_repository(&self, id: RepoId) -> Result<RepositoryRecord, ManagerError> {
        let lock = self.repo_lock(id);
        let _exclusive = lock.write().await;
        let record = self.repos.forget(&self.db, id)?;
        self.bases
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        let repo_dir = self.config.paths.repo_dir(id);
        match remove_tree_async(&repo_dir).await {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                repo = %id,
                path = %repo_dir.display(),
                %error,
                "cannot remove the forgotten repository's state directory"
            ),
        }
        Ok(record)
    }

    /// Workspaces in `repo` (every repository if `None`), in one of
    /// `states` (any if empty), held by `owner` (anyone if `None`), oldest
    /// first, each with its unended lease. Reads the database only.
    pub fn list(
        &self,
        repo: Option<RepoId>,
        states: &[WorkspaceState],
        owner: Option<&str>,
    ) -> Result<Vec<(WorkspaceRecord, Option<LeaseRecord>)>, ManagerError> {
        self.db
            .workspaces(repo, states)?
            .into_iter()
            .filter(|record| owner.is_none_or(|owner| record.owner == owner))
            .map(|record| {
                let lease = self.lease_of(&record)?;
                Ok((record, lease))
            })
            .collect()
    }

    /// The workspace's unended lease.
    pub fn lease_of(&self, record: &WorkspaceRecord) -> Result<Option<LeaseRecord>, ManagerError> {
        Ok(record
            .lease_id
            .map(|lease| self.db.lease(lease))
            .transpose()?)
    }

    /// One workspace from the database, with its disk usage from its
    /// backend and the other active workspaces on its change. With
    /// `snapshot_first`, takes an explicit snapshot before reading if the
    /// workspace is active (a released one has nothing to snapshot).
    pub async fn inspect(
        &self,
        id: WorkspaceId,
        snapshot_first: bool,
    ) -> Result<Inspected, ManagerError> {
        let active = self.db.workspace(id)?.state == WorkspaceState::Active;
        let op_id = if snapshot_first && active {
            self.snapshot(id, SnapshotKind::Explicit, "").await?.op_id
        } else {
            None
        };
        let record = self.db.workspace(id)?;
        let lease = self.lease_of(&record)?;
        let disk_usage = match self.entry(id) {
            Some(entry) if record.state == WorkspaceState::Active => {
                self.disk_usage(entry, &record).await
            }
            _ => None,
        };
        let other_workspaces_on_change = self
            .db
            .workspaces(Some(record.repo_id), &[WorkspaceState::Active])?
            .into_iter()
            .filter(|other| other.id != record.id && other.change_id == record.change_id)
            .map(|other| other.id)
            .collect();
        Ok(Inspected {
            record,
            lease,
            disk_usage,
            other_workspaces_on_change,
            op_id,
        })
    }

    /// The backend's estimate, or `None` (logged) when the base is no
    /// longer cached or the walk fails: inspect still answers.
    pub(super) async fn disk_usage(
        &self,
        entry: Arc<WorkspaceEntry>,
        record: &WorkspaceRecord,
    ) -> Option<crate::backend::DiskUsage> {
        let base = match self.base_cache(record.repo_id).get(&record.base_tree_id) {
            Ok(Some(base)) => base,
            Ok(None) => {
                tracing::warn!(workspace_id = %record.id, "base is no longer cached; no disk usage");
                return None;
            }
            Err(error) => {
                tracing::warn!(workspace_id = %record.id, error = %ErrorChain(&error), "cannot read the base");
                return None;
            }
        };
        let measured =
            tokio::task::spawn_blocking(move || entry.backend.disk_usage(&base, &entry.paths))
                .await;
        match measured {
            Ok(Ok(usage)) => Some(usage),
            Ok(Err(error)) => {
                tracing::warn!(workspace_id = %record.id, error = %ErrorChain(&error), "cannot measure disk usage");
                None
            }
            Err(error) => {
                tracing::warn!(workspace_id = %record.id, %error, "disk usage task failed");
                None
            }
        }
    }

    /// Loads every `active` workspace at daemon start (a `releasing` one,
    /// whose release was interrupted, goes back to `active` first): reopens
    /// its jj handle on its repository's thread and re-selects its backend
    /// by the stored name, without probing. A workspace whose directory is
    /// gone is reclaimed (`reclaims.detail` = "directory missing at start"),
    /// since nothing can be snapshotted. One whose jj handle cannot be
    /// reopened is loaded without it ([`ManagerError::NotLoaded`] for
    /// snapshots; a release without a final snapshot removes it). Workspaces of a repository that failed to open are
    /// skipped. Returns how many were loaded.
    ///
    /// R1.18 adds: with `meta.rematerialize_pending` set, a missing
    /// directory is rematerialized instead.
    pub async fn load_active(&self) -> Result<usize, ManagerError> {
        let rows = self
            .db
            .workspaces(None, &[WorkspaceState::Active, WorkspaceState::Releasing])?;
        let mut loaded = 0;
        for mut record in rows {
            let id = record.id;
            if record.state == WorkspaceState::Releasing {
                tracing::warn!(workspace_id = %id, "release was interrupted; workspace is active again");
                self.db.set_workspace_state(id, WorkspaceState::Active)?;
                record.state = WorkspaceState::Active;
            }
            let Some(handle) = self.repos.get(record.repo_id) else {
                tracing::error!(
                    workspace_id = %id,
                    repo = %record.repo_id,
                    "the workspace's repository is not open; workspace not loaded"
                );
                continue;
            };
            if std::fs::symlink_metadata(&record.path).is_err() {
                self.reclaim_missing(&record, &handle).await;
                continue;
            }
            let kind = match record.backend.parse::<BackendKind>() {
                Ok(kind) => kind,
                Err(error) => {
                    tracing::error!(workspace_id = %id, error = %error, "workspace not loaded");
                    continue;
                }
            };
            let paths = WorkspacePaths::at(id, record.path.clone(), record.state_dir.clone());
            let jj = self.reopen(&handle, &paths).await;
            let entry = WorkspaceEntry {
                record: Mutex::new(record),
                paths,
                backend: backend_for(kind),
                jj: Arc::new(Mutex::new(jj)),
                repo: handle,
                scan_lock: tokio::sync::Mutex::new(()),
            };
            self.insert_entry(id, Arc::new(entry));
            loaded += 1;
        }
        Ok(loaded)
    }

    /// The workspace's jj handle, loaded at the operation its working copy
    /// last recorded so its next snapshot is not stale; `None` (logged) if
    /// it cannot be reopened.
    async fn reopen(
        &self,
        handle: &RepoHandle,
        paths: &WorkspacePaths,
    ) -> Option<reef_lib::jj::Repo> {
        let id = paths.id;
        let root = paths.root.clone();
        let state_dir = paths.state_dir.clone();
        let opened = handle
            .call_async(move |head| {
                async move {
                    let mut ws = head.repo.open_workspace(&root, &state_dir)?;
                    if let Err(error) = ws.reload_at_working_copy_op().await {
                        tracing::warn!(
                            workspace_id = %id,
                            error = %ErrorChain(&error),
                            "cannot load the workspace at its working copy's operation; using head"
                        );
                    }
                    Ok::<_, ManagerError>(ws)
                }
                .boxed_local()
            })
            .await;
        match opened {
            Ok(Ok(ws)) => Some(ws),
            Ok(Err(error)) => {
                tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "cannot reopen the workspace");
                None
            }
            Err(error) => {
                tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "cannot reopen the workspace");
                None
            }
        }
    }

    /// Reclaims a workspace whose directory is gone: forgets its jj
    /// workspace, removes its state directory, ends its lease, and logs the
    /// reclaim. Failures are logged; the next start retries what is left.
    async fn reclaim_missing(&self, record: &WorkspaceRecord, handle: &RepoHandle) {
        let id = record.id;
        tracing::error!(
            workspace_id = %id,
            path = %record.path.display(),
            "workspace directory is missing; reclaiming it"
        );
        if let Err(error) = forget_jj_workspace(
            handle,
            id,
            "reef: release workspace {id} (directory missing)",
        )
        .await
        {
            tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "cannot forget the jj workspace");
        }
        remove_state_dir(&record.state_dir).await;
        let reclaim = ReclaimRecord {
            id: 0,
            repo_id: record.repo_id,
            workspace_id: id,
            reason: ReclaimReason::Gc,
            final_op_id: None,
            snapshot_taken: false,
            freed_bytes: None,
            at: Timestamp::now(),
            detail: Some(DIRECTORY_MISSING.to_owned()),
        };
        if let Err(error) =
            self.db
                .finish_workspace(id, WorkspaceState::Reclaimed, LeaseEnd::Expired, &reclaim)
        {
            tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "cannot record the reclaim");
        }
    }
}

/// Forgets the jj workspace `reef-<id>` on the repository thread with
/// `description` (`{id}` replaced), tolerating one that is already gone.
pub(super) async fn forget_jj_workspace(
    handle: &RepoHandle,
    id: WorkspaceId,
    description: &str,
) -> Result<(), ManagerError> {
    let name = id.jj_workspace_name();
    let description = description.replace("{id}", &id.as_hex());
    let forgotten = handle
        .call_async(move |head| {
            async move { head.repo.forget_workspace(&name, &description).await }.boxed_local()
        })
        .await;
    match forgotten? {
        Ok(_) | Err(ForgetWorkspaceError::NoSuchWorkspace(_)) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Removes a workspace's working-copy state directory, tolerating one that
/// is already gone; other failures are logged.
pub(super) async fn remove_state_dir(state_dir: &Path) {
    let dir = state_dir.to_owned();
    let removed = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&dir)).await;
    match removed {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(Err(error)) => tracing::warn!(
            path = %state_dir.display(),
            %error,
            "cannot remove the working-copy state directory"
        ),
        Err(error) => tracing::warn!(%error, "removing the working-copy state directory failed"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use reef_lib::error::{Classify as _, ErrorKind};

    use super::*;
    use crate::config::Paths;

    fn manager(state: &Path) -> Arc<Manager> {
        let config = Config::with_defaults(Paths::in_state_dir(state).unwrap());
        let settings = config.jj_settings().unwrap();
        Arc::new(Manager::new(
            config,
            Arc::new(Db::open_in_memory().unwrap()),
            Arc::new(RepoRegistry::new()),
            settings,
        ))
    }

    #[tokio::test]
    async fn forget_waits_for_creates_in_flight() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = manager(tmp.path());
        let repo = RepoId::generate();
        // What a create holds from before it reads the repository's row
        // until it has written its own.
        let lock = manager.repo_lock(repo);
        let create = lock.read().await;

        let forgetting = tokio::spawn({
            let manager = Arc::clone(&manager);
            async move { manager.forget_repository(repo).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!forgetting.is_finished(), "forget waits for the create");
        drop(create);
        let error = forgetting.await.unwrap().unwrap_err();
        assert_eq!(
            error.kind(),
            ErrorKind::NotFound,
            "then runs (no such repository here)"
        );
    }
}
