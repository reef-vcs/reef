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
//!
//! The rewrite gives every kept operation a new id, so a working copy that
//! names an old one no longer loads. The caller moves Reef's own; GC moves
//! the user's jj workspaces itself, under their working-copy locks (issue
//! #94, ADR 0040).

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use futures::{StreamExt as _, TryStreamExt as _};
use jj_lib::backend::BackendError;
use jj_lib::default_index::{DefaultIndexStore, DefaultIndexStoreInitError};
use jj_lib::git_backend::GitBackend;
use jj_lib::local_working_copy::{LocalWorkingCopy, TreeState, TreeStateSettings};
use jj_lib::lock::{FileLock, FileLockError};
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_heads_store::OpHeadsStoreError;
use jj_lib::op_store::{OpStore, OpStoreError, OpStoreResult, OperationId};
use jj_lib::op_walk::{walk_ancestors, walk_ancestors_range};
use jj_lib::operation::Operation;
use jj_lib::protos::local_working_copy::Checkout;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::{ReadonlyRepo, Repo as _, RepoLoader, RepoLoaderError};
use jj_lib::working_copy::{WorkingCopyFreshness, WorkingCopyStateError};
use jj_lib::workspace::{DefaultWorkspaceLoaderFactory, Workspace, WorkspaceLoaderFactory as _};
use prost::Message as _;

use super::oplog::system_time;
use super::repo::{OpenError, OperationIdHex, ReadError, Repo};
use super::workspace::REEF_WORKSPACE_PREFIX;
use crate::error::{Classify, ErrorChain, ErrorKind, classify_by_source};

/// How long [`Repo::gc`] waits for a `jj` command to release one of the
/// user's jj workspaces before it abandons nothing. It bounds a snapshot in
/// progress, not a policy.
pub const CHECKOUT_LOCK_WAIT: Duration = Duration::from_secs(2);

/// How often a busy working-copy lock is tried again.
const CHECKOUT_LOCK_POLL: Duration = Duration::from_millis(50);

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
    /// What GC did with each of the user's jj workspaces (every one not
    /// named `reef-*`), in name order. Empty when `abandon_blocked`.
    pub checkouts: Vec<CheckoutMove>,
    /// Why nothing was abandoned although operations were old enough; the
    /// collection of what was already unreachable still ran.
    /// `operations_abandoned` is 0 when set.
    pub abandon_blocked: Option<AbandonBlocked>,
    /// Each operation the rewrite kept, its old id to its new one, sorted
    /// by old id; empty when nothing was abandoned. Indexes that name
    /// operations map their ids through it (R2.16).
    pub rewritten: Vec<(OperationIdHex, OperationIdHex)>,
}

/// What [`Repo::gc`] did with one of the user's jj workspaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckoutMove {
    /// The workspace's name, `default` for the registered root.
    pub workspace: String,
    /// Its directory, or empty when jj has none recorded.
    pub path: PathBuf,
    /// The operation its working copy named before GC, when read.
    pub from: Option<OperationIdHex>,
    /// What GC did.
    pub outcome: CheckoutOutcome,
}

/// What [`Repo::gc`] did with a jj workspace's working-copy operation.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CheckoutOutcome {
    /// Its operation was rewritten; the working copy now names the rewrite,
    /// which has the same view, so jj sees exactly what it saw before.
    Rewritten {
        /// The rewritten operation.
        to: OperationIdHex,
    },
    /// Its operation was abandoned, by this GC or an earlier one, and its
    /// files are its commit's at head: it now names the head.
    ToHead {
        /// The head.
        to: OperationIdHex,
    },
    /// Its operation is kept as it is (the root operation, or nothing was
    /// rewritten).
    Unchanged,
    /// Its operation was abandoned and its files are not its commit's at
    /// head. The operation was deleted, so `jj workspace update-stale`
    /// there takes jj's recovery path (a recovery commit, nothing lost).
    LeftForRecovery,
    /// GC could not hold it and left it alone, as before issue #94.
    Skipped {
        /// Why.
        reason: String,
    },
}

/// Why [`Repo::gc`] abandoned no operations.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AbandonBlocked {
    /// A `jj` command held a user's jj workspace for [`CHECKOUT_LOCK_WAIT`].
    Locked {
        /// The workspace's name.
        workspace: String,
        /// Its directory.
        path: PathBuf,
    },
}

impl fmt::Display for AbandonBlocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Locked { workspace, path } => write!(
                f,
                "jj workspace `{workspace}` at {} is locked by another process (a running `jj` command)",
                path.display()
            ),
        }
    }
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
    /// One of the user's jj workspaces could not be moved onto the rewritten
    /// operation log. Nothing was collected, so its old operation still
    /// loads for `jj workspace update-stale`.
    #[error("move jj workspace `{workspace}` at {}", path.display())]
    Checkout {
        /// The workspace's name.
        workspace: String,
        /// Its directory.
        path: PathBuf,
        /// The write or removal that failed.
        #[source]
        source: io::Error,
    },
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
            Self::Checkout { source, .. } => classify_by_source(source),
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
    /// The operation log could not be read to place the working copy.
    #[error("read the operation log")]
    OpStore(#[source] OpStoreError),
}

impl Classify for WorkingCopyCheckError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::State(error) => classify_by_source(error),
            Self::Read(error) => error.kind(),
            Self::OpStore(error) => classify_by_source(error),
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
    /// `keep_newer`. The rewritten operations get new ids, and a working
    /// copy that names an old one no longer loads. The caller moves Reef's
    /// own (`reef-*`) working copies to the new head, in
    /// [`Repo::gc_with`]'s callback. GC moves every other jj workspace it
    /// can find (the root's `default`, any `jj workspace add`) itself,
    /// holding its working-copy lock from before the head is loaded until
    /// the op heads have moved: one whose operation was rewritten names
    /// the rewrite; one whose operation is abandoned names the new head if
    /// its files are its commit's there, and otherwise loses that
    /// operation, so jj's recovery for a missing operation applies
    /// ([`CheckoutOutcome`]). While a `jj` command holds one of those locks
    /// for [`CHECKOUT_LOCK_WAIT`], nothing is abandoned
    /// ([`GcOutcome::abandon_blocked`]). Call it on the registered root.
    ///
    /// Nothing newer than `keep_newer` is deleted: unreachable operation
    /// and view files and git objects are pruned by mtime. If a step after
    /// the op heads move fails, this [`Repo`] is still reloaded at the new
    /// head (best effort) before the error returns; a failed move of a
    /// user's workspace ([`GcError::Checkout`]) skips the collection.
    pub async fn gc(&mut self, keep_newer: SystemTime) -> Result<GcOutcome, GcError> {
        self.gc_with(keep_newer, &mut |_, _| {}).await
    }

    /// [`Repo::gc`], calling `abandoned` with the head it rewrote and the
    /// new head right after the op heads move to it, before anything is collected: where the caller
    /// moves its working copies there ([`Repo::rebind_working_copy`]), so
    /// no failure or crash in the collection that follows can strand them.
    /// Not called when nothing is abandoned.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn gc_with(
        &mut self,
        keep_newer: SystemTime,
        abandoned: &mut dyn FnMut(&OperationIdHex, &OperationIdHex),
    ) -> Result<GcOutcome, GcError> {
        let mut moved = false;
        let result = self.gc_unlogged(keep_newer, abandoned, &mut moved).await;
        if result.is_err() && moved {
            match self.repo.loader().load_at_head().await {
                Ok(repo) => self.repo = repo,
                Err(error) => tracing::error!(
                    error = %ErrorChain(&error),
                    "cannot reload at the new head after a failed GC"
                ),
            }
        }
        let outcome = result.inspect_err(|error| crate::log_failure!(error))?;
        if let Some(blocked) = &outcome.abandon_blocked {
            tracing::warn!("no operations were abandoned: {blocked}");
        }
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
        abandoned: &mut dyn FnMut(&OperationIdHex, &OperationIdHex),
        moved: &mut bool,
    ) -> Result<GcOutcome, GcError> {
        let loader = self.repo.loader().clone();
        let Holding {
            head: head_repo,
            held,
            skipped: mut checkouts,
            blocked,
        } = self.hold_checkouts(&loader).await?;
        let head = head_repo.operation().clone();
        self.repo = head_repo;

        let mut operations_abandoned = 0;
        let mut new_head = head.id().clone();
        let cut = first_older_than(&head, keep_newer)
            .await?
            .filter(|cut| cut.id() != loader.op_store().root_operation_id());
        // A busy lock matters only when there is something to abandon.
        let abandon_blocked = blocked.filter(|_| cut.is_some());
        let cut = cut.filter(|_| abandon_blocked.is_none());
        let mut rewritten = None;
        let mut failed = None;
        if let Some(cut) = cut {
            let root = loader.root_operation().await;
            let reparented =
                reparent_mapped(loader.op_store().as_ref(), &cut, &head, &root).await?;
            new_head = reparented.new_head.clone();
            loader
                .op_heads_store()
                .update_op_heads(&[head.id().clone()], &new_head)
                .await?;
            *moved = true;
            failed = self.move_checkouts(&held, &reparented.rewritten, &new_head, &mut checkouts);
            operations_abandoned = reparented.unreachable_count;
            tracing::debug!(
                rewritten_count = reparented.rewritten.len(),
                abandoned_count = reparented.unreachable_count,
                "old operations abandoned"
            );
            rewritten = Some(reparented.rewritten);
        } else if !held.is_empty() {
            // Nothing is rewritten: every ancestor of the head stays, and
            // only a workspace an earlier GC stranded moves.
            let mut kept = HashMap::new();
            let mut ancestors = std::pin::pin!(walk_ancestors(std::slice::from_ref(&head)));
            while let Some(op) = ancestors.next().await {
                let id = op?.id().clone();
                kept.insert(id.clone(), id);
            }
            failed = self.move_checkouts(&held, &kept, &new_head, &mut checkouts);
        }
        // Releases the locks.
        let places: Vec<CheckoutPlace> = held.into_iter().map(CheckoutPlace::from).collect();
        if *moved {
            abandoned(
                &OperationIdHex::of(head.id()),
                &OperationIdHex::of(&new_head),
            );
        }
        if let Some(error) = failed {
            return Err(error);
        }
        if let Some(rewritten) = &rewritten {
            self.recheck_checkouts(&places, rewritten, &mut checkouts)?;
        }
        checkouts.sort_by(|a, b| a.workspace.cmp(&b.workspace));

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
        let mut rewritten: Vec<(OperationIdHex, OperationIdHex)> = rewritten
            .unwrap_or_default()
            .iter()
            .map(|(old, new)| (OperationIdHex::of(old), OperationIdHex::of(new)))
            .collect();
        rewritten.sort();
        Ok(GcOutcome {
            new_head: OperationIdHex::of(&new_head),
            operations_abandoned,
            reindex,
            git_gc: git_gc_start.elapsed(),
            checkouts,
            abandon_blocked,
            rewritten,
        })
    }

    /// Loads at head with every user's jj workspace in its view locked or
    /// skipped. The locks are taken before the load GC classifies them at,
    /// so no `jj` command snapshots in between; a workspace added meanwhile
    /// is locked in the next round. Locks nothing if one stays busy.
    async fn hold_checkouts(&self, loader: &RepoLoader) -> Result<Holding, GcError> {
        // Merges divergent operation heads first, so there is one head.
        let mut at_head = loader.load_at_head().await?;
        let deadline = Instant::now() + CHECKOUT_LOCK_WAIT;
        let mut held: Vec<HeldCheckout> = Vec::new();
        let mut skipped: Vec<CheckoutMove> = Vec::new();
        loop {
            let pending: Vec<String> = user_workspace_names(&at_head)
                .into_iter()
                .filter(|name| {
                    !held.iter().any(|checkout| checkout.name == *name)
                        && !skipped.iter().any(|checkout| checkout.workspace == *name)
                })
                .collect();
            if pending.is_empty() {
                break;
            }
            for name in pending {
                match self.hold_checkout(&name, deadline) {
                    Ok(checkout) => held.push(checkout),
                    Err(Unheld::Skipped { path, reason }) => {
                        tracing::warn!(
                            path = %path.display(),
                            "jj workspace `{name}` not moved by GC ({reason}); it may need `jj workspace update-stale`"
                        );
                        skipped.push(CheckoutMove {
                            workspace: name,
                            path,
                            from: None,
                            outcome: CheckoutOutcome::Skipped { reason },
                        });
                    }
                    Err(Unheld::Busy { path }) => {
                        // Dropping `held` releases the others.
                        return Ok(Holding {
                            head: at_head,
                            held: Vec::new(),
                            skipped: Vec::new(),
                            blocked: Some(AbandonBlocked::Locked {
                                workspace: name,
                                path,
                            }),
                        });
                    }
                }
            }
            at_head = loader.load_at_head().await?;
        }
        held.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Holding {
            head: at_head,
            held,
            skipped,
            blocked: None,
        })
    }

    /// Finds and locks the user's jj workspace `name`, polling a busy lock
    /// until `deadline`. One that is not this repository's local working
    /// copy at a directory jj recorded is skipped.
    fn hold_checkout(&self, name: &str, deadline: Instant) -> Result<HeldCheckout, Unheld> {
        let skip = |path: &Path, reason: String| Unheld::Skipped {
            path: path.to_owned(),
            reason,
        };
        let dir = if name == self.workspace_name() {
            self.workspace_root().to_owned()
        } else {
            let workspace = WorkspaceNameBuf::from(name);
            match self
                .repo
                .loader()
                .workspace_store()
                .get_workspace_path(&workspace)
            {
                Ok(Some(dir)) => dir,
                Ok(None) => return Err(skip(Path::new(""), "jj has no path for it".into())),
                Err(error) => {
                    return Err(skip(
                        Path::new(""),
                        format!("cannot read jj's workspace store: {}", ErrorChain(&error)),
                    ));
                }
            }
        };
        let state_path = dir.join(".jj").join("working_copy");
        if !state_path.join("checkout").is_file() {
            return Err(skip(&dir, "it has no jj checkout".into()));
        }
        let workspace_loader = DefaultWorkspaceLoaderFactory
            .create(&dir)
            .map_err(|error| skip(&dir, format!("cannot load it: {}", ErrorChain(&error))))?;
        let same_repo = match (
            std::fs::canonicalize(workspace_loader.repo_path()),
            std::fs::canonicalize(self.repo_path()),
        ) {
            (Ok(theirs), Ok(ours)) => theirs == ours,
            _ => false,
        };
        if !same_repo {
            return Err(skip(&dir, "it belongs to another repository".into()));
        }
        match workspace_loader.get_working_copy_type() {
            Ok(kind) if kind == LocalWorkingCopy::NAME => {}
            Ok(kind) => {
                return Err(skip(
                    &dir,
                    format!("its working copy is {kind:?}, not local"),
                ));
            }
            Err(error) => {
                return Err(skip(
                    &dir,
                    format!("cannot read its working-copy type: {}", ErrorChain(&error)),
                ));
            }
        }
        // Checked before locking too: a directory that another name in the
        // view now uses (removed and added again) is that name's, and its lock
        // may already be GC's own.
        let names_this = |checkout: &Checkout| {
            if checkout.workspace_name.is_empty() || checkout.workspace_name == name {
                Ok(())
            } else {
                Err(skip(
                    &dir,
                    format!("its checkout names workspace `{}`", checkout.workspace_name),
                ))
            }
        };
        let read = |what: &str| {
            read_checkout(&state_path)
                .map_err(|error| skip(&dir, format!("cannot read its checkout {what}: {error}")))
        };
        names_this(&read("before locking")?)?;
        let lock = match lock_checkout(&state_path, deadline) {
            Ok(Some(lock)) => lock,
            Ok(None) => return Err(Unheld::Busy { path: dir }),
            Err(error) => {
                return Err(skip(
                    &dir,
                    format!("cannot lock it: {}", ErrorChain(&error)),
                ));
            }
        };
        // Read under the lock: the operation GC classifies.
        let checkout = read("under its lock")?;
        names_this(&checkout)?;
        let id_len = self.repo.op_store().root_operation_id().as_bytes().len();
        if checkout.operation_id.len() != id_len {
            return Err(skip(&dir, "its checkout names no valid operation".into()));
        }
        Ok(HeldCheckout {
            name: name.to_owned(),
            dir,
            state_path,
            op: OperationId::new(checkout.operation_id),
            _lock: lock,
        })
    }

    /// Moves each held workspace onto the operation log as `kept` (old id
    /// to the id it has now) leaves it, appending to `moves`. Loaded at the
    /// head GC classified at; `new_head` is that head's id now. Returns the
    /// first failure; the others are still moved.
    fn move_checkouts(
        &mut self,
        held: &[HeldCheckout],
        kept: &HashMap<OperationId, OperationId>,
        new_head: &OperationId,
        moves: &mut Vec<CheckoutMove>,
    ) -> Option<GcError> {
        let root_op = self.repo.op_store().root_operation_id().clone();
        let mut failed = None;
        let mut reload = false;
        for checkout in held {
            let outcome = match kept.get(&checkout.op) {
                Some(to) if *to == checkout.op => CheckoutOutcome::Unchanged,
                Some(to) => CheckoutOutcome::Rewritten {
                    to: OperationIdHex::of(to),
                },
                None if checkout.op == root_op => CheckoutOutcome::Unchanged,
                None if self.files_match(checkout) => CheckoutOutcome::ToHead {
                    to: OperationIdHex::of(new_head),
                },
                None => CheckoutOutcome::LeftForRecovery,
            };
            let written = match &outcome {
                CheckoutOutcome::Rewritten { to } | CheckoutOutcome::ToHead { to } => {
                    write_checkout(&checkout.state_path, &checkout.name, &to.to_id())
                        .map(|()| reload |= checkout.dir == self.workspace_root())
                }
                CheckoutOutcome::LeftForRecovery => self.remove_operation_file(&checkout.op),
                _ => Ok(()),
            };
            let (from, name) = (OperationIdHex::of(&checkout.op), &checkout.name);
            let path = checkout.dir.display();
            match (&written, &outcome) {
                (Err(error), _) => tracing::error!(
                    %path,
                    op_id = %from,
                    error = %ErrorChain(error),
                    "cannot move jj workspace `{name}` onto the rewritten operation log"
                ),
                (Ok(()), CheckoutOutcome::Rewritten { to } | CheckoutOutcome::ToHead { to }) => {
                    tracing::info!(
                        %path,
                        op_id = %to,
                        "jj workspace `{name}` moved onto the rewritten operation log from {from}"
                    );
                }
                (Ok(()), CheckoutOutcome::LeftForRecovery) => tracing::warn!(
                    %path,
                    op_id = %from,
                    "jj workspace `{name}` left for `jj workspace update-stale`: its files are not \
                     its commit's at head; running it there makes a recovery commit of the files"
                ),
                (Ok(()), _) => tracing::debug!(
                    %path,
                    op_id = %from,
                    "jj workspace `{name}`'s operation is kept"
                ),
            }
            if let Err(source) = written
                && failed.is_none()
            {
                failed = Some(GcError::Checkout {
                    workspace: checkout.name.clone(),
                    path: checkout.dir.clone(),
                    source,
                });
            }
            moves.push(CheckoutMove {
                workspace: checkout.name.clone(),
                path: checkout.dir.clone(),
                from: Some(from),
                outcome,
            });
        }
        if reload && let Err(error) = self.reload_working_copy() {
            failed.get_or_insert(error);
        }
        failed
    }

    /// A `jj` command that committed an operation before GC loaded the head,
    /// and was waiting on GC's lock to check out, writes that operation's
    /// old id once the locks are released: jj does not check what its
    /// checkout names before replacing it. Moves each workspace that names
    /// a rewritten operation again onto the rewrite, before the collection
    /// prunes the old id's ancestors. A lock still busy, or a checkout that
    /// cannot be read, fails GC before the collection: the old operations
    /// stay loadable, and the next GC moves the workspace.
    fn recheck_checkouts(
        &mut self,
        places: &[CheckoutPlace],
        rewritten: &HashMap<OperationId, OperationId>,
        moves: &mut [CheckoutMove],
    ) -> Result<(), GcError> {
        let deadline = Instant::now() + CHECKOUT_LOCK_WAIT;
        let mut reload = false;
        for place in places {
            let failed = |source| GcError::Checkout {
                workspace: place.name.clone(),
                path: place.dir.clone(),
                source,
            };
            let _lock = match lock_checkout(&place.state_path, deadline) {
                Ok(Some(lock)) => lock,
                Ok(None) => {
                    return Err(failed(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "a `jj` command held it after GC moved it; nothing was collected, so \
                         run reef ws gc again when it finishes",
                    )));
                }
                Err(error) => return Err(failed(error.err)),
            };
            let op = OperationId::new(
                read_checkout(&place.state_path)
                    .map_err(failed)?
                    .operation_id,
            );
            let Some(to) = rewritten.get(&op) else {
                continue;
            };
            write_checkout(&place.state_path, &place.name, to).map_err(failed)?;
            reload |= place.dir == self.workspace_root();
            let (from, to) = (OperationIdHex::of(&op), OperationIdHex::of(to));
            tracing::info!(
                path = %place.dir.display(),
                op_id = %to,
                "jj workspace `{}` named the rewritten operation {from} again (a `jj` command \
                 finished during GC); moved onto the rewrite",
                place.name
            );
            if let Some(entry) = moves.iter_mut().find(|entry| entry.workspace == place.name) {
                entry.outcome = CheckoutOutcome::Rewritten { to };
            }
        }
        if reload {
            self.reload_working_copy()?;
        }
        Ok(())
    }

    /// Whether a held workspace's recorded tree is its working-copy commit's
    /// tree in the loaded view. Anything unreadable counts as no, so GC
    /// never hides staleness.
    fn files_match(&self, checkout: &HeldCheckout) -> bool {
        let matched = || -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
            let name = WorkspaceNameBuf::from(checkout.name.as_str());
            let Some(id) = self.repo.view().get_wc_commit_id(&name) else {
                return Ok(false);
            };
            if !checkout.state_path.join("tree_state").is_file() {
                // The load would create one.
                return Ok(false);
            }
            let commit = self.repo.store().get_commit(id)?;
            let settings = TreeStateSettings::try_from_user_settings(self.workspace.settings())?;
            let tree_state = TreeState::load(
                self.repo.store().clone(),
                checkout.dir.clone(),
                checkout.state_path.clone(),
                &settings,
            )?;
            Ok(tree_state.current_tree().tree_ids_and_labels()
                == commit.tree().tree_ids_and_labels())
        };
        matched().unwrap_or_else(|error| {
            tracing::warn!(
                path = %checkout.dir.display(),
                error = %error,
                "cannot compare jj workspace `{}`'s files with its commit; treated as different",
                checkout.name
            );
            false
        })
    }

    /// Deletes an operation no op head reaches, so a working copy that names
    /// it takes jj's recovery for a missing operation. Only for jj's simple
    /// operation store, whose layout this knows; under the caller's lock.
    fn remove_operation_file(&self, op: &OperationId) -> io::Result<()> {
        let store = self.repo.op_store();
        if store.name() != "simple_op_store" {
            tracing::warn!(
                "unknown operation store {}; the operation is left",
                store.name()
            );
            return Ok(());
        }
        let path = self
            .repo_path()
            .join("op_store")
            .join("operations")
            .join(op.hex());
        match std::fs::remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Reloads this workspace's working copy after GC wrote its checkout,
    /// so the cached state names the operation on disk.
    fn reload_working_copy(&mut self) -> Result<(), GcError> {
        let root = self.workspace_root().to_owned();
        let working_copy = LocalWorkingCopy::load(
            self.repo.store().clone(),
            root.clone(),
            root.join(".jj").join("working_copy"),
            self.workspace.settings(),
        )
        .map_err(|error| OpenError::from(jj_lib::workspace::WorkspaceLoadError::from(error)))?;
        self.workspace = Workspace::new(
            &root,
            self.repo_path().to_owned(),
            Box::new(working_copy),
            self.workspace.repo_loader().clone(),
        )
        .map_err(|error| OpenError::from(jj_lib::workspace::WorkspaceLoadError::from(error)))?;
        Ok(())
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
            tracing::error!(error = %ErrorChain(error), "cannot rebind the working copy");
        })
    }

    /// Moves a working copy stranded by an interrupted GC to the loaded
    /// operation: one whose operation is gone, or is not an ancestor of the
    /// loaded head (jj's sibling answer, which a load at head leaves only for
    /// an operation the op heads no longer reach), while its recorded tree is
    /// still its commit's in the loaded view. Anything else is left for the
    /// snapshot's staleness policy. Returns whether it moved the working
    /// copy. Load at head first.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn rebind_if_stranded(&mut self) -> Result<bool, WorkingCopyCheckError> {
        let repo = self.repo.clone();
        let Some(id) = repo
            .view()
            .get_wc_commit_id(self.workspace.workspace_name())
        else {
            return Ok(false);
        };
        let commit = repo.store().get_commit(id).map_err(ReadError::from)?;
        let mut locked = self.workspace.start_working_copy_mutation().await?;
        let stranded =
            match WorkingCopyFreshness::check_stale(locked.locked_wc(), &commit, &repo).await {
                Ok(WorkingCopyFreshness::SiblingOperation)
                | Err(OpStoreError::ObjectNotFound { .. }) => true,
                Ok(_) => false,
                Err(error) => return Err(WorkingCopyCheckError::OpStore(error)),
            };
        let matches = locked.locked_wc().old_tree().tree_ids_and_labels()
            == commit.tree().tree_ids_and_labels();
        if !(stranded && matches) {
            // Dropped without finishing: nothing is written.
            return Ok(false);
        }
        let wc_op = OperationIdHex::of(locked.locked_wc().old_operation_id());
        locked.finish(repo.op_id().clone()).await?;
        tracing::warn!(
            op_id = %wc_op,
            "working copy was stranded on this abandoned operation; moved to the head"
        );
        Ok(true)
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
        let recorded = self.recorded_tree()?;
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

/// One of the user's jj workspaces, locked by [`Repo::gc`].
struct HeldCheckout {
    name: String,
    dir: PathBuf,
    /// `<dir>/.jj/working_copy`.
    state_path: PathBuf,
    /// The operation its checkout named, read under the lock.
    op: OperationId,
    _lock: FileLock,
}

/// Where a workspace [`Repo::gc`] held is, once its lock is released.
struct CheckoutPlace {
    name: String,
    dir: PathBuf,
    state_path: PathBuf,
}

impl From<HeldCheckout> for CheckoutPlace {
    fn from(held: HeldCheckout) -> Self {
        CheckoutPlace {
            name: held.name,
            dir: held.dir,
            state_path: held.state_path,
        }
    }
}

/// Reads jj's `checkout` file in `state_path`.
fn read_checkout(state_path: &Path) -> io::Result<Checkout> {
    let bytes = std::fs::read(state_path.join("checkout"))?;
    Checkout::decode(&*bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Takes jj's working-copy lock in `state_path`, polling until `deadline`;
/// `None` while another process holds it. The lock file is given the
/// checkout's owner, as jj would have created it: a daemon running as root
/// must not leave a file the user's `jj` cannot open, then or after a crash.
fn lock_checkout(state_path: &Path, deadline: Instant) -> Result<Option<FileLock>, FileLockError> {
    use std::os::unix::fs::MetadataExt as _;

    let lock_path = state_path.join("working_copy.lock");
    let lock = loop {
        match FileLock::try_lock(lock_path.clone())? {
            Some(lock) => break lock,
            None if Instant::now() < deadline => std::thread::sleep(CHECKOUT_LOCK_POLL),
            None => return Ok(None),
        }
    };
    if let (Ok(checkout), Ok(file)) = (
        std::fs::metadata(state_path.join("checkout")),
        std::fs::metadata(&lock_path),
    ) && (file.uid(), file.gid()) != (checkout.uid(), checkout.gid())
        && let Err(error) =
            std::os::unix::fs::chown(&lock_path, Some(checkout.uid()), Some(checkout.gid()))
    {
        tracing::warn!(
            path = %lock_path.display(),
            error = %error,
            "cannot give jj's lock file the checkout's owner"
        );
    }
    Ok(Some(lock))
}

/// The head [`Repo::gc`] works at, and the user's jj workspaces it holds.
struct Holding {
    head: Arc<ReadonlyRepo>,
    /// Locked, in name order.
    held: Vec<HeldCheckout>,
    /// Not held; GC goes on without them.
    skipped: Vec<CheckoutMove>,
    /// Set when a lock stayed busy; nothing is held then.
    blocked: Option<AbandonBlocked>,
}

/// Why [`Repo::gc`] does not hold a workspace.
enum Unheld {
    /// It cannot be moved; GC goes on without it.
    Skipped { path: PathBuf, reason: String },
    /// A `jj` command held its lock until the deadline.
    Busy { path: PathBuf },
}

/// The names of the jj workspaces in `repo`'s view that are not Reef's.
fn user_workspace_names(repo: &ReadonlyRepo) -> Vec<String> {
    repo.view()
        .wc_commit_ids()
        .keys()
        .map(|name| name.as_str().to_owned())
        .filter(|name| !name.starts_with(REEF_WORKSPACE_PREFIX))
        .collect()
}

/// Writes jj's `checkout` file in `state_path`, naming `op`, as jj-lib
/// 0.46.0's `CheckoutState::save` does (`local_working_copy.rs:2705`; check
/// again on a jj-lib upgrade). Keeps the old file's owner and mode, so a
/// daemon running as root leaves it readable to the user's `jj`. The caller
/// holds the working-copy lock.
fn write_checkout(state_path: &Path, name: &str, op: &OperationId) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let path = state_path.join("checkout");
    let old = std::fs::metadata(&path)?;
    let proto = Checkout {
        operation_id: op.to_bytes(),
        workspace_name: name.to_owned(),
    };
    let mut temp = tempfile::NamedTempFile::new_in(state_path)?;
    temp.as_file_mut().write_all(&proto.encode_to_vec())?;
    let new = temp.as_file().metadata()?;
    if (new.uid(), new.gid()) != (old.uid(), old.gid()) {
        std::os::unix::fs::fchown(temp.as_file(), Some(old.uid()), Some(old.gid()))?;
    }
    if new.mode() & 0o7777 != old.mode() & 0o7777 {
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(old.mode() & 0o7777))?;
    }
    jj_lib::file_util::persist_temp_file(temp, &path)?;
    Ok(())
}

/// What [`reparent_mapped`] wrote.
struct Reparented {
    /// The rewrite of the head.
    new_head: OperationId,
    /// Each rewritten operation's old id to its new one.
    rewritten: HashMap<OperationId, OperationId>,
    /// Operations no longer reachable from the new head.
    unreachable_count: usize,
}

/// jj-lib 0.46.0's `op_walk::reparent_range` (`op_walk.rs:403`) for one
/// root and one head, returning the old-to-new map it builds and drops.
/// Operation ids are content hashes, so it writes exactly what jj's would.
/// Check again on a jj-lib upgrade (`reparent_mapped_matches_jj`).
async fn reparent_mapped(
    op_store: &dyn OpStore,
    cut: &Operation,
    head: &Operation,
    dest: &Operation,
) -> OpStoreResult<Reparented> {
    let to_reparent: Vec<Operation> =
        walk_ancestors_range(std::slice::from_ref(head), std::slice::from_ref(cut))
            .try_collect()
            .await?;
    let unreachable_count =
        walk_ancestors_range(std::slice::from_ref(cut), std::slice::from_ref(dest))
            .try_fold(0, |count, _| async move { Ok(count + 1) })
            .await?;
    let mut rewritten = HashMap::new();
    for old in to_reparent.into_iter().rev() {
        let mut data = old.store_operation().clone();
        let mut dest_once = Some(dest.id());
        data.parents = data
            .parents
            .iter()
            .filter_map(|id| rewritten.get(id).or_else(|| dest_once.take()))
            .cloned()
            .collect();
        let new_id = op_store.write_operation(&data).await?;
        rewritten.insert(old.id().clone(), new_id);
    }
    let new_head = rewritten.get(head.id()).unwrap_or(dest.id()).clone();
    Ok(Reparented {
        new_head,
        rewritten,
        unreachable_count,
    })
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

#[cfg(test)]
mod tests {
    use jj_lib::op_walk::reparent_range;

    use super::*;
    use crate::jj::settings::{Identity, user_settings};

    /// Reef's copy writes the operations jj's `reparent_range` writes, and
    /// maps each to a rewrite with the same view.
    #[tokio::test]
    async fn reparent_mapped_matches_jj() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = user_settings(&Identity {
            name: "Reef Test".into(),
            email: "reef-test@example.com".into(),
        })
        .expect("settings");
        let mut repo = Repo::init_internal_git(dir.path(), &settings)
            .await
            .expect("init");
        for n in 0..4 {
            std::fs::write(dir.path().join("a.txt"), format!("{n}\n")).expect("write");
            repo.snapshot("snapshot").await.expect("snapshot");
        }
        let loader = repo.head().loader().clone();
        let head = loader
            .load_at_head()
            .await
            .expect("load")
            .operation()
            .clone();
        let ops: Vec<Operation> = walk_ancestors(std::slice::from_ref(&head))
            .try_collect()
            .await
            .expect("walk");
        assert!(ops.len() >= 6, "{} operations", ops.len());
        let (cut, root) = (&ops[2], loader.root_operation().await);
        let store = loader.op_store().as_ref();

        let ours = reparent_mapped(store, cut, &head, &root)
            .await
            .expect("ours");
        let jjs = reparent_range(
            store,
            std::slice::from_ref(cut),
            std::slice::from_ref(&head),
            &root,
        )
        .await
        .expect("jj's");
        assert_eq!(jjs.new_head_ids, std::slice::from_ref(&ours.new_head));
        assert_eq!(jjs.rewritten_count, ours.rewritten.len());
        assert_eq!(jjs.unreachable_count, ours.unreachable_count);
        assert_eq!(ours.rewritten.len(), 2);
        for (old, new) in &ours.rewritten {
            let old = store.read_operation(old).await.expect("old");
            let new = store.read_operation(new).await.expect("new");
            assert_eq!(old.view_id, new.view_id);
        }
    }
}
