//! Undoing operations: [`Repo::revert_operation`] reverts exactly one
//! operation onto the head, as `jj op revert` does, refusing what it
//! cannot revert exactly (R2.03, ADR 0031); [`Repo::undo_last`] is Phase
//! 1's restore of the head's parent, kept for the spike's tests.

use std::collections::BTreeSet;
use std::sync::Arc;

use jj_lib::backend::{BackendError, CommitId};
use jj_lib::index::ChangeIdIndex;
use jj_lib::object_id::{HexPrefix, ObjectId as _, PrefixResolution};
use jj_lib::op_store::{OpStoreError, RefTarget, View};
use jj_lib::op_walk::closest_common_ancestors;
use jj_lib::operation::Operation;
use jj_lib::repo::{ReadonlyRepo, Repo as _, RepoLoaderError};
use jj_lib::transaction::TransactionCommitError;
use jj_lib::working_copy::CheckoutError;

use super::effects::{bookmarks_of, visible_difference};
use super::oplog::{OpKind, OpLogError, OpMeta, classify, entry};
use super::repo::{OperationIdHex, ReadError, Repo};
use super::snapshot::SnapshotError;
use crate::error::{Classify, ErrorKind, classify_by_source};

/// Description prefix jj uses for undo operations; `jj undo` reads it to
/// walk an undo stack.
pub const UNDO_OP_DESC_PREFIX: &str = "undo: restore to operation ";

/// Result of [`Repo::undo_last`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoOutcome {
    /// The snapshot operation taken first because the working copy had
    /// changes; `undone` is then this operation. `None` if nothing changed.
    pub snapshotted: Option<OperationIdHex>,
    /// The operation that was undone.
    pub undone: OperationIdHex,
    /// The operation whose view was restored.
    pub restored_to: OperationIdHex,
    /// The new head operation recording the undo.
    pub new_op: OperationIdHex,
}

/// Failure to undo an operation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UndoError {
    /// The head operation is the root operation.
    #[error("cannot undo the root operation")]
    RootOperation,
    /// The head operation merges divergent heads; jj refuses these too.
    #[error("cannot undo a merge operation")]
    MergeOperation,
    /// The operation being restored predates this workspace, so undoing would
    /// remove the workspace's working-copy commit. Nothing was written.
    #[error("cannot undo past the creation of workspace {0}")]
    WorkspaceNotInRestoredView(String),
    /// Snapshotting the working copy before the undo failed.
    #[error("failed to snapshot the working copy before undo")]
    Snapshot(#[from] SnapshotError),
    /// An operation or view could not be read.
    #[error("failed to read the operation log")]
    OpStore(#[from] OpStoreError),
    /// The undo operation could not be committed.
    #[error("failed to commit the undo operation")]
    Commit(#[from] TransactionCommitError),
    /// The restored working-copy commit could not be read.
    #[error(transparent)]
    Read(#[from] ReadError),
    /// Reading a commit failed.
    #[error("failed to read a commit")]
    Backend(#[from] BackendError),
    /// Updating the files on disk failed.
    #[error("failed to check out the restored working-copy commit")]
    Checkout(#[from] CheckoutError),
    /// An operation [`Repo::revert_operation`] never reverts.
    #[error("operation {op} cannot be undone: {why}")]
    NotUndoable {
        /// The operation.
        op: OperationIdHex,
        /// Why, as a phrase.
        why: &'static str,
    },
    /// The operation is not an ancestor of the head.
    #[error("operation {0} is not in the history of the head operation")]
    NotInHistory(OperationIdHex),
    /// Something the operation changed has changed again since.
    #[error("operation {op} cannot be undone exactly: {what} has changed since")]
    Superseded {
        /// The operation.
        op: OperationIdHex,
        /// The first thing that moved, as a phrase.
        what: String,
    },
    /// Loading the repository at an operation, or merging, failed.
    #[error("failed to load the repository at an operation")]
    Load(#[from] RepoLoaderError),
    /// Reading the operation log or the index failed.
    #[error(transparent)]
    OpLog(#[from] OpLogError),
}

impl Classify for UndoError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::RootOperation
            | Self::MergeOperation
            | Self::WorkspaceNotInRestoredView(_)
            | Self::NotInHistory(_)
            | Self::Superseded { .. }
            | Self::Checkout(CheckoutError::ConcurrentCheckout) => ErrorKind::Conflict,
            Self::NotUndoable { .. } => ErrorKind::InvalidArgument,
            Self::Load(error) => classify_by_source(error),
            Self::OpLog(error) => error.kind(),
            Self::Snapshot(error) => error.kind(),
            Self::Read(error) => error.kind(),
            Self::OpStore(error) => classify_by_source(error),
            Self::Commit(error) => classify_by_source(error),
            Self::Backend(error) => classify_by_source(error),
            Self::Checkout(error) => classify_by_source(error),
        }
    }
}

/// What [`Repo::revert_operation`] needs to know before it writes: the
/// jj workspaces the revert would move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertPlan {
    /// The operation to revert.
    pub op_id: OperationIdHex,
    /// The jj workspaces whose working copy the operation moved, or whose
    /// working-copy commit descends from a commit it made visible: those
    /// the revert moves, sorted.
    pub workspaces: Vec<String>,
}

/// What an operation changed, as a revert checks it.
struct Changed {
    op: Operation,
    parent: Operation,
    /// Commits visible after it and not before.
    added: Vec<CommitId>,
    /// Commits visible before it and not after.
    removed: Vec<CommitId>,
    /// `(remote, name, target after)` of the bookmarks it moved; remote
    /// `None` for a local bookmark.
    bookmarks: Vec<(Option<String>, String, RefTarget)>,
    /// `(name, target after)` of the local tags it moved.
    tags: Vec<(String, RefTarget)>,
    /// `(workspace, working-copy commit after)` of the working copies it
    /// moved.
    working_copies: Vec<(String, CommitId)>,
}

/// Refuses an operation [`Repo::revert_operation`] never reverts: the root,
/// a merge of concurrent operations, one that adds or forgets a jj
/// workspace, a Reef workspace move, and one that changed git refs or
/// git's `HEAD`.
pub async fn undoable(op: &Operation) -> Result<(), UndoError> {
    let refuse = |why| {
        Err(UndoError::NotUndoable {
            op: OperationIdHex::of(op.id()),
            why,
        })
    };
    let parents = op.parents().await?;
    let parent = match parents.as_slice() {
        [] => return refuse("it is the root operation"),
        [parent] => parent,
        _ => return refuse("it merges concurrent operations"),
    };
    let (after, before) = (op.view().await?, parent.view().await?);
    let (after, before) = (after.store_view(), before.store_view());
    let names = |view: &View| view.wc_commit_ids.keys().cloned().collect::<BTreeSet<_>>();
    if names(after) != names(before) {
        return refuse("it adds or forgets a workspace; release or create one instead");
    }
    if classify(&entry(op)).kind == Some(OpKind::MoveWorkspace) && !op.metadata().is_snapshot {
        return refuse("it moves a workspace; move the workspace back instead");
    }
    let git = |view: &View| {
        (
            view.git_refs.clone(),
            view.git_heads.clone(),
            view.remote_views
                .iter()
                .find(|(remote, _)| remote.as_str() == GIT_REMOTE)
                .map(|(_, remote)| remote.clone()),
        )
    };
    if git(after) != git(before) {
        return refuse("it changed git refs or git's HEAD");
    }
    Ok(())
}

/// The remote whose bookmarks are git's own refs.
const GIT_REMOTE: &str = "git";

/// Loads `op` by id and refuses it unless it is [`undoable`] and an
/// ancestor of `head`'s operation.
async fn load_revertible(
    head: &ReadonlyRepo,
    op_id: &OperationIdHex,
) -> Result<Operation, UndoError> {
    let op = head.loader().load_operation(&op_id.to_id()).await?;
    undoable(&op).await?;
    let common = closest_common_ancestors([op.clone()], [head.operation().clone()]).await?;
    if !matches!(common.as_slice(), [only] if only.id() == op.id()) {
        return Err(UndoError::NotInHistory(op_id.clone()));
    }
    Ok(op)
}

/// What `op` changed, over `head`'s index (which holds every commit of an
/// ancestor of the head).
async fn changed(head: &ReadonlyRepo, op: Operation) -> Result<Changed, UndoError> {
    let parent = op.parents().await?.remove(0);
    let (after_view, before_view) = (op.view().await?, parent.view().await?);
    let (after, before) = (after_view.store_view(), before_view.store_view());
    let heads = |view: &View| view.head_ids.iter().cloned().collect::<Vec<_>>();
    let (after_heads, before_heads) = (heads(after), heads(before));
    let added = visible_difference(head, &after_heads, &before_heads)
        .await?
        .into_iter()
        .map(|(commit, _)| commit)
        .collect();
    let removed = visible_difference(head, &before_heads, &after_heads)
        .await?
        .into_iter()
        .map(|(commit, _)| commit)
        .collect();
    let absent = RefTarget::absent();
    let (after_refs, before_refs) = (bookmarks_of(after), bookmarks_of(before));
    let mut bookmarks = Vec::new();
    for key in after_refs
        .keys()
        .chain(before_refs.keys())
        .collect::<BTreeSet<_>>()
    {
        let now = after_refs.get(key).copied().unwrap_or(&absent);
        if before_refs.get(key).copied().unwrap_or(&absent) != now {
            let (remote, name) = key;
            bookmarks.push((remote.map(str::to_owned), (*name).to_owned(), now.clone()));
        }
    }
    let mut tags = Vec::new();
    for name in after
        .local_tags
        .keys()
        .chain(before.local_tags.keys())
        .collect::<BTreeSet<_>>()
    {
        let now = after.local_tags.get(name).unwrap_or(&absent);
        if before.local_tags.get(name).unwrap_or(&absent) != now {
            tags.push((name.as_str().to_owned(), now.clone()));
        }
    }
    let working_copies = after
        .wc_commit_ids
        .iter()
        .filter(|(name, id)| before.wc_commit_ids.get(*name) != Some(*id))
        .map(|(name, id)| (name.as_str().to_owned(), id.clone()))
        .collect();
    Ok(Changed {
        op,
        parent,
        added,
        removed,
        bookmarks,
        tags,
        working_copies,
    })
}

/// Whether `commit` is visible in `index`, the change id index of a view.
async fn visible_in(
    head: &ReadonlyRepo,
    index: &dyn ChangeIdIndex,
    commit: &CommitId,
) -> Result<bool, UndoError> {
    let change = head
        .store()
        .get_commit_async(commit)
        .await?
        .change_id()
        .clone();
    let resolved = index
        .resolve_prefix(&HexPrefix::from_id(&change))
        .await
        .map_err(|error| UndoError::Backend(BackendError::Other(error.into())))?;
    Ok(match resolved {
        PrefixResolution::SingleMatch(targets) => targets.has_visible(commit),
        PrefixResolution::NoMatch | PrefixResolution::AmbiguousMatch => false,
    })
}

/// The first thing `changed` touched that is no longer as it left it at
/// `head`, as a phrase; `None` when the operation can be reverted exactly.
async fn moved_since(head: &ReadonlyRepo, changed: &Changed) -> Result<Option<String>, UndoError> {
    let view = head.view();
    let heads: Vec<CommitId> = view.heads().iter().cloned().collect();
    let index = head
        .readonly_index()
        .change_id_index(&mut heads.iter())
        .map_err(|error| UndoError::Backend(BackendError::Other(error.into())))?;
    for commit in &changed.added {
        if !visible_in(head, &*index, commit).await? {
            return Ok(Some(format!("commit {} it made visible", commit.hex())));
        }
    }
    for commit in &changed.removed {
        if visible_in(head, &*index, commit).await? {
            return Ok(Some(format!("commit {} it hid", commit.hex())));
        }
    }
    let now = bookmarks_of(view.store_view());
    let absent = RefTarget::absent();
    for (remote, name, target) in &changed.bookmarks {
        if now
            .get(&(remote.as_deref(), name.as_str()))
            .copied()
            .unwrap_or(&absent)
            != target
        {
            let label = remote
                .as_ref()
                .map_or(name.clone(), |remote| format!("{name}@{remote}"));
            return Ok(Some(format!("bookmark {label}")));
        }
    }
    for (name, target) in &changed.tags {
        if view
            .store_view()
            .local_tags
            .iter()
            .find(|(n, _)| n.as_str() == name)
            .map_or(&absent, |(_, t)| t)
            != target
        {
            return Ok(Some(format!("tag {name}")));
        }
    }
    for (name, commit) in &changed.working_copies {
        let current = view
            .wc_commit_ids()
            .iter()
            .find(|(n, _)| n.as_str() == name)
            .map(|(_, id)| id);
        if current != Some(commit) {
            return Ok(Some(format!("the working copy of {name}")));
        }
    }
    Ok(None)
}

/// Plans reverting `op_id` at `head`: refuses an operation that is not
/// [`undoable`] or not in the head's history, and lists the workspaces the
/// revert would move. Whether it is superseded is checked when it is
/// written, after the caller has snapshotted those workspaces.
pub async fn plan_revert(
    head: &Arc<ReadonlyRepo>,
    op_id: &OperationIdHex,
) -> Result<RevertPlan, UndoError> {
    let op = load_revertible(head, op_id).await?;
    let changed = changed(head, op).await?;
    let mut workspaces: BTreeSet<String> = changed
        .working_copies
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    for (name, wc) in head.view().wc_commit_ids() {
        for commit in &changed.added {
            if head
                .index()
                .is_ancestor(commit, wc)
                .await
                .map_err(|error| UndoError::Backend(BackendError::Other(error.into())))?
            {
                workspaces.insert(name.as_str().to_owned());
                break;
            }
        }
    }
    Ok(RevertPlan {
        op_id: op_id.clone(),
        workspaces: workspaces.into_iter().collect(),
    })
}

impl Repo {
    /// Reverts exactly the operation `op_id` onto the head, as `jj op
    /// revert` does, in one operation recorded with `meta`, which this
    /// [`Repo`] adopts: a three-way merge of the views from the operation
    /// back to its parent, descendants rebased. What it created is
    /// abandoned or rewritten back, what it hid comes back, and the
    /// bookmarks and working copies it moved go back; later operations
    /// stay. Returns the new operation, `None` when there was nothing to
    /// revert.
    ///
    /// Refuses an operation that is not [`undoable`], not in the head's
    /// history ([`UndoError::NotInHistory`]), or superseded: something it
    /// changed has changed again since ([`UndoError::Superseded`]), where
    /// `jj op revert` would merge, usually leaving changes divergent.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), op_id = %op_id))]
    pub async fn revert_operation(
        &mut self,
        op_id: &OperationIdHex,
        meta: &OpMeta,
    ) -> Result<Option<OperationIdHex>, UndoError> {
        let op = load_revertible(&self.repo, op_id).await?;
        let changed = changed(&self.repo, op).await?;
        if let Some(what) = moved_since(&self.repo, &changed).await? {
            return Err(UndoError::Superseded {
                op: op_id.clone(),
                what,
            });
        }
        let loader = self.repo.loader();
        let at_op = loader.load_at(&changed.op).await?;
        let at_parent = loader.load_at(&changed.parent).await?;
        let mut tx = self.repo.start_transaction();
        tx.repo_mut().merge(&at_op, &at_parent).await?;
        tx.repo_mut().rebase_descendants().await?;
        if !tx.repo().has_changes() {
            return Ok(None);
        }
        self.repo = meta.commit(tx).await?;
        Ok(Some(OperationIdHex::of(self.repo.op_id())))
    }

    /// Snapshots the working copy, then restores the view of the head
    /// operation's parent and checks the restored working-copy commit out on
    /// disk.
    ///
    /// As with `jj undo`, the snapshot comes first, so edits made since the
    /// last snapshot are recorded before the checkout replaces them; if there
    /// were any, the undo undoes that snapshot and the edits stay reachable in
    /// the operation log. This is "restore to parent operation" only: unlike
    /// `jj undo` it does not skip over earlier undos, refuse operations from
    /// other workspaces, or reset git `HEAD` in colocated repos; the daemon
    /// reverts one operation with [`Repo::revert_operation`] instead.
    ///
    /// The undo operation is recorded before the checkout. If the checkout
    /// fails, the operation stands and the working copy is stale until it is
    /// updated.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn undo_last(&mut self) -> Result<UndoOutcome, UndoError> {
        let outcome = self
            .undo_last_unlogged()
            .await
            .inspect_err(|error| crate::log_failure!(error))?;
        tracing::info!(op_id = %outcome.new_op, "operation undone");
        Ok(outcome)
    }

    async fn undo_last_unlogged(&mut self) -> Result<UndoOutcome, UndoError> {
        let snapshot = self.snapshot_unlogged("snapshot working copy").await?;
        let snapshotted = snapshot.changed.then_some(snapshot.op_id);

        let target = self.repo.operation().clone();
        let mut parents = target.parents().await?;
        let parent = match parents.len() {
            0 => return Err(UndoError::RootOperation),
            1 => parents.remove(0),
            _ => return Err(UndoError::MergeOperation),
        };
        let old_wc_commit = self.wc_commit()?;

        let restored = parent.view().await?;
        let workspace_name = self.workspace.workspace_name();
        if restored.get_wc_commit_id(workspace_name).is_none() {
            return Err(UndoError::WorkspaceNotInRestoredView(
                workspace_name.as_str().to_owned(),
            ));
        }
        let mut tx = self.repo.start_transaction();
        tx.set_workspace_name(self.workspace.workspace_name());
        let new_view = restore_view(restored.store_view(), tx.base_repo().view().store_view());
        tx.repo_mut().set_view(new_view);
        let meta = OpMeta::new(
            OpKind::Undo,
            format!("{UNDO_OP_DESC_PREFIX}{}", parent.id().hex()),
        );
        let new_repo = meta.commit(tx).await?;
        self.repo = new_repo;

        // Always check out, as jj-cli does: even when the working-copy commit is
        // unchanged this records the new operation in the working-copy state,
        // so the next snapshot does not see a stale working copy.
        let new_wc_commit = self.wc_commit()?;
        self.workspace
            .check_out(
                self.repo.op_id().clone(),
                Some(&old_wc_commit.tree()),
                &new_wc_commit,
            )
            .await?;

        Ok(UndoOutcome {
            snapshotted,
            undone: OperationIdHex::of(target.id()),
            restored_to: OperationIdHex::of(parent.id()),
            new_op: OperationIdHex::of(self.repo.op_id()),
        })
    }
}

/// jj-cli's `view_with_desired_portions_restored` with its default of
/// restoring both the repo and remote-tracking portions: heads, bookmarks,
/// tags, remote views, and working-copy commits come from `restored`; git refs
/// and git `HEAD` stay as they are in `current`.
fn restore_view(restored: &View, current: &View) -> View {
    View {
        head_ids: restored.head_ids.clone(),
        local_bookmarks: restored.local_bookmarks.clone(),
        local_tags: restored.local_tags.clone(),
        remote_views: restored.remote_views.clone(),
        git_refs: current.git_refs.clone(),
        git_heads: current.git_heads.clone(),
        wc_commit_ids: restored.wc_commit_ids.clone(),
    }
}
