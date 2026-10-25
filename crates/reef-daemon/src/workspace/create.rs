//! [`Manager::create`]: from a request to a leased directory.
//!
//! The steps, and the thread each runs on:
//!
//! 1. (async) Validate, resolve the repository, select the watcher
//!    (R1.14), allocate the id, compute the paths, select the backend
//!    (probing on the blocking pool).
//! 2. (repository thread) Reload at head; in a colocated repository import
//!    git refs; resolve the start revision; note whether another Reef
//!    workspace already has the edited commit; read `reef.toml` from the
//!    resolved commit's tree (R1.19), which sets the lease length when the
//!    request gives none and the snapshot excludes, and hash each declared
//!    store's lockfile in that tree (R1.20). Then (async) look each store up:
//!    one with no sealed build fails the create unless the request skips
//!    missing stores. Step 4 looks them up again under the store manager's
//!    mount lock.
//! 3. (blocking, then the repository thread) Ensure the base: the base
//!    cache's per-tree lock, then, if the tree is not cached, on the
//!    repository thread either `Repo::materialize_tree_from` over a clone
//!    of the most recently used base (backends with a [`DirCloner`](crate::backend::DirCloner), R1.08)
//!    or `Repo::materialize_tree`, which is also the fallback.
//! 4. (blocking) The backend materializes the workspace from the base, and
//!    the sealed stores are mounted into it (R1.20).
//! 5. (repository thread) `Repo::create_workspace_with`, the working copy
//!    loaded with the watcher's jj settings.
//! 6. (blocking) Seed the working copy's `tree_state` from the base's, or
//!    fall back to `Repo::reset_to`.
//! 7. (async) Insert the row and issue the lease in one transaction
//!    ([`crate::lease::LeaseManager::issue_with_workspace`]).
//!
//! Then the workspace is loaded and its watch set up, on the blocking pool
//! ([`crate::snapshot::Scheduler::track`]). The create returns once it is
//! watched or its watch is pending (ADR 0028): a Watchman subscription
//! waiting for creates to go quiet (R1.29), or on Linux a notify watch
//! queued for the attach thread (R1.32), whose `watch` time is then only
//! the queueing. Either way the first snapshot once the watch is up is a
//! full scan, so a write right after the create is not lost. Each step's
//! time is in [`CreateTimings`], the response, and the `workspace created`
//! log line (R1.29).
//!
//! A failure after step 4 unmounts the stores and tears the directory down;
//! after step 5 also forgets the jj workspace and removes its state
//! directory. Partial state never leaves a row behind. From mounting the
//! stores to the row, the create holds the store manager's mount lock
//! shared, so GC cannot remove a store it mounts before its entry counts it
//! as in use.

use std::cell::Cell;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use reef_lib::error::ErrorChain;
use reef_lib::jj::{
    ChangeSummary, CommitIdHex, CreateWorkspaceError, MaterializeError, Repo, ResolveError,
    StartPoint,
};
use reef_lib::spec::SPEC_FILE;
use reef_lib::telemetry::elapsed_ms;

use super::manager::{forget_jj_workspace, remove_state_dir};
use super::stores::{StorePlan, StoreTarget, TreeAt, store_targets};
use super::{
    CreateSpec, CreateTimings, Created, Manager, ManagerError, SnapshotSpec, Start, WorkspaceEntry,
};
use crate::backend::{Backend, BackendChoice, BackendError, BaseEntry, WorkspacePaths, select};
use crate::model::{Attention, RepoId, Timestamp, WorkspaceId, WorkspaceRecord, WorkspaceState};
use crate::repo::RepoHandle;
use crate::snapshot::TrackReason;
use crate::spec::ResolvedSpec;
use crate::store::{MountedStore, StoreManager};

/// The description of a colocated repository's ref import before a create.
const IMPORT_DESCRIPTION: &str = "reef: import git refs";

/// Step 2's answer; also a move's (R1.06).
pub(super) struct ResolvedStart {
    pub start: StartPoint,
    pub commit: CommitIdHex,
    /// What [`default_start`] chose, when the request named no start.
    pub default: Option<String>,
    pub tree_key: String,
    pub shared_change: bool,
    pub spec: ResolvedSpec,
    /// The spec's stores, their lockfiles hashed in the commit's tree.
    pub stores: Vec<StoreTarget>,
}

impl Manager {
    /// Creates a workspace; see the module docs for the steps.
    #[tracing::instrument(skip_all, fields(repo = %spec.repo, workspace_id))]
    pub async fn create(&self, spec: CreateSpec) -> Result<Created, ManagerError> {
        let started = Instant::now();
        let mut lap = Lap(started);
        let mut timings = CreateTimings::default();
        // 1. Stamped first, so a Watchman subscription of the previous
        // create holds off until this one is done.
        self.scheduler.create_started();
        if spec.owner.trim().is_empty() {
            return Err(ManagerError::EmptyOwner);
        }
        // Checked before any work; the length is settled at step 2.
        self.leases.length_for(spec.lease)?;
        // Held until the row is written, so the repository is not forgotten
        // under this create.
        let repo_lock = self.repo_lock(spec.repo);
        let _shared = repo_lock.read().await;
        let repo = self.db.repository(spec.repo)?;
        let handle = self.handle(repo.id)?;
        let watcher = self.watcher_for(&repo)?;
        let wc_settings = self.wc_settings(watcher)?;
        let id = WorkspaceId::generate();
        tracing::Span::current().record("workspace_id", tracing::field::display(id));
        let paths = self.new_paths(repo.id, id, spec.path.as_deref())?;
        let backend = self.select_backend(spec.backend, repo.id, &paths).await?;
        lap.next();

        // 2.
        let resolved = resolve_start(&handle, spec.start.clone()).await?;
        let length = self
            .leases
            .length_with_spec(spec.lease, resolved.spec.lease)?;
        // Fails a create that would fail anyway before anything is made.
        self.plan_stores(&resolved.stores, &spec.store_rules(), &resolved.commit)?;
        timings.resolve_ms = lap.next();

        // 3.
        let (base, base_was_cached) = self.ensure_base(&handle, &resolved, &*backend).await?;
        timings.base_ms = lap.next();

        // 4.
        let mut undo = Undo::new(handle.clone(), id);
        materialize(backend, &base, &paths, &mut undo).await?;
        timings.materialize_ms = lap.next();
        let (_mounting, plan, mounted) = self
            .mount_planned(&resolved, &spec, &paths.root, &mut undo)
            .await?;
        timings.mount_ms = lap.next();

        // 5.
        let generation = handle.generation();
        let created =
            create_jj_workspace(&handle, id, &paths, resolved.start.clone(), wc_settings).await;
        let (ws, op_id, wc) = undo.or_undo(created).await?;
        undo.jj_workspace = true;
        undo.state_dir = Some(paths.state_dir.clone());
        timings.jj_ms = lap.next();

        // 6.
        let ws = undo
            .or_undo(seed(ws, &base, &paths, &wc.commit_id).await)
            .await?;
        timings.seed_ms = lap.next();

        // 7.
        let now = Timestamp::now();
        let lease = self.leases.new_lease(id, repo.id, &spec.owner, length, now);
        let record = WorkspaceRecord {
            id,
            repo_id: repo.id,
            owner: spec.owner,
            purpose: spec.purpose,
            change_id: wc.change_id.to_string(),
            commit_id: wc.commit_id.to_string(),
            base_tree_id: base.tree_id.clone(),
            backend: undo.backend_kind(),
            path: std::fs::canonicalize(&paths.root).unwrap_or_else(|_| paths.root.clone()),
            state_dir: paths.state_dir.clone(),
            lease_id: Some(lease.id),
            state: WorkspaceState::Active,
            divergent: false,
            attention: (!plan.missing.is_empty()).then_some(Attention::StoreMissing),
            created_at: now,
            last_snapshot_at: None,
            last_snapshot_op: None,
            watcher,
        };
        if let Err(error) = self.leases.issue_with_workspace(&record, &lease) {
            drop(ws);
            undo.run().await;
            return Err(error.into());
        }
        timings.insert_ms = lap.next();
        self.load_created(
            &record,
            &mut undo,
            (ws, generation),
            handle,
            resolved.spec.snapshot.clone(),
            mounted.clone(),
        )
        .await;
        timings.watch_ms = lap.next();
        timings.total_ms = millis(started.elapsed());
        let created = Created {
            record,
            lease,
            op_id,
            shared_change: resolved.shared_change,
            default_start: resolved.default.clone(),
            timings,
            base_was_cached,
            stores: mounted,
        };
        log_created(&created, &resolved, &plan);
        Ok(created)
    }

    /// The end of step 4: the stores looked up again under the store
    /// manager's mount lock (GC may have removed one since step 2) and
    /// mounted at `root`, recorded in `undo`. Returns the lock, which the
    /// create holds until its row is written, so GC sees the new entry use
    /// them.
    async fn mount_planned(
        &self,
        resolved: &ResolvedStart,
        spec: &CreateSpec,
        root: &Path,
        undo: &mut Undo,
    ) -> Result<
        (
            tokio::sync::RwLockReadGuard<'_, ()>,
            StorePlan,
            Vec<MountedStore>,
        ),
        ManagerError,
    > {
        let mounting = self.stores.mounting.read().await;
        let plan = undo
            .or_undo(self.plan_stores(&resolved.stores, &spec.store_rules(), &resolved.commit))
            .await?;
        let kind = undo.backend.as_ref().map(|(backend, _)| backend.kind());
        let mounted = match kind {
            Some(kind) => self.mount_stores(&plan.mount, root, kind).await,
            None => Ok(Vec::new()),
        };
        let mounted = undo.or_undo(mounted).await?;
        undo.mount_stores(Arc::clone(&self.stores), mounted.clone());
        Ok((mounting, plan, mounted))
    }

    /// The end of step 7: loads the new workspace, taking the backend from
    /// `undo`, and starts scheduling its snapshots.
    async fn load_created(
        &self,
        record: &WorkspaceRecord,
        undo: &mut Undo,
        (ws, generation): (Repo, u64),
        handle: RepoHandle,
        spec: SnapshotSpec,
        stores: Vec<MountedStore>,
    ) {
        let Some((backend, paths)) = undo.backend.take() else {
            unreachable!("set after step 4")
        };
        // The entry owns the mounts now; release unmounts them.
        undo.stores = None;
        let entry = WorkspaceEntry {
            record: Mutex::new(record.clone()),
            paths,
            backend,
            jj: Arc::new(Mutex::new(Some(ws))),
            jj_generation: AtomicU64::new(generation),
            repo: handle,
            scan_lock: tokio::sync::Mutex::new(()),
            spec: Mutex::new(spec),
            stores: Mutex::new(stores),
        };
        self.insert_entry(record.id, Arc::new(entry));
        self.scheduler.track(record.id, TrackReason::Created).await;
    }

    /// Step 1's backend: `choice`, or `workspace.backend`, probed (on the
    /// blocking pool) at the directory the workspace is created in against
    /// the repository's base cache.
    async fn select_backend(
        &self,
        choice: Option<BackendChoice>,
        repo: RepoId,
        paths: &WorkspacePaths,
    ) -> Result<Box<dyn Backend>, ManagerError> {
        let choice = choice.unwrap_or(self.config.file.workspace.backend);
        let probe_dir = paths
            .root
            .parent()
            .map_or_else(|| paths.root.clone(), Path::to_owned);
        let bases_dir = self.base_cache(repo).dir().to_owned();
        let overlay = self.config.file.workspace.overlay();
        Ok(
            tokio::task::spawn_blocking(move || select(choice, &probe_dir, &bases_dir, &overlay))
                .await??,
        )
    }

    /// The new workspace's paths: `path` (absolute) or
    /// `<workspace.root>/<id>`, which must not exist; state in
    /// `<state>/repos/<repo-id>/wc/<id>`.
    fn new_paths(
        &self,
        repo: RepoId,
        id: WorkspaceId,
        path: Option<&Path>,
    ) -> Result<WorkspacePaths, ManagerError> {
        let root = match path {
            Some(path) if !path.is_absolute() => {
                return Err(ManagerError::PathNotAbsolute(path.to_owned()));
            }
            Some(path) => path.to_owned(),
            None => self.config.workspace_root(repo).join(id.as_hex()),
        };
        if std::fs::symlink_metadata(&root).is_ok() {
            return Err(ManagerError::PathExists(root));
        }
        // Rows are recorded with canonical paths; the root does not exist
        // yet, so canonicalize its parent. Checked here, not only by the
        // insert at step 7, so the failure costs nothing.
        let canonical = root
            .parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .zip(root.file_name())
            .map_or_else(|| root.clone(), |(parent, name)| parent.join(name));
        if let Some(holder) = self.db.workspace_at_path(&canonical)? {
            return Err(ManagerError::PathRecorded {
                path: root,
                id: holder.id,
                state: holder.state,
            });
        }
        let state_dir = self
            .config
            .paths
            .repo_dir(repo)
            .join("wc")
            .join(id.as_hex());
        Ok(WorkspacePaths::at(id, root, state_dir))
    }

    /// Step 3: [`Manager::ensure_base_of`] for the resolved commit; also a
    /// move's (R1.06).
    pub(super) async fn ensure_base(
        &self,
        handle: &RepoHandle,
        resolved: &ResolvedStart,
        backend: &dyn Backend,
    ) -> Result<(BaseEntry, bool), ManagerError> {
        self.ensure_base_of(handle, &resolved.commit, &resolved.tree_key, backend)
            .await
    }

    /// The base for `commit`'s tree (`tree_key`), and whether it was
    /// already cached; step 3, and a rematerialization's (R1.18). A cold
    /// materialization runs on the repository thread, under the base
    /// cache's lock for the tree. If the backend has a
    /// [`DirCloner`](crate::backend::DirCloner) and the cache another base,
    /// the most recently used one is cloned and only the diff checked out;
    /// if that fails, the whole tree is.
    pub(super) async fn ensure_base_of(
        &self,
        handle: &RepoHandle,
        commit: &CommitIdHex,
        tree_key: &str,
        backend: &dyn Backend,
    ) -> Result<(BaseEntry, bool), ManagerError> {
        let cache = self.base_cache(handle.id());
        let cloner = backend.dir_cloner();
        let handle = handle.clone();
        let commit = commit.clone();
        let key = tree_key.to_owned();
        tokio::task::spawn_blocking(move || {
            // Cells, so the closure can be called twice (the fallback) while
            // the error of the first call is cleared in between.
            let cached = Cell::new(true);
            let thread_error = Cell::new(None);
            let materialize = |incremental: bool, dir: &Path, state: &Path| {
                cached.set(false);
                let (dir, state, commit) = (dir.to_owned(), state.to_owned(), commit.clone());
                let at = dir.clone();
                let job = handle.call_async(move |head| {
                    async move {
                        if incremental {
                            head.repo.materialize_tree_from(&commit, &dir, &state).await
                        } else {
                            head.repo.materialize_tree(&commit, &dir, &state).await
                        }
                    }
                    .boxed_local()
                });
                match pollster::block_on(job) {
                    Ok(materialized) => materialized,
                    Err(error) => {
                        let message = error.to_string();
                        thread_error.set(Some(error));
                        Err(MaterializeError::Io {
                            op: "materialize on the repository thread",
                            path: at,
                            source: io::Error::other(message),
                        })
                    }
                }
            };
            let nearest = cloner
                .and_then(|cloner| nearest_base(&cache, &key).map(|nearest| (cloner, nearest)));
            let ensured = match nearest {
                Some((cloner, nearest)) => cache
                    .ensure_from(&key, &nearest, cloner, |dir, state| {
                        materialize(true, dir, state)
                    })
                    .or_else(|error| {
                        tracing::warn!(
                            tree_id = %key,
                            from_tree_id = %nearest.tree_id,
                            error = %ErrorChain(&error),
                            "cannot build the base from another; checking out the whole tree"
                        );
                        thread_error.set(None);
                        cache.ensure(&key, |dir, state| materialize(false, dir, state))
                    }),
                None => cache.ensure(&key, |dir, state| materialize(false, dir, state)),
            };
            match (ensured, thread_error.take()) {
                (Err(_), Some(error)) => Err(error.into()),
                (ensured, _) => Ok((ensured?, cached.get())),
            }
        })
        .await?
    }
}

/// The `workspace created` line: what was made and each step's time.
fn log_created(created: &Created, resolved: &ResolvedStart, plan: &StorePlan) {
    let (record, timings) = (&created.record, &created.timings);
    let (stores, stores_missing) = (resolved.spec.spec.stores.len(), plan.missing.len());
    tracing::info!(
        path = %record.path.display(),
        backend = %record.backend,
        change_id = %record.change_id,
        op_id = %created.op_id,
        shared_change = created.shared_change,
        stores,
        stores_mounted = created.stores.len(),
        stores_missing,
        base_was_cached = created.base_was_cached,
        resolve_ms = timings.resolve_ms,
        base_ms = timings.base_ms,
        materialize_ms = timings.materialize_ms,
        mount_ms = timings.mount_ms,
        jj_ms = timings.jj_ms,
        seed_ms = timings.seed_ms,
        insert_ms = timings.insert_ms,
        watch_ms = timings.watch_ms,
        total_ms = timings.total_ms,
        "workspace created"
    );
}

/// A stopwatch for [`CreateTimings`]: the time since the previous lap.
struct Lap(Instant);

impl Lap {
    /// Milliseconds since the previous call (or the start), restarting.
    fn next(&mut self) -> u64 {
        let now = Instant::now();
        let lap = now.duration_since(self.0);
        self.0 = now;
        millis(lap)
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The base to clone a new base for `tree_id` from: the most recently used
/// other entry that is complete on disk, if any (cloning an incomplete one
/// would cost a clone and its removal before failing). A failure to list is
/// logged and means none.
fn nearest_base(cache: &crate::backend::BaseCache, tree_id: &str) -> Option<BaseEntry> {
    match cache.entries() {
        Ok(entries) => entries
            .into_iter()
            .rev()
            .find(|entry| entry.tree_id != tree_id && entry.is_complete()),
        Err(error) => {
            tracing::warn!(error = %ErrorChain(&error), "cannot list the base cache");
            None
        }
    }
}

/// Step 4: the backend materializes the workspace from the base. On
/// success `undo` owns the backend, to tear down if a later step fails.
async fn materialize(
    backend: Box<dyn Backend>,
    base: &BaseEntry,
    paths: &WorkspacePaths,
    undo: &mut Undo,
) -> Result<(), ManagerError> {
    let (backend, materialized) = {
        let (base, paths) = (base.clone(), paths.clone());
        tokio::task::spawn_blocking(move || {
            let result = backend.materialize(&base, &paths);
            (backend, result)
        })
        .await?
    };
    match materialized {
        Ok(()) => {
            undo.backend = Some((backend, paths.clone()));
            Ok(())
        }
        // A root that appeared since step 1 is someone else's: leave it.
        Err(error @ BackendError::RootExists(_)) => Err(error.into()),
        Err(error) => {
            undo.backend = Some((backend, paths.clone()));
            undo.run().await;
            Err(error.into())
        }
    }
}

/// Step 2, on the repository thread; also a move's target (R1.06).
pub(super) async fn resolve_start(
    handle: &RepoHandle,
    start: Start,
) -> Result<ResolvedStart, ManagerError> {
    handle
        .call_async(move |head| {
            async move {
                refresh_for_resolve(head).await?;
                let mut chosen = None;
                let (start, commit) = match start {
                    Start::Change(text) => {
                        let resolved = head.repo.resolve_revision(&text).await?;
                        (
                            StartPoint::Edit(resolved.commit_id.clone()),
                            resolved.commit_id,
                        )
                    }
                    Start::NewOn(text) => {
                        let resolved = head.repo.resolve_revision(&text).await?;
                        (
                            StartPoint::NewOn(resolved.commit_id.clone()),
                            resolved.commit_id,
                        )
                    }
                    Start::Default => {
                        let default = default_start(&head.repo).await?;
                        if default.chosen == ROOT_CHECKOUT {
                            tracing::info!(
                                commit_id = %default.commit,
                                "no main, origin HEAD, master or trunk bookmark; starting on the root checkout's change"
                            );
                        }
                        chosen = Some(default.chosen);
                        (StartPoint::NewOn(default.commit.clone()), default.commit)
                    }
                };
                // Before anything reads its tree (RX.01).
                head.repo
                    .check_tree_names(&commit, &mut head.checked_trees)
                    .await?;
                let shared_change = matches!(start, StartPoint::Edit(_))
                    && head.repo.workspaces_on_commit(&commit).iter().any(|name| {
                        name.starts_with(reef_lib::jj::workspace::REEF_WORKSPACE_PREFIX)
                    });
                let tree_key = head.repo.tree_key(&commit).await?;
                let spec = head.repo.spec_at(&commit).await?;
                if spec.is_none() && head.root.join(SPEC_FILE).exists() {
                    tracing::info!(
                        commit_id = %commit,
                        "{SPEC_FILE} is on disk at the root but not in the target commit; using the defaults (snapshot or commit it)"
                    );
                }
                let stores = match &spec {
                    Some(spec) => {
                        store_targets(&head.repo, &spec.stores, TreeAt::Commit(&commit)).await?
                    }
                    None => Vec::new(),
                };
                Ok::<_, ManagerError>(ResolvedStart {
                    start,
                    commit,
                    default: chosen,
                    tree_key,
                    shared_change,
                    spec: crate::spec::resolve(spec),
                    stores,
                })
            }
            .boxed_local()
        })
        .await?
}

/// Reloads the repository thread at head and, in a colocated repository,
/// imports git refs, so operations the `jj` CLI wrote on the registered
/// root (bookmarks, new changes) count when a revision is resolved.
pub(super) async fn refresh_for_resolve(
    head: &mut crate::repo::RepoHead,
) -> Result<(), ManagerError> {
    head.reload().await?;
    if head.colocated {
        let begun = Instant::now();
        let imported = head.repo.import_git_refs(IMPORT_DESCRIPTION).await?;
        tracing::info!(
            op_id = ?imported.op_id.as_ref().map(ToString::to_string),
            elapsed_ms = elapsed_ms(begun),
            "git refs imported before resolving"
        );
    }
    Ok(())
}

/// What a create's default start (R1.31) reports as `CreateResponse.start` when
/// no bookmark matched.
pub use reef_api::json::ROOT_CHECKOUT;

/// The remote whose default branch and remote-tracking bookmarks
/// [`default_start`] tries.
const DEFAULT_REMOTE: &str = "origin";

/// The commit a create without a start revision starts on (R1.31), and
/// what it is: a bookmark (`master`, `main@origin`) or [`ROOT_CHECKOUT`].
pub(super) struct DefaultStart {
    pub commit: CommitIdHex,
    pub chosen: String,
}

/// The bookmarks [`default_start`] tries, in order, as (name, remote):
/// `main`; the branch `origin/HEAD` names, local then remote-tracking, so a
/// stale local `master` beside a remote whose default is now `main` does
/// not win; `master` and `trunk`; then `main`, `master` and `trunk` on
/// `origin`.
fn default_candidates(origin_head: Option<&str>) -> Vec<(String, Option<&'static str>)> {
    let mut candidates: Vec<(String, Option<&'static str>)> = Vec::new();
    let mut push = |name: &str, remote| {
        // origin's HEAD is often `main` or `master`: each is tried once.
        if !candidates.iter().any(|(n, r)| n == name && *r == remote) {
            candidates.push((name.to_owned(), remote));
        }
    };
    push("main", None);
    if let Some(head) = origin_head {
        push(head, None);
        push(head, Some(DEFAULT_REMOTE));
    }
    for name in ["master", "trunk"] {
        push(name, None);
    }
    for name in ["main", "master", "trunk"] {
        push(name, Some(DEFAULT_REMOTE));
    }
    candidates
}

/// The start of a create without a start revision: the first of
/// [`default_candidates`] that exists, else the `default` workspace's
/// working-copy commit. A conflicted bookmark is skipped with a warning. A
/// store build without `--rev`, GC's store step, and a move without a
/// target use the same.
pub(super) async fn default_start(repo: &Repo) -> Result<DefaultStart, ManagerError> {
    let origin_head = repo.git_remote_head(DEFAULT_REMOTE);
    for (name, remote) in default_candidates(origin_head.as_deref()) {
        let chosen = match remote {
            None => name.clone(),
            Some(remote) => format!("{name}@{remote}"),
        };
        match repo.resolve_bookmark(&name, remote).await {
            Ok(Some(resolved)) => {
                return Ok(DefaultStart {
                    commit: resolved.commit_id,
                    chosen,
                });
            }
            Ok(None) => {}
            Err(error @ ResolveError::ConflictedBookmark(_)) => {
                tracing::warn!(
                    error = %ErrorChain(&error),
                    "skipping {chosen} as the default start"
                );
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(DefaultStart {
        commit: repo.wc_change()?.commit_id,
        chosen: ROOT_CHECKOUT.to_owned(),
    })
}

/// Step 5, on the repository thread: the jj workspace, its handle, the
/// operation, and its working-copy commit. A failure after jj committed
/// the operation forgets the workspace again before returning.
async fn create_jj_workspace(
    handle: &RepoHandle,
    id: WorkspaceId,
    paths: &WorkspacePaths,
    start: StartPoint,
    wc_settings: reef_lib::jj::UserSettings,
) -> Result<(Repo, String, ChangeSummary), ManagerError> {
    let name = id.jj_workspace_name();
    let root = paths.root.clone();
    let state_dir = paths.state_dir.clone();
    let description = format!("reef: create workspace {id}");
    handle
        .call_async(move |head| {
            async move {
                let created = head
                    .repo
                    .create_workspace_with(
                        &name,
                        &root,
                        &state_dir,
                        start,
                        &description,
                        &wc_settings,
                    )
                    .await;
                let (ws, op_id) = match created {
                    Ok(created) => created,
                    Err(error) => {
                        // These follow the committed operation; the others
                        // come before it and leave nothing.
                        if matches!(
                            error,
                            CreateWorkspaceError::Init(_)
                                | CreateWorkspaceError::WorkspaceStore(_)
                                | CreateWorkspaceError::Path(_)
                        ) {
                            forget_on_thread(head, &name, id).await;
                        }
                        return Err(error.into());
                    }
                };
                match ws.wc_change() {
                    Ok(wc) => Ok((ws, op_id.to_string(), wc)),
                    Err(error) => {
                        drop(ws);
                        forget_on_thread(head, &name, id).await;
                        std::fs::remove_dir_all(&state_dir).ok();
                        Err(error.into())
                    }
                }
            }
            .boxed_local()
        })
        .await?
}

/// Forgets `name` from inside a repository-thread job, logging failure.
async fn forget_on_thread(head: &mut crate::repo::RepoHead, name: &str, id: WorkspaceId) {
    let description = format!("reef: release workspace {id} (create failed)");
    match head.repo.forget_workspace(name, &description).await {
        Ok(_) | Err(reef_lib::jj::ForgetWorkspaceError::NoSuchWorkspace(_)) => {}
        Err(error) => tracing::error!(
            workspace_id = %id,
            error = %ErrorChain(&error),
            "cannot forget the jj workspace of a failed create"
        ),
    }
}

/// Step 6: copies the base's `tree_state` over the new working copy's, so
/// the first snapshot is a no-op scan. If that fails, resets the working
/// copy to `commit` instead (every file rehashed on the next snapshot).
pub(super) async fn seed(
    mut ws: Repo,
    base: &BaseEntry,
    paths: &WorkspacePaths,
    commit: &CommitIdHex,
) -> Result<Repo, ManagerError> {
    let from = base.tree_state_file();
    let to = paths
        .state_dir
        .join(reef_lib::jj::materialize::TREE_STATE_FILE);
    let copied = {
        let (from, to) = (from.clone(), to.clone());
        tokio::task::spawn_blocking(move || std::fs::copy(&from, &to)).await?
    };
    let Err(error) = copied else {
        return Ok(ws);
    };
    tracing::warn!(
        from = %from.display(),
        %error,
        "cannot seed the working copy from the base; resetting it instead"
    );
    let commit = commit.clone();
    let (ws, reset) = tokio::task::spawn_blocking(move || {
        let reset = pollster::block_on(ws.reset_to(&commit));
        (ws, reset)
    })
    .await?;
    reset?;
    Ok(ws)
}

/// What a failed create has to undo, in reverse order of the steps.
struct Undo {
    handle: RepoHandle,
    id: WorkspaceId,
    /// The backend and the paths it materialized (step 4).
    backend: Option<(Box<dyn Backend>, WorkspacePaths)>,
    /// Whether the jj workspace exists (step 5).
    jj_workspace: bool,
    /// The working-copy state directory (step 5).
    state_dir: Option<PathBuf>,
    /// The stores mounted into the directory (step 4), unmounted before
    /// it is torn down.
    stores: Option<(Arc<StoreManager>, Vec<MountedStore>)>,
}

impl Undo {
    fn new(handle: RepoHandle, id: WorkspaceId) -> Self {
        Self {
            handle,
            id,
            backend: None,
            jj_workspace: false,
            state_dir: None,
            stores: None,
        }
    }

    /// `result`, after undoing what was done when it is an error.
    async fn or_undo<T>(&mut self, result: Result<T, ManagerError>) -> Result<T, ManagerError> {
        if result.is_err() {
            self.run().await;
        }
        result
    }

    /// Records the stores mounted at step 4.
    fn mount_stores(&mut self, stores: Arc<StoreManager>, mounted: Vec<MountedStore>) {
        if !mounted.is_empty() {
            self.stores = Some((stores, mounted));
        }
    }

    fn backend_kind(&self) -> String {
        self.backend
            .as_ref()
            .map(|(backend, _)| backend.kind().as_str().to_owned())
            .unwrap_or_default()
    }

    /// Undoes what was done; failures are logged.
    async fn run(&mut self) {
        let id = self.id;
        if std::mem::take(&mut self.jj_workspace)
            && let Err(error) = forget_jj_workspace(
                &self.handle,
                id,
                "reef: release workspace {id} (create failed)",
            )
            .await
        {
            tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "cannot undo the jj workspace");
        }
        if let Some(state_dir) = self.state_dir.take() {
            remove_state_dir(&state_dir).await;
        }
        if let Some((stores, mounted)) = self.stores.take()
            && let Some((_, paths)) = &self.backend
        {
            let root = paths.root.clone();
            let unmounted = tokio::task::spawn_blocking(move || {
                for store in &mounted {
                    if let Err(error) = stores.unmount(&root, &store.mount, store.mode) {
                        tracing::error!(workspace_id = %id, store = %store.key.name, error = %ErrorChain(&error), "cannot unmount a store of a failed create");
                    }
                }
            })
            .await;
            if let Err(error) = unmounted {
                tracing::error!(workspace_id = %id, %error, "store unmount task failed");
            }
        }
        if let Some((backend, paths)) = self.backend.take() {
            if std::fs::symlink_metadata(&paths.root).is_err() {
                return;
            }
            let torn = tokio::task::spawn_blocking(move || backend.tear_down(&paths)).await;
            match torn {
                Ok(Ok(()) | Err(BackendError::RootMissing(_))) => {}
                Ok(Err(error)) => tracing::error!(
                    workspace_id = %id,
                    error = %ErrorChain(&error),
                    "cannot remove the directory of a failed create"
                ),
                Err(error) => tracing::error!(workspace_id = %id, %error, "tear-down task failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Paths};

    fn settings(state: &Path) -> reef_lib::jj::UserSettings {
        Config::with_defaults(Paths::in_state_dir(state).unwrap())
            .jj_settings()
            .unwrap()
    }

    /// A colocated repository at `<tmp>/repo` whose git side has one commit
    /// and exactly `refs` (full name, then `@` for the commit or
    /// `ref: <name>` for a symbolic ref), refs imported, as a create sees
    /// it.
    async fn repo_with(refs: &[(&str, &str)]) -> (tempfile::TempDir, Repo, CommitIdHex) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        let commit = reef_testing::git::init_repo(&root, &[("README", "hello\n")]);
        let git = root.join(".git");
        std::fs::remove_file(git.join("refs/heads/main")).unwrap();
        for (name, value) in refs {
            let path = git.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let contents = if *value == "@" { &commit } else { *value };
            std::fs::write(path, format!("{contents}\n")).unwrap();
        }
        // HEAD on the first local branch, as after a clone; else unborn.
        if let Some((name, _)) = refs
            .iter()
            .find(|(name, _)| name.starts_with("refs/heads/"))
        {
            std::fs::write(git.join("HEAD"), format!("ref: {name}\n")).unwrap();
        }
        let settings = settings(&tmp.path().join("state"));
        let mut repo = Repo::init_in_git_repo(&root, &settings).await.unwrap();
        repo.import_git_refs("import").await.unwrap();
        (tmp, repo, commit.parse().unwrap())
    }

    async fn chosen(refs: &[(&str, &str)]) -> String {
        let (_tmp, repo, commit) = repo_with(refs).await;
        let start = default_start(&repo).await.unwrap();
        if start.chosen != ROOT_CHECKOUT {
            assert_eq!(start.commit, commit, "{refs:?}");
        }
        start.chosen
    }

    const MAIN: (&str, &str) = ("refs/heads/main", "@");
    const MASTER: (&str, &str) = ("refs/heads/master", "@");
    const TRUNK: (&str, &str) = ("refs/heads/trunk", "@");
    const DEVELOP: (&str, &str) = ("refs/heads/develop", "@");
    const ORIGIN_MAIN: (&str, &str) = ("refs/remotes/origin/main", "@");
    const ORIGIN_MASTER: (&str, &str) = ("refs/remotes/origin/master", "@");
    const ORIGIN_TRUNK: (&str, &str) = ("refs/remotes/origin/trunk", "@");
    const ORIGIN_DEVELOP: (&str, &str) = ("refs/remotes/origin/develop", "@");
    const ORIGIN_HEAD_MAIN: (&str, &str) =
        ("refs/remotes/origin/HEAD", "ref: refs/remotes/origin/main");
    const ORIGIN_HEAD_DEVELOP: (&str, &str) = (
        "refs/remotes/origin/HEAD",
        "ref: refs/remotes/origin/develop",
    );
    const ORIGIN_HEAD_GONE: (&str, &str) =
        ("refs/remotes/origin/HEAD", "ref: refs/remotes/origin/gone");

    #[tokio::test]
    async fn default_start_order() {
        // 1. `main` first, whatever else exists.
        assert_eq!(chosen(&[MAIN, MASTER, TRUNK]).await, "main");
        assert_eq!(
            chosen(&[MAIN, DEVELOP, ORIGIN_DEVELOP, ORIGIN_HEAD_DEVELOP]).await,
            "main"
        );
        // 2. origin's HEAD, local then remote-tracking, before `master`: a
        // stale local `master` beside a remote whose default is `main`.
        assert_eq!(
            chosen(&[MASTER, ORIGIN_MAIN, ORIGIN_HEAD_MAIN]).await,
            "main@origin"
        );
        assert_eq!(
            chosen(&[MASTER, DEVELOP, ORIGIN_DEVELOP, ORIGIN_HEAD_DEVELOP]).await,
            "develop"
        );
        // 3. `master`, then `trunk` (all reverse-hex letters: looked up as a
        // bookmark only, never as a change id prefix).
        assert_eq!(chosen(&[MASTER, TRUNK]).await, "master");
        assert_eq!(chosen(&[TRUNK, ORIGIN_MAIN]).await, "trunk");
        // 4. Remote-tracking `main`, `master`, `trunk` on origin.
        assert_eq!(chosen(&[ORIGIN_MASTER, ORIGIN_MAIN]).await, "main@origin");
        assert_eq!(
            chosen(&[ORIGIN_TRUNK, ORIGIN_MASTER]).await,
            "master@origin"
        );
        assert_eq!(chosen(&[ORIGIN_TRUNK]).await, "trunk@origin");
        // origin's HEAD naming a branch that exists nowhere falls through.
        assert_eq!(chosen(&[MASTER, ORIGIN_HEAD_GONE]).await, "master");
        // 5. None: the root checkout's change.
        assert_eq!(chosen(&[DEVELOP]).await, ROOT_CHECKOUT);
        let (_tmp, repo, _) = repo_with(&[]).await;
        let start = default_start(&repo).await.unwrap();
        assert_eq!(start.chosen, ROOT_CHECKOUT);
        assert_eq!(start.commit, repo.wc_change().unwrap().commit_id);
    }

    #[tokio::test]
    async fn default_start_skips_a_conflicted_bookmark() {
        use jj_lib::backend::CommitId;
        use jj_lib::default_backend_factories::{
            default_backend_factories, default_working_copy_factories,
        };
        use jj_lib::merge::Merge;
        use jj_lib::ref_name::RefName;
        use jj_lib::workspace::Workspace;

        let (tmp, repo, commit) = repo_with(&[MAIN, MASTER]).await;
        let other = repo.wc_change().unwrap().commit_id;
        let root = tmp.path().join("repo");
        let settings = settings(&tmp.path().join("state"));
        drop(repo);
        let workspace = Workspace::load(
            &settings,
            &root,
            &default_backend_factories(),
            &default_working_copy_factories(),
        )
        .unwrap();
        let loaded = workspace.repo_loader().load_at_head().await.unwrap();
        let mut tx = loaded.start_transaction();
        let id = |hex: &CommitIdHex| Some(CommitId::try_from_hex(hex.as_str()).unwrap());
        tx.repo_mut().set_local_bookmark_target(
            RefName::new("main"),
            Merge::from_vec(vec![id(&commit), None, id(&other)]),
        );
        tx.commit("conflict main").await.unwrap();

        let repo = Repo::open(&root, &settings).await.unwrap();
        let start = default_start(&repo).await.unwrap();
        assert_eq!(start.chosen, "master");
        assert_eq!(start.commit, commit);
    }

    #[test]
    fn candidates_are_tried_once() {
        let names: Vec<_> = default_candidates(Some("master"))
            .into_iter()
            .map(|(name, remote)| format!("{name}@{}", remote.unwrap_or("")))
            .collect();
        assert_eq!(
            names,
            [
                "main@",
                "master@",
                "master@origin",
                "trunk@",
                "main@origin",
                "trunk@origin"
            ]
        );
    }

    #[test]
    fn candidates_without_an_origin_head() {
        let names: Vec<_> = default_candidates(None)
            .into_iter()
            .map(|(name, remote)| (name, remote.unwrap_or("")))
            .collect();
        let expected = [
            ("main", ""),
            ("master", ""),
            ("trunk", ""),
            ("main", "origin"),
            ("master", "origin"),
            ("trunk", "origin"),
        ];
        assert_eq!(
            names,
            expected.map(|(name, remote)| (name.to_owned(), remote))
        );
    }
}
