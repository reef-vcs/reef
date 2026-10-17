//! [`Manager::create`]: from a request to a leased directory.
//!
//! The steps, and the thread each runs on:
//!
//! 1. (async) Validate, resolve the repository, allocate the id, compute the
//!    paths, select the backend (probing on the blocking pool).
//! 2. (repository thread) Reload at head; in a colocated repository import
//!    git refs; resolve the start revision; note whether another Reef
//!    workspace already has the edited commit.
//! 3. (blocking, then the repository thread) Ensure the base: the base
//!    cache's per-tree lock, then `Repo::materialize_tree` on the repository
//!    thread if the tree is not cached.
//! 4. (blocking) The backend materializes the workspace from the base.
//! 5. (repository thread) `Repo::create_workspace`.
//! 6. (blocking) Seed the working copy's `tree_state` from the base's, or
//!    fall back to `Repo::reset_to`.
//! 7. (async) Insert the row and issue the lease in one transaction.
//!
//! A failure after step 4 tears the directory down; after step 5 also
//! forgets the jj workspace and removes its state directory. Partial state
//! never leaves a row behind.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use reef_lib::error::ErrorChain;
use reef_lib::jj::{
    ChangeSummary, CommitIdHex, CreateWorkspaceError, MaterializeError, Repo, ResolveError,
    ResolvedVia, StartPoint,
};
use reef_lib::telemetry::elapsed_ms;

use super::manager::{forget_jj_workspace, remove_state_dir};
use super::{CreateSpec, Created, Manager, ManagerError, Start, WorkspaceEntry, millis};
use crate::backend::{Backend, BackendError, BaseEntry, WorkspacePaths, select};
use crate::model::{
    LeaseId, LeaseRecord, RepoId, Timestamp, WorkspaceId, WorkspaceRecord, WorkspaceState,
};
use crate::repo::RepoHandle;

/// The description of a colocated repository's ref import before a create.
const IMPORT_DESCRIPTION: &str = "reef: import git refs";

/// Step 2's answer.
struct ResolvedStart {
    start: StartPoint,
    commit: CommitIdHex,
    tree_key: String,
    shared_change: bool,
}

impl Manager {
    /// Creates a workspace; see the module docs for the steps.
    #[tracing::instrument(skip_all, fields(repo = %spec.repo, workspace_id))]
    pub async fn create(&self, spec: CreateSpec) -> Result<Created, ManagerError> {
        // 1.
        if spec.owner.trim().is_empty() {
            return Err(ManagerError::EmptyOwner);
        }
        let lease_length = self.lease_length(spec.lease)?;
        let repo = self.db.repository(spec.repo)?;
        let handle = self.handle(repo.id)?;
        let id = WorkspaceId::generate();
        tracing::Span::current().record("workspace_id", tracing::field::display(id));
        let paths = self.new_paths(repo.id, id, spec.path.as_deref())?;
        let choice = spec.backend.unwrap_or(self.config.file.workspace.backend);
        let probe_dir = paths
            .root
            .parent()
            .map_or_else(|| paths.root.clone(), Path::to_owned);
        let backend = tokio::task::spawn_blocking(move || select(choice, &probe_dir)).await??;

        // 2.
        let resolved = resolve_start(&handle, spec.start).await?;

        // 3.
        let begun = Instant::now();
        let (base, base_was_cached) = self.ensure_base(&handle, repo.id, &resolved).await?;

        // 4.
        let mut undo = Undo::new(handle.clone(), id);
        materialize(backend, &base, &paths, &mut undo).await?;
        let materialize = begun.elapsed();

        // 5.
        let (ws, op_id, wc) =
            match create_jj_workspace(&handle, id, &paths, resolved.start.clone()).await {
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
        let lease = self.new_lease(repo.id, id, &spec.owner, lease_length, now);
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
        };
        if let Err(error) = self.db.insert_workspace_with_lease(&record, &lease) {
            drop(ws);
            undo.run().await;
            return Err(error.into());
        }
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
        };
        self.insert_entry(id, Arc::new(entry));
        tracing::info!(
            path = %record.path.display(),
            backend = %record.backend,
            change_id = %record.change_id,
            %op_id,
            shared_change = resolved.shared_change,
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

    /// The lease length for a request: `lease.default` when none (or zero)
    /// is asked for; above `lease.max` is refused.
    fn lease_length(&self, requested: Option<Duration>) -> Result<Duration, ManagerError> {
        let lease = &self.config.file.lease;
        match requested {
            None => Ok(lease.default),
            Some(length) if length.is_zero() => Ok(lease.default),
            Some(length) if length > lease.max => Err(ManagerError::LeaseTooLong {
                requested: length,
                max: lease.max,
            }),
            Some(length) => Ok(length),
        }
    }

    /// The first lease of a new workspace, issued `now`.
    fn new_lease(
        &self,
        repo: RepoId,
        id: WorkspaceId,
        owner: &str,
        length: Duration,
        now: Timestamp,
    ) -> LeaseRecord {
        LeaseRecord {
            id: LeaseId::generate(),
            repo_id: repo,
            workspace_id: id,
            owner: owner.to_owned(),
            issued_at: now,
            expires_at: Timestamp(now.0.saturating_add(millis(length))),
            last_heartbeat_at: now,
            length,
            grace: self.config.file.lease.grace,
            ended_at: None,
            end_reason: None,
        }
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
    /// thread, under the base cache's lock for the tree.
    async fn ensure_base(
        &self,
        handle: &RepoHandle,
        repo: RepoId,
        resolved: &ResolvedStart,
    ) -> Result<(BaseEntry, bool), ManagerError> {
        let cache = self.base_cache(repo);
        let handle = handle.clone();
        let commit = resolved.commit.clone();
        let key = resolved.tree_key.clone();
        tokio::task::spawn_blocking(move || {
            let mut cached = true;
            let mut thread_error = None;
            let ensured = cache.ensure(&key, |dir, state| {
                cached = false;
                let (dir, state) = (dir.to_owned(), state.to_owned());
                let at = dir.clone();
                let job = handle.call_async(move |head| {
                    async move { head.repo.materialize_tree(&commit, &dir, &state).await }
                        .boxed_local()
                });
                match pollster::block_on(job) {
                    Ok(materialized) => materialized,
                    Err(error) => {
                        let message = error.to_string();
                        thread_error = Some(error);
                        Err(MaterializeError::Io {
                            op: "materialize on the repository thread",
                            path: at,
                            source: io::Error::other(message),
                        })
                    }
                }
            });
            match (ensured, thread_error) {
                (Err(_), Some(error)) => Err(error.into()),
                (ensured, _) => Ok((ensured?, cached)),
            }
        })
        .await?
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
                Ok::<_, ManagerError>(ResolvedStart {
                    start,
                    commit,
                    tree_key,
                    shared_change,
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
                    .create_workspace(&name, &root, &state_dir, start, &description)
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
