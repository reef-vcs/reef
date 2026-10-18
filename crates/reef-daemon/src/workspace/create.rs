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
//!    request gives none and the snapshot excludes.
//! 3. (blocking, then the repository thread) Ensure the base: the base
//!    cache's per-tree lock, then, if the tree is not cached, on the
//!    repository thread either `Repo::materialize_tree_from` over a clone
//!    of the most recently used base (backends with a [`DirCloner`](crate::backend::DirCloner), R1.08)
//!    or `Repo::materialize_tree`, which is also the fallback.
//! 4. (blocking) The backend materializes the workspace from the base.
//! 5. (repository thread) `Repo::create_workspace_with`, the working copy
//!    loaded with the watcher's jj settings.
//! 6. (blocking) Seed the working copy's `tree_state` from the base's, or
//!    fall back to `Repo::reset_to`.
//! 7. (async) Insert the row and issue the lease in one transaction
//!    ([`crate::lease::LeaseManager::issue_with_workspace`]).
//!
//! A failure after step 4 tears the directory down; after step 5 also
//! forgets the jj workspace and removes its state directory. Partial state
//! never leaves a row behind.

use std::cell::Cell;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures::FutureExt as _;
use reef_lib::error::ErrorChain;
use reef_lib::jj::{
    ChangeSummary, CommitIdHex, CreateWorkspaceError, MaterializeError, Repo, ResolveError,
    ResolvedVia, StartPoint,
};
use reef_lib::spec::SPEC_FILE;
use reef_lib::telemetry::elapsed_ms;

use super::manager::{forget_jj_workspace, remove_state_dir};
use super::{CreateSpec, Created, Manager, ManagerError, SnapshotSpec, Start, WorkspaceEntry};
use crate::backend::{Backend, BackendChoice, BackendError, BaseEntry, WorkspacePaths, select};
use crate::model::{RepoId, Timestamp, WorkspaceId, WorkspaceRecord, WorkspaceState};
use crate::repo::RepoHandle;
use crate::spec::ResolvedSpec;

/// The description of a colocated repository's ref import before a create.
const IMPORT_DESCRIPTION: &str = "reef: import git refs";

/// Step 2's answer.
struct ResolvedStart {
    start: StartPoint,
    commit: CommitIdHex,
    tree_key: String,
    shared_change: bool,
    spec: ResolvedSpec,
}

impl Manager {
    /// Creates a workspace; see the module docs for the steps.
    #[tracing::instrument(skip_all, fields(repo = %spec.repo, workspace_id))]
    pub async fn create(&self, spec: CreateSpec) -> Result<Created, ManagerError> {
        // 1.
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

        // 2.
        let resolved = resolve_start(&handle, spec.start).await?;
        let length = self
            .leases
            .length_with_spec(spec.lease, resolved.spec.lease)?;

        // 3.
        let begun = Instant::now();
        let (base, base_was_cached) = self.ensure_base(&handle, &resolved, &*backend).await?;

        // 4.
        let mut undo = Undo::new(handle.clone(), id);
        materialize(backend, &base, &paths, &mut undo).await?;
        let materialize = begun.elapsed();

        // 5.
        let (ws, op_id, wc) =
            match create_jj_workspace(&handle, id, &paths, resolved.start.clone(), wc_settings)
                .await
            {
                Ok(created) => created,
                Err(error) => {
                    undo.run().await;
                    return Err(error);
                }
            };
        undo.jj_workspace = true;
        undo.state_dir = Some(paths.state_dir.clone());

        // 6.
        let ws = match seed(ws, &base, &paths, &wc.commit_id).await {
            Ok(ws) => ws,
            Err(error) => {
                undo.run().await;
                return Err(error);
            }
        };

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
            attention: None,
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
        self.load_created(
            &record,
            &mut undo,
            ws,
            handle,
            resolved.spec.snapshot.clone(),
        );
        tracing::info!(
            path = %record.path.display(),
            backend = %record.backend,
            change_id = %record.change_id,
            %op_id,
            shared_change = resolved.shared_change,
            stores = resolved.spec.spec.stores.len(),
            base_was_cached,
            materialize_ms = u64::try_from(materialize.as_millis()).unwrap_or(u64::MAX),
            "workspace created"
        );
        Ok(Created {
            record,
            lease,
            op_id,
            shared_change: resolved.shared_change,
            materialize,
            base_was_cached,
        })
    }

    /// The end of step 7: loads the new workspace, taking the backend from
    /// `undo`, and starts scheduling its snapshots.
    fn load_created(
        &self,
        record: &WorkspaceRecord,
        undo: &mut Undo,
        ws: Repo,
        handle: RepoHandle,
        spec: SnapshotSpec,
    ) {
        let Some((backend, paths)) = undo.backend.take() else {
            unreachable!("set after step 4")
        };
        let entry = WorkspaceEntry {
            record: Mutex::new(record.clone()),
            paths,
            backend,
            jj: Arc::new(Mutex::new(Some(ws))),
            repo: handle,
            scan_lock: tokio::sync::Mutex::new(()),
            spec: Mutex::new(spec),
        };
        self.insert_entry(record.id, Arc::new(entry));
        self.scheduler.track(record.id);
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

    /// Step 3: the base for the resolved commit's tree, and whether it was
    /// already cached. A cold materialization runs on the repository
    /// thread, under the base cache's lock for the tree. If the backend has
    /// a [`DirCloner`](crate::backend::DirCloner) and the cache another base, the most recently used one is cloned
    /// and only the diff checked out; if that fails, the whole tree is.
    async fn ensure_base(
        &self,
        handle: &RepoHandle,
        resolved: &ResolvedStart,
        backend: &dyn Backend,
    ) -> Result<(BaseEntry, bool), ManagerError> {
        let cache = self.base_cache(handle.id());
        let cloner = backend.dir_cloner();
        let handle = handle.clone();
        let commit = resolved.commit.clone();
        let key = resolved.tree_key.clone();
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

/// Step 2, on the repository thread.
async fn resolve_start(handle: &RepoHandle, start: Start) -> Result<ResolvedStart, ManagerError> {
    handle
        .call_async(move |head| {
            async move {
                // Operations the `jj` CLI wrote on the registered root
                // (bookmarks, new changes) count.
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
                        // `main` is neither hex nor reverse hex, so it can
                        // only resolve as a bookmark.
                        let commit = match head.repo.resolve_revision("main").await {
                            Ok(resolved) if resolved.via == ResolvedVia::Bookmark => {
                                resolved.commit_id
                            }
                            Ok(_) | Err(ResolveError::NoMatch(_)) => {
                                head.repo.wc_change()?.commit_id
                            }
                            Err(error) => return Err(error.into()),
                        };
                        (StartPoint::NewOn(commit.clone()), commit)
                    }
                };
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
                Ok::<_, ManagerError>(ResolvedStart {
                    start,
                    commit,
                    tree_key,
                    shared_change,
                    spec: crate::spec::resolve(spec),
                })
            }
            .boxed_local()
        })
        .await?
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
async fn seed(
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
}

impl Undo {
    fn new(handle: RepoHandle, id: WorkspaceId) -> Self {
        Self {
            handle,
            id,
            backend: None,
            jj_workspace: false,
            state_dir: None,
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
