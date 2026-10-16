//! Undoing the most recent operation.

use jj_lib::backend::BackendError;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{OpStoreError, View};
use jj_lib::transaction::TransactionCommitError;
use jj_lib::working_copy::CheckoutError;

use super::repo::{OperationIdHex, ReadError, Repo};
use super::snapshot::SnapshotError;

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
}

impl Repo {
    /// Snapshots the working copy, then restores the view of the head
    /// operation's parent and checks the restored working-copy commit out on
    /// disk.
    ///
    /// As with `jj undo`, the snapshot comes first, so edits made since the
    /// last snapshot are recorded before the checkout replaces them; if there
    /// were any, the undo undoes that snapshot and the edits stay reachable in
    /// the operation log. This is "restore to parent operation" only: unlike
    /// `jj undo` it does not skip over earlier undos, refuse operations from
    /// other workspaces, or reset git `HEAD` in colocated repos (R2.03).
    ///
    /// The undo operation is recorded before the checkout. If the checkout
    /// fails, the operation stands and the working copy is stale until it is
    /// updated.
    pub async fn undo_last(&mut self) -> Result<UndoOutcome, UndoError> {
        let snapshot = self.snapshot("snapshot working copy").await?;
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
        let new_repo = tx
            .commit(format!("{UNDO_OP_DESC_PREFIX}{}", parent.id().hex()))
            .await?;
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
