//! Garbage collection of a repository's operation log, index, and git
//! objects (R1.17).
//!
//! jj keeps every operation, and every commit an operation's view or
//! predecessor records names, reachable: the index is rebuilt from the
//! reachable operations, and `git gc` keeps whatever the index reports as a
//! head. A hidden snapshot commit is therefore freed only once the old
//! operations that name it are unreachable. [`Repo::gc`] does that first,
//! by rewriting the newer operations onto the root operation, then runs
//! jj's own collectors in an order derived from their semantics (jj-cli's
//! `util gc` was not checked).

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use futures::StreamExt as _;
use jj_lib::backend::BackendError;
use jj_lib::default_index::{DefaultIndexStore, DefaultIndexStoreInitError};
use jj_lib::git_backend::GitBackend;
use jj_lib::op_heads_store::OpHeadsStoreError;
use jj_lib::op_store::{OpStoreError, OperationId};
use jj_lib::op_walk::{reparent_range, walk_ancestors};
use jj_lib::operation::Operation;
use jj_lib::repo::{Repo as _, RepoLoaderError};
use jj_lib::working_copy::WorkingCopyStateError;

use super::oplog::system_time;
use super::repo::{OpenError, OperationIdHex, ReadError, Repo};
use crate::error::{Classify, ErrorKind, classify_by_source};

/// What [`Repo::gc`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcOutcome {
    /// The head operation afterwards: a rewrite of the head before, with the
    /// same view, when anything was abandoned; else that head itself.
    pub new_head: OperationIdHex,
    /// Operations made unreachable by the rewrite.
    pub operations_abandoned: usize,
    /// Rebuilding the index from the surviving operations.
    pub reindex: Duration,
    /// jj's backend GC: its no-gc refs and `git gc`.
    pub git_gc: Duration,
}

/// Failure of [`Repo::gc`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GcError {
    /// The operation store could not be read, written, or collected.
    #[error("operation store")]
    OpStore(#[from] OpStoreError),
    /// The operation heads could not be read or moved.
    #[error("operation heads")]
    OpHeads(#[from] OpHeadsStoreError),
    /// The index could not be reset for rebuilding.
    #[error("reset the index")]
    Index(#[from] DefaultIndexStoreInitError),
    /// The backend's GC failed: most often `git gc`, which needs the
    /// external `git` (`git.executable-path`).
    #[error("backend GC")]
    Backend(#[from] BackendError),
    /// The store is not git-backed. Phases 1 to 3 are (ADR 0011); the
    /// variant exists so another backend fails loudly.
    #[error("the repository's store is not git-backed")]
    NotGitBackend,
    /// The repository could not be reloaded at head.
    #[error("reload the repository")]
    Reload(#[from] OpenError),
}

impl From<RepoLoaderError> for GcError {
    fn from(error: RepoLoaderError) -> Self {
        Self::Reload(OpenError::Repo(error))
    }
}

impl Classify for GcError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::OpStore(error) => classify_by_source(error),
            Self::OpHeads(error) => classify_by_source(error),
            Self::Index(error) => classify_by_source(error),
            Self::Backend(error) => classify_by_source(error),
            Self::NotGitBackend => ErrorKind::Internal,
            Self::Reload(error) => error.kind(),
        }
    }
}

/// Failure to compare a working copy with the loaded view.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WorkingCopyCheckError {
    /// The working-copy state could not be read.
    #[error("read the working-copy state")]
    State(#[from] WorkingCopyStateError),
    /// The view's working-copy commit could not be read.
    #[error(transparent)]
    Read(#[from] ReadError),
}

impl Classify for WorkingCopyCheckError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::State(error) => classify_by_source(error),
            Self::Read(error) => error.kind(),
        }
    }
}

impl Repo {
    /// Abandons the operations older than `keep_newer`, then runs the
    /// operation store's GC, rebuilds the index, and runs the backend's GC,
    /// in that order. Reloads at head twice. Returns the new head.
    ///
    /// Abandoning: walking the head's ancestors newest first, the first
    /// operation O that ended before `keep_newer` is cut off, with all its
    /// ancestors, by rewriting `O..head` onto the root operation
    /// (`reparent_range`) and making the rewrite the op head. The head is
    /// always in the rewritten range, so its view survives with any
    /// `keep_newer`. The rewritten operations get new ids: a working copy
    /// last updated at an abandoned operation no longer loads, so the caller
    /// moves each one it owns to the new head ([`Repo::rebind_working_copy`])
    /// and leaves others (the user's `default`) for `jj workspace
    /// update-stale`.
    ///
    /// Nothing newer than `keep_newer` is deleted: unreachable operation
    /// and view files and git objects are pruned by mtime. If a step after
    /// the op heads move fails, this [`Repo`] is still reloaded at the new
    /// head (best effort) before the error returns, so the caller can rebind
    /// working copies to [`Repo::op_id`] either way.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn gc(&mut self, keep_newer: SystemTime) -> Result<GcOutcome, GcError> {
        let mut moved = false;
        let result = self.gc_unlogged(keep_newer, &mut moved).await;
        if result.is_err() && moved {
            match self.repo.loader().load_at_head().await {
                Ok(repo) => self.repo = repo,
                Err(error) => tracing::error!(
                    error = %crate::error::ErrorChain(&error),
                    "cannot reload at the new head after a failed GC"
                ),
            }
        }
        let outcome = result.inspect_err(|error| crate::log_failure!(error))?;
        tracing::info!(
            op_id = %outcome.new_head,
            abandoned_count = outcome.operations_abandoned,
            reindex_ms = u64::try_from(outcome.reindex.as_millis()).unwrap_or(u64::MAX),
            git_gc_ms = u64::try_from(outcome.git_gc.as_millis()).unwrap_or(u64::MAX),
            "repository garbage collected"
        );
        Ok(outcome)
    }

    async fn gc_unlogged(
        &mut self,
        keep_newer: SystemTime,
        moved: &mut bool,
    ) -> Result<GcOutcome, GcError> {
        let loader = self.repo.loader().clone();
        // Merges divergent operation heads first, so there is one head.
        let head_repo = loader.load_at_head().await?;
        let head = head_repo.operation().clone();
        self.repo = head_repo;

        let mut operations_abandoned = 0;
        let mut new_head = head.id().clone();
        if let Some(cut) = first_older_than(&head, keep_newer).await?
            && cut.id() != loader.op_store().root_operation_id()
        {
            let root = loader.root_operation().await;
            let stats = reparent_range(
                loader.op_store().as_ref(),
                std::slice::from_ref(&cut),
                std::slice::from_ref(&head),
                &root,
            )
            .await?;
            if let Some(id) = stats.new_head_ids.first() {
                new_head = id.clone();
            }
            loader
                .op_heads_store()
                .update_op_heads(&[head.id().clone()], &new_head)
                .await?;
            *moved = true;
            operations_abandoned = stats.unreachable_count;
            tracing::debug!(
                rewritten_count = stats.rewritten_count,
                abandoned_count = stats.unreachable_count,
                "old operations abandoned"
            );
        }

        // Every current head, in case another process wrote one meanwhile.
        let heads = loader.op_heads_store().get_op_heads().await?;
        loader.op_store().gc(&heads, keep_newer).await?;

        if let Some(index_store) = self.repo.index_store().downcast_ref::<DefaultIndexStore>() {
            index_store.reinit()?;
        } else {
            tracing::warn!("the index is not jj's default; it is not rebuilt");
        }
        let reindex_start = Instant::now();
        self.repo = loader.load_at_head().await?;
        let reindex = reindex_start.elapsed();
        tracing::info!(
            reindex_ms = u64::try_from(reindex.as_millis()).unwrap_or(u64::MAX),
            "index rebuilt from the surviving operations"
        );

        let git_gc_start = Instant::now();
        let git = self
            .repo
            .store()
            .backend_impl::<GitBackend>()
            .ok_or(GcError::NotGitBackend)?;
        git.gc(self.repo.readonly_index().as_index(), keep_newer)?;
        Ok(GcOutcome {
            new_head: OperationIdHex::of(&new_head),
            operations_abandoned,
            reindex,
            git_gc: git_gc_start.elapsed(),
        })
    }

    /// Sets this workspace's working-copy operation to `op_id` without
    /// touching its files or its recorded tree (`LockedWorkspace::finish`
    /// with no other change). Used after [`Repo::gc`] for workspaces whose
    /// working copy matches the head view
    /// ([`Repo::working_copy_matches_view`]); for any other it would hide
    /// their staleness.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), op_id = %op_id))]
    pub async fn rebind_working_copy(
        &mut self,
        op_id: &OperationIdHex,
    ) -> Result<(), WorkingCopyStateError> {
        let rebound = async {
            let id =
                OperationId::try_from_hex(op_id.as_str()).ok_or_else(|| WorkingCopyStateError {
                    message: format!("{op_id} is not an operation id"),
                    err: "invalid operation id".into(),
                })?;
            let locked = self.workspace.start_working_copy_mutation().await?;
            locked.finish(id).await
        };
        rebound.await.inspect_err(|error| {
            tracing::error!(error = %crate::error::ErrorChain(error), "cannot rebind the working copy");
        })
    }

    /// Whether this workspace's recorded tree is its working-copy commit's
    /// tree in the loaded view: the files snapshotted last are the commit,
    /// so moving the working copy to the loaded operation hides nothing.
    /// False when the view has no working-copy commit for it.
    pub fn working_copy_matches_view(&self) -> Result<bool, WorkingCopyCheckError> {
        let Some(id) = self
            .repo
            .view()
            .get_wc_commit_id(self.workspace.workspace_name())
        else {
            return Ok(false);
        };
        let commit = self.repo.store().get_commit(id).map_err(ReadError::from)?;
        let recorded = self.workspace.working_copy().tree()?;
        Ok(recorded.tree_ids_and_labels() == commit.tree().tree_ids_and_labels())
    }

    /// The directories [`Repo::gc`] shrinks, for measuring it: the operation
    /// store (`.jj/repo/op_store`) and the git objects directory
    /// (`.jj/repo/store/git/objects`, or `<root>/.git/objects` when
    /// colocated).
    pub fn gc_measured_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.repo_path().join("op_store")];
        if let Ok(git) = jj_lib::git::get_git_backend(self.repo.store()) {
            dirs.push(git.git_repo_path().join("objects"));
        }
        dirs
    }

    /// The operation this workspace's working copy was last updated at.
    pub fn working_copy_op_id(&self) -> OperationIdHex {
        OperationIdHex::of(self.workspace.working_copy().operation_id())
    }
}

/// The newest strict ancestor of `head` that ended before `keep_newer`.
async fn first_older_than(
    head: &Operation,
    keep_newer: SystemTime,
) -> Result<Option<Operation>, OpStoreError> {
    let mut ancestors = std::pin::pin!(walk_ancestors(std::slice::from_ref(head)));
    while let Some(op) = ancestors.next().await {
        let op = op?;
        if op.id() != head.id() && system_time(op.metadata().time.end) < keep_newer {
            return Ok(Some(op));
        }
    }
    Ok(None)
}
