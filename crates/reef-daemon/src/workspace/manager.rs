//! The [`Manager`]: the loaded workspaces, lookups, list and inspect,
//! reloading at daemon start, and repository registration.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use futures::FutureExt as _;
use reef_lib::error::ErrorChain;
use reef_lib::jj::settings::Fsmonitor;
use reef_lib::jj::{CommitIdHex, ForgetWorkspaceError, ReadonlyRepo, UserSettings};

use super::{Inspected, ManagerError, WorkspaceEntry};
use crate::backend::fs_util::remove_tree_async;
use crate::backend::{BackendKind, BaseCache, WorkspacePaths, backend_for};
use crate::config::Config;
use crate::db::{Db, DbError, META_REMATERIALIZE_PENDING, WorkspaceFilter};
use crate::lease::LeaseManager;
use crate::model::{
    LeaseEnd, LeaseRecord, ReclaimReason, ReclaimRecord, RepoId, RepositoryRecord, Timestamp,
    WatcherKind, WorkspaceId, WorkspaceRecord, WorkspaceState,
};
use crate::repo::{Registered, RepoHandle, RepoRegistry};
use crate::snapshot::index;
use crate::snapshot::{Scheduler, SnapshotKind, TrackReason};
use crate::store::StoreManager;
use crate::watcher::{WatchError, select_watcher};
use reef_api::proto::v1::register_request::IfNotJj;

/// The `reclaims.detail` of a workspace whose directory was gone at start.
pub(super) const DIRECTORY_MISSING: &str = "directory missing at start";

/// Owns every loaded workspace and runs the workspace operations. One per
/// daemon, in [`crate::AppState::manager`].
pub struct Manager {
    pub(super) config: Config,
    pub(super) db: Arc<Db>,
    pub(super) repos: Arc<RepoRegistry>,
    pub(super) settings: UserSettings,
    /// [`Manager::settings`] with `fsmonitor.backend = "watchman"`, for the
    /// working copies of Watchman workspaces (R1.14). `None` in a build
    /// without the `watchman` feature.
    watchman_settings: Option<UserSettings>,
    /// One base cache per repository, so concurrent creates share its
    /// per-tree locks.
    bases: Mutex<HashMap<RepoId, Arc<BaseCache>>>,
    /// One lock per repository: creates hold it shared from before they
    /// read the repository's row until their own row is written, and
    /// forgetting the repository holds it exclusively, so a forget never
    /// runs under a create that has no row yet.
    repo_locks: Mutex<HashMap<RepoId, Arc<tokio::sync::RwLock<()>>>>,
    entries: RwLock<HashMap<WorkspaceId, Arc<WorkspaceEntry>>>,
    /// Schedules snapshots and bounds concurrent scans at
    /// `watcher.max_concurrent_scans`.
    pub(super) scheduler: Scheduler,
    /// Issues and renews leases, and reclaims expired ones (R1.15).
    pub(super) leases: Arc<LeaseManager>,
    /// The shared dependency stores (R1.20).
    pub(super) stores: Arc<StoreManager>,
}

impl std::fmt::Debug for Manager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Manager")
            .field("loaded", &self.read_entries().len())
            .finish_non_exhaustive()
    }
}

impl Manager {
    /// A manager with nothing loaded yet; [`Manager::load_active`] loads,
    /// [`Scheduler::start`] starts automatic snapshots, and
    /// [`LeaseManager::start_expiry_loop`] starts reclaiming expired leases.
    pub fn new(
        config: Config,
        db: Arc<Db>,
        repos: Arc<RepoRegistry>,
        settings: UserSettings,
    ) -> Arc<Manager> {
        let watchman_settings = match config.jj_settings_for(Fsmonitor::Watchman) {
            Ok(settings) => Some(settings),
            Err(reef_lib::jj::settings::SettingsError::FeatureDisabled(_)) => None,
            Err(error) => {
                tracing::error!(error = %ErrorChain(&error), "cannot build the jj settings for Watchman workspaces");
                None
            }
        };
        Arc::new_cyclic(|manager| {
            let scheduler = Scheduler::new(
                &config.file.watcher,
                config.max_concurrent_scans(),
                manager.clone(),
            );
            let leases = Arc::new(LeaseManager::new(
                config.file.lease.clone(),
                Arc::clone(&db),
                manager.clone(),
            ));
            let stores = Arc::new(StoreManager::new(&config.paths.state_dir, Arc::clone(&db)));
            Manager {
                config,
                db,
                repos,
                settings,
                watchman_settings,
                bases: Mutex::new(HashMap::new()),
                repo_locks: Mutex::new(HashMap::new()),
                entries: RwLock::new(HashMap::new()),
                scheduler,
                leases,
                stores,
            }
        })
    }

    /// The snapshot scheduler.
    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// The lease manager.
    pub fn leases(&self) -> &Arc<LeaseManager> {
        &self.leases
    }

    /// The metadata database.
    pub(crate) fn db(&self) -> &Db {
        &self.db
    }

    /// Replaces a loaded workspace's snapshot spec and re-watches it with
    /// the new skip list. Create sets the spec from `reef.toml` (R1.19);
    /// tests set it here.
    #[cfg(any(test, feature = "testing"))]
    pub async fn set_snapshot_spec(
        &self,
        id: WorkspaceId,
        spec: super::SnapshotSpec,
    ) -> Result<(), ManagerError> {
        let entry = self.entry(id).ok_or(ManagerError::NotLoaded(id))?;
        *entry.spec.lock().unwrap_or_else(PoisonError::into_inner) = spec;
        self.scheduler.rewatch(id).await;
        Ok(())
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

    pub(super) fn read_entries(
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

    /// The repository's create/forget lock (see `repo_locks`); GC holds it
    /// exclusively too (R1.17).
    pub(crate) fn repo_lock(&self, repo: RepoId) -> Arc<tokio::sync::RwLock<()>> {
        let mut locks = self
            .repo_locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::clone(locks.entry(repo).or_default())
    }

    /// The jj settings a working copy watched by `watcher` is loaded with.
    pub(super) fn wc_settings(&self, watcher: WatcherKind) -> Result<UserSettings, ManagerError> {
        match watcher {
            WatcherKind::Notify => Ok(self.settings.clone()),
            WatcherKind::Watchman => self.watchman_settings.clone().ok_or(ManagerError::Watcher(
                WatchError::FeatureDisabled("watchman"),
            )),
        }
    }

    /// The watcher a new workspace of `repo` gets; see [`select_watcher`].
    pub(super) fn watcher_for(&self, repo: &RepositoryRecord) -> Result<WatcherKind, ManagerError> {
        Ok(select_watcher(
            &self.config.file.watcher,
            repo.tracked_files,
        )?)
    }

    /// Counts a registered, open repository's tracked files if they were
    /// not counted yet (at registration, or at the first start after
    /// R1.14), stores the count, and logs which watcher its new workspaces
    /// get. Returns the record with the count. Failures are logged; the
    /// repository is then treated as uncounted (below the threshold).
    pub async fn prepare_watcher(&self, mut record: RepositoryRecord) -> RepositoryRecord {
        let id = record.id;
        if record.tracked_files.is_none() {
            let counted = match self.repos.get(id) {
                Some(handle) => handle
                    .call(|head| head.repo.tracked_file_count())
                    .await
                    .map_err(ManagerError::from)
                    .and_then(|counted| counted.map_err(ManagerError::from)),
                None => Err(ManagerError::RepoNotOpen(id)),
            };
            match counted {
                Ok(count) => match self.db.set_repository_tracked_files(id, count) {
                    Ok(()) => record.tracked_files = Some(count),
                    Err(error) => {
                        tracing::error!(repo = %id, error = %ErrorChain(&error), "cannot record the tracked file count");
                    }
                },
                Err(error) => {
                    tracing::error!(repo = %id, error = %ErrorChain(&error), "cannot count the repository's tracked files");
                }
            }
        }
        let backend = self.config.file.watcher.backend.as_str();
        let threshold = self.config.file.watcher.watchman_threshold_files;
        match self.watcher_for(&record) {
            Ok(kind) => tracing::info!(
                repo = %id,
                tracked_files_count = record.tracked_files,
                watchman_threshold_files = threshold,
                backend,
                watcher = kind.as_str(),
                "watcher selected for the repository's new workspaces"
            ),
            Err(error) => tracing::warn!(
                repo = %id,
                tracked_files_count = record.tracked_files,
                backend,
                error = %ErrorChain(&error),
                "no watcher for the repository's new workspaces; creating one fails until this is fixed"
            ),
        }
        record
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
        self.loaded_entry(id, false)
    }

    /// The loaded entry of an `active` workspace (or a `releasing` one, with
    /// `releasing_ok`), or why there is none.
    pub(super) fn loaded_entry(
        &self,
        id: WorkspaceId,
        releasing_ok: bool,
    ) -> Result<Arc<WorkspaceEntry>, ManagerError> {
        if let Some(entry) = self.entry(id) {
            let record = entry.record();
            let state = record.state;
            let allowed = state == WorkspaceState::Active
                || (releasing_ok && state == WorkspaceState::Releasing);
            if !allowed {
                return Err(ManagerError::NotActive {
                    id,
                    repo: record.repo_id,
                    state,
                });
            }
            return Ok(entry);
        }
        let record = self.db.workspace(id)?;
        if record.state != WorkspaceState::Active {
            return Err(ManagerError::NotActive {
                id,
                repo: record.repo_id,
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
            return self
                .db
                .repository_containing(&path)?
                .ok_or(ManagerError::NotRegistered(path));
        }
        Ok(self.db.repository_by_prefix(text)?)
    }

    /// The workspace a request names by id or unique prefix, in the
    /// repository `repo` names, or in any repository when `repo` is empty.
    /// An absolute path names the active workspace whose directory is it
    /// or contains it (R1.26: `reef here` from a shell that `cd`'d in).
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
        let path = Path::new(workspace);
        if path.is_absolute() {
            // Workspace paths are canonical; a path that does not exist is
            // compared as given.
            let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
            return self
                .db
                .active_workspace_containing(&path)?
                .filter(|record| repo.is_none_or(|repo| record.repo_id == repo))
                .ok_or_else(|| {
                    DbError::NotFound {
                        what: "workspace containing",
                        id: path.display().to_string(),
                    }
                    .into()
                });
        }
        Ok(self.db.workspace_by_prefix(repo, workspace)?)
    }

    /// Registers the repository containing `path`; see
    /// [`RepoRegistry::register`].
    pub async fn register_repository(
        &self,
        path: PathBuf,
        if_not_jj: IfNotJj,
    ) -> Result<Registered, ManagerError> {
        let repos = Arc::clone(&self.repos);
        let db = Arc::clone(&self.db);
        let settings = self.settings.clone();
        let registered =
            tokio::task::spawn_blocking(move || repos.register(&db, &path, &settings, if_not_jj))
                .await??;
        Ok(Registered {
            record: self.prepare_watcher(registered.record).await,
            ..registered
        })
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
    /// first, each with its lease ([`Manager::lease_of`]). Reads the
    /// database only.
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

    /// One page of workspaces with their leases, newest first, after
    /// `after` (see [`crate::db::Db::workspaces_page`]).
    pub fn list_page(
        &self,
        filter: &WorkspaceFilter<'_>,
        after: Option<(Timestamp, WorkspaceId)>,
        limit: usize,
    ) -> Result<Vec<(WorkspaceRecord, Option<LeaseRecord>)>, ManagerError> {
        self.db
            .workspaces_page(filter, after, limit)?
            .into_iter()
            .map(|record| {
                let lease = self.lease_of(&record)?;
                Ok((record, lease))
            })
            .collect()
    }

    /// The workspace's lease: its unended one, or for a released or
    /// reclaimed workspace the one it ended with.
    pub fn lease_of(&self, record: &WorkspaceRecord) -> Result<Option<LeaseRecord>, ManagerError> {
        Ok(match record.lease_id {
            Some(lease) => Some(self.db.lease(lease)?),
            None if record.state.is_terminal() => self.db.last_lease(record.id)?,
            None => None,
        })
    }

    /// One workspace from the database, with its disk usage from its
    /// backend, the other active workspaces on its change, and its indexed
    /// snapshots (R1.16). With
    /// `snapshot_first`, takes an explicit snapshot before reading if the
    /// workspace is active (a released one has nothing to snapshot).
    /// Without `measure_disk`, leaves the disk usage out rather than walk
    /// the directory (`reef here`, R1.26).
    pub async fn inspect(
        &self,
        id: WorkspaceId,
        snapshot_first: bool,
        measure_disk: bool,
    ) -> Result<Inspected, ManagerError> {
        let active = self.db.workspace(id)?.state == WorkspaceState::Active;
        let op_id = if snapshot_first && active {
            let snapshot = self.snapshot(id, SnapshotKind::Explicit, "").await?;
            snapshot.changed.then(|| snapshot.op_id.to_string())
        } else {
            None
        };
        let record = self.db.workspace(id)?;
        let lease = self.lease_of(&record)?;
        let disk_usage = match self.entry(id) {
            Some(entry) if measure_disk && record.state == WorkspaceState::Active => {
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
        let (snapshot_count, local_snapshot_count) = index::counts(&self.db, id)?;
        let recent_snapshots = index::recent(&self.db, id, index::RECENT_LIMIT)?;
        let stores = self
            .entry(id)
            .map(|entry| entry.stores())
            .unwrap_or_default();
        Ok(Inspected {
            record,
            lease,
            disk_usage,
            other_workspaces_on_change,
            op_id,
            snapshot_count,
            local_snapshot_count,
            recent_snapshots,
            stores,
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
        // The stores mounted now: a move (R1.06) may have changed them.
        let paths = entry
            .paths
            .clone()
            .with_store_mounts(entry.stores().iter().map(|store| store.mount.as_str()));
        let measured =
            tokio::task::spawn_blocking(move || entry.backend.disk_usage(&base, &paths)).await;
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
    /// snapshots; a release without a final snapshot removes it).
    /// Workspaces of a repository that failed to open are skipped. Returns
    /// how many were loaded.
    ///
    /// After a restore (`meta.rematerialize_pending`, R1.18), every one is
    /// rematerialized first (`workspace/rematerialize.rs`): a missing
    /// directory is rebuilt, and every working copy is recreated, since a
    /// surviving one may name an operation the restore took back. One that
    /// fails or cannot be loaded is logged and left unloaded, not
    /// reclaimed, and the flag stays set so the next start tries again. It
    /// is cleared once every one succeeds.
    pub async fn load_active(&self) -> Result<usize, ManagerError> {
        let rows = self
            .db
            .workspaces(None, &[WorkspaceState::Active, WorkspaceState::Releasing])?;
        let rematerialize = self.db.meta(META_REMATERIALIZE_PENDING)?.is_some();
        let mut rematerialize_failed = false;
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
                rematerialize_failed |= rematerialize;
                continue;
            };
            let missing = std::fs::symlink_metadata(&record.path).is_err();
            if rematerialize {
                if let Err(error) = self.rematerialize(&mut record, &handle).await {
                    tracing::error!(
                        workspace_id = %id,
                        error = %ErrorChain(&error),
                        "cannot rematerialize the workspace after a restore; it is not loaded"
                    );
                    rematerialize_failed = true;
                    continue;
                }
            } else if missing {
                self.reclaim_missing(&record, &handle).await;
                continue;
            }
            let kind = match record.backend.parse::<BackendKind>() {
                Ok(kind) => kind,
                Err(error) => {
                    tracing::error!(workspace_id = %id, error = %error, "workspace not loaded");
                    rematerialize_failed |= rematerialize;
                    continue;
                }
            };
            let paths = WorkspacePaths::at(id, record.path.clone(), record.state_dir.clone());
            let settings = match self.wc_settings(record.watcher) {
                Ok(settings) => settings,
                Err(error) => {
                    // Scans are full ones, and the watcher is missing too,
                    // so the workspace lives on the fallback timer.
                    tracing::warn!(
                        workspace_id = %id,
                        watcher = record.watcher.as_str(),
                        error = %ErrorChain(&error),
                        "loading the workspace without its watcher's jj settings"
                    );
                    self.settings.clone()
                }
            };
            let generation = handle.generation();
            let jj = self.reopen(&handle, &paths, settings).await;
            let spec = base_spec(&handle, &record).await;
            let stores = self.load_stores(&handle, &record).await;
            let entry = WorkspaceEntry {
                record: Mutex::new(record),
                paths,
                backend: backend_for(kind, &self.config.file.workspace.overlay()),
                jj: Arc::new(Mutex::new(jj)),
                jj_generation: AtomicU64::new(generation),
                repo: handle,
                scan_lock: tokio::sync::Mutex::new(()),
                spec: Mutex::new(spec),
                stores: Mutex::new(stores),
            };
            self.insert_entry(id, Arc::new(entry));
            self.scheduler.track(id, TrackReason::Loaded).await;
            loaded += 1;
        }
        if rematerialize && !rematerialize_failed {
            self.db.delete_meta(META_REMATERIALIZE_PENDING)?;
            tracing::info!("restore finished: every active workspace is rematerialized");
        }
        Ok(loaded)
    }

    /// The workspace's jj handle, loaded at the repository thread's head
    /// (each snapshot reloads it there anyway), with a working copy an
    /// interrupted GC stranded moved to that head; `None` (logged) if it
    /// cannot be reopened.
    async fn reopen(
        &self,
        handle: &RepoHandle,
        paths: &WorkspacePaths,
        settings: UserSettings,
    ) -> Option<reef_lib::jj::Repo> {
        let id = paths.id;
        let root = paths.root.clone();
        let state_dir = paths.state_dir.clone();
        let opened = handle
            .call_async(move |head| {
                async move {
                    head.refresh().await?;
                    let mut ws = head.repo.open_workspace_with(&root, &state_dir, &settings)?;
                    // A GC cut short by a crash can leave the working copy
                    // on an abandoned operation (R1.17).
                    if let Err(error) = ws.rebind_if_stranded().await {
                        tracing::warn!(workspace_id = %id, error = %ErrorChain(&error), "cannot check the working copy's operation");
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

    /// The repository thread's head, refreshed first ([`RepoHead::refresh`])
    /// if `refresh`, else as it stands (a reopen that failed after a panic
    /// is retried either way), with the workspace's jj handle reopened on
    /// the repository's
    /// store if the thread has reopened the repository after a panicked job
    /// since the handle was opened (#63). The handle shares the store it was
    /// opened on, and a panic inside jj-lib can leave that store's locks
    /// poisoned, failing every scan until the daemon restarts. The
    /// generations are compared in the same job that reads the head, so a
    /// reopen queued ahead of it is seen; one landing after it fails one scan, and
    /// the next renews. A handle that is gone (released, or not reopened at
    /// start) stays gone. The caller holds the workspace's scan lock.
    ///
    /// [`RepoHead::refresh`]: crate::repo::RepoHead::refresh
    pub(super) async fn workspace_head(
        &self,
        entry: &Arc<WorkspaceEntry>,
        refresh: bool,
    ) -> Result<Arc<ReadonlyRepo>, ManagerError> {
        let id = entry.paths.id;
        let loaded = entry
            .jj
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some();
        let opened_at = entry.jj_generation.load(Ordering::Relaxed);
        // As at start, a Watchman workspace whose settings cannot be built
        // falls back to the default ones (logged by `load_active`).
        let settings = self
            .wc_settings(entry.record().watcher)
            .unwrap_or_else(|_| self.settings.clone());
        let root = entry.paths.root.clone();
        let state_dir = entry.paths.state_dir.clone();
        let (head, renewed) = entry
            .repo
            .call_async(move |head| {
                async move {
                    let readonly = if refresh || head.reopen_pending() {
                        head.refresh().await?
                    } else {
                        head.readonly()
                    };
                    let generation = head.generation();
                    if !loaded || generation == opened_at {
                        return Ok((readonly, None));
                    }
                    let ws = head
                        .repo
                        .open_workspace_with(&root, &state_dir, &settings)?;
                    Ok::<_, ManagerError>((readonly, Some((ws, generation))))
                }
                .boxed_local()
            })
            .await??;
        if let Some((ws, generation)) = renewed {
            let entry = Arc::clone(entry);
            tokio::task::spawn_blocking(move || {
                let mut guard = entry.jj.lock().unwrap_or_else(PoisonError::into_inner);
                if guard.is_some() {
                    *guard = Some(ws);
                    entry.jj_generation.store(generation, Ordering::Relaxed);
                    tracing::info!(workspace_id = %id, "workspace reopened after its repository was");
                }
            })
            .await?;
        }
        Ok(head)
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
/// Returns the operation that forgot it, `None` if it was gone.
pub(super) async fn forget_jj_workspace(
    handle: &RepoHandle,
    id: WorkspaceId,
    description: &str,
) -> Result<Option<String>, ManagerError> {
    let name = id.jj_workspace_name();
    let description = description.replace("{id}", &id.as_hex());
    let forgotten = handle
        .call_async(move |head| {
            async move { head.repo.forget_workspace(&name, &description).await }.boxed_local()
        })
        .await;
    match forgotten? {
        Ok(op_id) => Ok(Some(op_id.to_string())),
        Err(ForgetWorkspaceError::NoSuchWorkspace(_)) => Ok(None),
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

/// The snapshot spec `record`'s workspace was created with: `reef.toml`
/// read again from its base tree, the tree it was created on (R1.19), so a
/// restart neither loses its excludes nor picks up a `reef.toml` edited
/// inside it. If the base tree cannot be read (GC can prune the tree of a
/// commit edited in place once the workspace has rewritten it), the
/// workspace's current commit is read instead: its `reef.toml` is at worst
/// one edited inside it, which beats snapshotting `target/` and store
/// mounts. If that fails too, the defaults. `Repo` logs each failure.
async fn base_spec(handle: &RepoHandle, record: &WorkspaceRecord) -> super::SnapshotSpec {
    let key = record.base_tree_id.clone();
    let commit = record.commit_id.parse::<CommitIdHex>().ok();
    let read = handle
        .call_async(move |head| {
            async move {
                if let Ok(spec) = head.repo.spec_in_tree(&key).await {
                    return Some((spec, true));
                }
                let spec = head.repo.spec_at(&commit?).await.ok()?;
                Some((spec, false))
            }
            .boxed_local()
        })
        .await;
    match read {
        Ok(Some((spec, true))) => return crate::spec::resolve(spec).snapshot,
        Ok(Some((spec, false))) => {
            tracing::warn!(
                workspace_id = %record.id,
                tree_id = %record.base_tree_id,
                commit_id = %record.commit_id,
                "cannot read reef.toml from the workspace's base tree; using its current commit's"
            );
            return crate::spec::resolve(spec).snapshot;
        }
        Ok(None) => {}
        Err(error) => tracing::error!(
            workspace_id = %record.id,
            error = %ErrorChain(&error),
            "cannot read the workspace's reef.toml on the repository thread"
        ),
    }
    tracing::error!(
        workspace_id = %record.id,
        "no reef.toml readable for the workspace; its snapshots use no excludes"
    );
    super::SnapshotSpec::default()
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
        Manager::new(
            config,
            Arc::new(Db::open_in_memory().unwrap()),
            Arc::new(RepoRegistry::new()),
            settings,
        )
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
