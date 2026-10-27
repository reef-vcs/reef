//! Snapshotting the working copy into the working-copy commit.
//!
//! A snapshot has three steps, so a daemon can run each on the thread that
//! owns what it touches (ADR 0012, R1.13):
//!
//! 1. [`Repo::begin_snapshot`] locks the working copy, checks it against the
//!    loaded repository (the staleness policy, ADR 0018), and scans the
//!    files. It runs on the workspace's thread and returns a
//!    [`PendingSnapshot`] holding the lock.
//! 2. [`commit_snapshot`] records the scanned tree in a transaction. It runs
//!    wherever the repository's head lives and needs only the plain-data
//!    [`SnapshotPlan`].
//! 3. [`PendingSnapshot::finish`] saves the working-copy state at the new
//!    operation, back on the workspace's thread.
//!
//! [`Repo::snapshot`] runs the three in one place.
//!
//! # Carried snapshots
//!
//! A write by Reef that moves a workspace to another commit of its change
//! hands the workspace's next snapshot a [`Carry`] (issue #80, ADR 0039).
//! When that snapshot finds the working copy stale with edits of its own,
//! and the carry names exactly the move, it records the three-way merge of
//! the write's tree and the files on disk, over the tree the working copy
//! last recorded, as a rewrite of the write's commit, instead of diverging.
//! The merged tree also holds the write's changes, which are not on disk:
//! [`PendingSnapshot::check_out_carried`] writes them after the commit and
//! before [`PendingSnapshot::finish`].
//!
//! # Narrowed scans
//!
//! A caller that knows which paths changed passes them to
//! [`Repo::begin_snapshot`] (R1.30, ADR 0026). The working copy is still
//! locked, and its freshness checked, as for any snapshot; a fresh working
//! copy with a working-copy commit in the view is then scanned through a
//! second, narrowed load of its tree state: jj's test monitor
//! (`FsmonitorSettings::Test`) given each path and, for each, the
//! `.gitignore` under it, which jj turns into a rescan of everything under
//! the path. A path covers itself and, if it is or was a directory,
//! everything under it, on disk and in the recorded tree, so deletions under
//! it are found too. [`PendingSnapshot::finish`] saves that tree state before
//! jj saves the operation. The locked working copy itself is not scanned, so
//! jj's `finish` saves only the operation, and the in-memory working copy
//! keeps the tree it was locked with: `Repo::recorded_tree` reads the
//! recorded one from disk.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use futures::StreamExt as _;
use jj_lib::backend::{BackendError, CommitId, TreeId};
use jj_lib::commit::Commit;
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::fsmonitor::FsmonitorSettings;
use jj_lib::gitignore::GitIgnoreError;
use jj_lib::local_working_copy::{LocalWorkingCopy, TreeState, TreeStateError, TreeStateSettings};
use jj_lib::matchers::{EverythingMatcher, NothingMatcher};
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{OpStoreError, OperationId};
use jj_lib::ref_name::{WorkspaceName, WorkspaceNameBuf};
use jj_lib::repo::{ReadonlyRepo, Repo as _, RewriteRootCommit};
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::transaction::TransactionCommitError;
use jj_lib::working_copy::{
    CheckoutError, SnapshotOptions, SnapshotStats, UntrackedReason, WorkingCopyFreshness,
    WorkingCopyStateError,
};
use jj_lib::workspace::LockedWorkspace;

use super::ignore::IgnoreSources;
use super::oplog::{OpKind, OpMeta};
use super::repo::{ChangeIdHex, CommitIdHex, OperationIdHex, ReadError, Repo};
use super::settings::{SettingsError, max_new_file_size};
use crate::error::{Classify, ErrorKind, classify_by_source};
use crate::telemetry::elapsed_ms;

/// Why a new file was left untracked by a snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UntrackedReasonKind {
    /// The file is larger than `snapshot.max-new-file-size`.
    FileTooLarge {
        /// Size of the file in bytes.
        size: u64,
        /// The configured limit in bytes.
        max_size: u64,
    },
    /// The file does not match the auto-track matcher. Reef tracks
    /// everything, so this does not occur yet.
    NotAutoTracked,
}

/// How long each phase of a snapshot took.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SnapshotTiming {
    /// Locking the working copy, loading its saved state, reading the
    /// working-copy commit, walking the files, and hashing changed ones.
    pub scan: Duration,
    /// Writing the new working-copy commit and rebasing its descendants.
    pub write: Duration,
    /// Committing the operation and saving the working-copy state.
    pub commit: Duration,
}

/// Result of [`Repo::snapshot`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotOutcome {
    /// False if the tree was unchanged, in which case no operation was written.
    pub changed: bool,
    /// The new head operation if `changed`, else the current one.
    pub op_id: OperationIdHex,
    /// The working-copy commit after the snapshot.
    pub wc_commit: CommitIdHex,
    /// New files left untracked, by repository-relative path.
    pub untracked: Vec<(String, UntrackedReasonKind)>,
    /// Phase durations.
    pub stats: SnapshotTiming,
}

/// Why a snapshot could not be planned against the loaded repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Behind {
    /// The working copy was updated at an operation newer than the loaded
    /// one.
    WorkingCopyNewer,
    /// The working copy's operation and the loaded one are siblings: two
    /// operation heads exist until a load at head merges them.
    SiblingOperation,
}

/// Failure to snapshot the working copy.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SnapshotError {
    /// The repository must be reloaded at head and the snapshot retried.
    /// Nothing was written.
    #[error("working copy is at operation {wc_op} but the repo is loaded at {repo_op}")]
    RepoBehind {
        /// How the two operations relate.
        kind: Behind,
        /// The operation recorded in the working-copy state.
        wc_op: OperationIdHex,
        /// The operation the [`Repo`] is loaded at.
        repo_op: OperationIdHex,
    },
    /// The commit the workspace last snapshotted is not in the store, so
    /// there is nothing to rewrite or recover from.
    #[error("commit {0} is not in the store")]
    ExpectedCommitMissing(CommitIdHex),
    /// An operation id handed back to [`PendingSnapshot::finish`] is not one.
    #[error("{0} is not an operation id")]
    InvalidOperationId(OperationIdHex),
    /// Settings needed for the snapshot were invalid.
    #[error("invalid snapshot settings")]
    Settings(#[from] SettingsError),
    /// An ignore file could not be read.
    #[error("failed to read ignore files")]
    Ignores(#[from] GitIgnoreError),
    /// The working-copy state could not be locked, read, or saved.
    #[error("failed to lock or save the working-copy state")]
    WorkingCopyState(#[from] WorkingCopyStateError),
    /// A narrowed scan's tree state could not be read or saved.
    #[error("failed to read or save the working-copy state")]
    TreeState(#[from] TreeStateError),
    /// The operation log could not be read to check the working copy's
    /// freshness.
    #[error("failed to read the operation log")]
    OpStore(#[from] OpStoreError),
    /// Walking the working copy failed.
    #[error("failed to snapshot the working copy")]
    Snapshot(#[from] jj_lib::working_copy::SnapshotError),
    /// Updating a stale, unedited working copy to its rewritten commit
    /// failed.
    #[error("failed to update the working copy to its rewritten commit")]
    Checkout(#[from] CheckoutError),
    /// The working-copy commit could not be read.
    #[error(transparent)]
    Read(#[from] ReadError),
    /// Writing the new commit or rebasing descendants failed.
    #[error("failed to write the snapshot commit")]
    Backend(#[from] BackendError),
    /// The working-copy commit is the root commit, which cannot be rewritten.
    #[error("the working-copy commit is the root commit")]
    RootCommit(#[from] RewriteRootCommit),
    /// The operation could not be committed.
    #[error("failed to commit the snapshot operation")]
    Commit(#[from] TransactionCommitError),
}

impl Classify for SnapshotError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::RepoBehind { .. }
            | Self::RootCommit(_)
            | Self::Checkout(CheckoutError::ConcurrentCheckout) => ErrorKind::Conflict,
            Self::ExpectedCommitMissing(_) => ErrorKind::NotFound,
            Self::InvalidOperationId(_)
            | Self::Snapshot(jj_lib::working_copy::SnapshotError::InvalidUtf8SymlinkTarget {
                ..
            }) => ErrorKind::InvalidArgument,
            Self::Settings(error) => error.kind(),
            Self::Read(error) => error.kind(),
            Self::Ignores(error) => classify_by_source(error),
            Self::WorkingCopyState(error) => classify_by_source(error),
            Self::TreeState(error) => classify_by_source(error),
            Self::OpStore(error) => classify_by_source(error),
            Self::Snapshot(error) => classify_by_source(error),
            Self::Checkout(error) => classify_by_source(error),
            Self::Backend(error) => classify_by_source(error),
            Self::Commit(error) => classify_by_source(error),
        }
    }
}

/// Reef's own write moved this workspace's working copy from `from` to `to`,
/// two commits of one change with different trees (ADR 0039). Given to
/// [`Repo::begin_snapshot`], it carries the workspace's edits since its last
/// snapshot onto `to` rather than diverging.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Carry {
    /// The commit the working copy was checked out from: the caller's
    /// `expected_commit`.
    pub from: CommitIdHex,
    /// The commit the write put the working copy on: the view's.
    pub to: CommitIdHex,
    /// The tree the edits are measured from; `None` for the tree the
    /// working copy last recorded. Set for a carry that is merged again
    /// after its check-out failed: the disk tree that merge took
    /// ([`SnapshotPlan::carried`]), so it is not taken twice.
    pub base: Option<ScannedTree>,
}

/// A tree by its ids and conflict labels: plain data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedTree {
    /// The tree.
    pub tree_ids: Merge<TreeId>,
    /// Its conflict labels.
    pub labels: ConflictLabels,
}

/// What a scan found and how to record it: plain data, so it crosses from
/// the workspace's thread to the repository's.
#[derive(Clone, Debug)]
pub struct SnapshotPlan {
    /// The jj workspace, `reef-<id>` for a Reef workspace.
    pub workspace_name: String,
    /// The commit to rewrite: the view's working-copy commit, or, when
    /// diverging, the commit the workspace last snapshotted. When
    /// recovering, the commit the workspace last snapshotted, which is not
    /// rewritten.
    pub rewrite_of: CommitIdHex,
    /// The scanned tree.
    pub tree_ids: Merge<TreeId>,
    /// The scanned tree's conflict labels.
    pub labels: ConflictLabels,
    /// `rewrite_of` is no longer the view's working-copy commit: another
    /// workspace rewrote it while this one had edits of its own.
    pub divergent: bool,
    /// `Some(parents)`: the view has no working-copy commit for the
    /// workspace, so a new commit on `parents`, with a new change id, is
    /// created instead of a rewrite.
    pub recover: Option<Vec<CommitIdHex>>,
    /// `Some(disk)`: the scan was carried onto `rewrite_of`, a write's
    /// commit ([`Carry`]): `tree_ids` is the merge, and `disk` is the tree
    /// on disk it merged, to merge over again if the check-out fails.
    pub carried: Option<ScannedTree>,
}

/// The locked working copy after a scan, from [`Repo::begin_snapshot`].
/// Holds the working-copy lock until [`PendingSnapshot::finish`] (or drop,
/// which saves nothing).
pub struct PendingSnapshot<'a> {
    locked: LockedWorkspace<'a>,
    plan: SnapshotPlan,
    changed: bool,
    adopted: bool,
    head_op: OperationIdHex,
    wc_commit: Option<CommitIdHex>,
    wc_change: Option<ChangeIdHex>,
    untracked: Vec<(String, UntrackedReasonKind)>,
    scan: Duration,
    load: Duration,
    walk: Duration,
    updated_files: u32,
    narrowed: Option<Narrowed>,
    /// [`PendingSnapshot::check_out_carried`] ran.
    checked_out: bool,
}

/// A narrowed scan's own load of the tree state, saved by
/// [`PendingSnapshot::finish`] (see the module docs).
struct Narrowed {
    tree_state: TreeState,
    /// The scan changed it: files, the tree, or refreshed file metadata.
    dirty: bool,
    /// Loading it.
    load: Duration,
}

impl std::fmt::Debug for PendingSnapshot<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingSnapshot")
            .field("plan", &self.plan)
            .field("changed", &self.changed)
            .field("adopted", &self.adopted)
            .field("head_op", &self.head_op)
            .field("narrowed", &self.narrowed.is_some())
            .finish_non_exhaustive()
    }
}

impl PendingSnapshot<'_> {
    /// Whether an operation must record the scan: the scanned tree differs
    /// from `rewrite_of`'s, the change diverges, or the workspace is
    /// recovered.
    pub fn changed(&self) -> bool {
        self.changed
    }

    /// Whether the working copy was stale with no edits of its own and was
    /// updated on disk to the view's working-copy commit. No operation is
    /// needed; [`PendingSnapshot::finish`] at [`PendingSnapshot::head_op`].
    pub fn adopted(&self) -> bool {
        self.adopted
    }

    /// What to record when [`PendingSnapshot::changed`].
    pub fn plan(&self) -> &SnapshotPlan {
        &self.plan
    }

    /// Whether the scan was carried onto a write's commit
    /// ([`SnapshotPlan::carried`]): after [`commit_snapshot`], call
    /// [`PendingSnapshot::check_out_carried`] before
    /// [`PendingSnapshot::finish`].
    pub fn carried(&self) -> bool {
        self.changed && self.plan.carried.is_some()
    }

    /// Checks `commit`, the carried snapshot's new commit in `repo`, out on
    /// disk: the paths where the merge differs from the files scanned, the
    /// write's own and any conflicted. Returns the files it updated, added
    /// or removed.
    pub async fn check_out_carried(
        &mut self,
        repo: &ReadonlyRepo,
        commit: &CommitIdHex,
    ) -> Result<u32, SnapshotError> {
        let commit = commit_by_hex(repo, commit).await?;
        self.updated_files = check_out(&mut self.locked, &commit).await?;
        self.checked_out = true;
        Ok(self.updated_files)
    }

    /// New files the scan left untracked, by repository-relative path.
    pub fn untracked(&self) -> &[(String, UntrackedReasonKind)] {
        &self.untracked
    }

    /// How long locking, the freshness check, and the scan took.
    pub fn scan_time(&self) -> Duration {
        self.scan
    }

    /// Whether the scan covered only the paths given to
    /// [`Repo::begin_snapshot`] (see the module docs).
    pub fn narrowed(&self) -> bool {
        self.narrowed.is_some()
    }

    /// The part of [`PendingSnapshot::scan_time`] spent locking the working
    /// copy, which makes jj re-read its whole tree state from disk, and for
    /// a narrowed scan reading it once more (R1.30).
    pub fn load_time(&self) -> Duration {
        self.load
    }

    /// The part of [`PendingSnapshot::scan_time`] spent walking the files and
    /// hashing changed ones (R1.30).
    pub fn walk_time(&self) -> Duration {
        self.walk
    }

    /// The operation the repository was loaded at when the scan began: what
    /// to finish at when nothing is committed.
    pub fn head_op(&self) -> &OperationIdHex {
        &self.head_op
    }

    /// The view's working-copy commit at [`PendingSnapshot::head_op`]; the
    /// workspace's commit after an unchanged or adopted snapshot. `None`
    /// when the view has none (the workspace was forgotten).
    pub fn wc_commit(&self) -> Option<&CommitIdHex> {
        self.wc_commit.as_ref()
    }

    /// The change of [`PendingSnapshot::wc_commit`].
    pub fn wc_change(&self) -> Option<&ChangeIdHex> {
        self.wc_change.as_ref()
    }

    /// Files the adopt branch, or [`PendingSnapshot::check_out_carried`],
    /// updated, added, or removed on disk; zero otherwise.
    pub fn updated_files(&self) -> u32 {
        self.updated_files
    }

    /// Saves the working-copy state at `op_id`: the new operation after
    /// [`commit_snapshot`], or [`PendingSnapshot::head_op`] when
    /// [`PendingSnapshot::changed`] is false. Releases the lock.
    pub async fn finish(self, op_id: &OperationIdHex) -> Result<(), SnapshotError> {
        let id = OperationId::try_from_hex(op_id.as_str())
            .ok_or_else(|| SnapshotError::InvalidOperationId(op_id.clone()))?;
        debug_assert!(
            !self.carried() || self.checked_out,
            "a carried snapshot is checked out before its state is saved"
        );
        // The tree state before the operation, as jj's own `finish` orders
        // them: a crash between the two leaves a working copy whose files
        // already are its commit's, which the next snapshot adopts.
        if let Some(mut narrowed) = self.narrowed
            && narrowed.dirty
        {
            narrowed.tree_state.save()?;
        }
        self.locked.finish(id).await?;
        Ok(())
    }
}

/// What [`commit_snapshot`] wrote.
#[derive(Clone)]
pub struct SnapshotCommit {
    /// The repository at the new operation.
    pub repo: Arc<ReadonlyRepo>,
    /// The new operation.
    pub op_id: OperationIdHex,
    /// The new working-copy commit.
    pub commit_id: CommitIdHex,
    /// Its change: `rewrite_of`'s, or a new one when recovering.
    pub change_id: ChangeIdHex,
    /// The commit it rewrote: `rewrite_of`; `None` when recovering.
    pub rewrote: Option<CommitIdHex>,
    /// The rewritten commit was not the head's working-copy commit for the
    /// workspace, so the change now has more than one visible commit. True
    /// when the plan said so, and also when another workspace rewrote the
    /// commit between the scan and this transaction.
    pub divergent: bool,
    /// Writing the commit and rebasing descendants.
    pub write: Duration,
    /// Committing the operation.
    pub commit: Duration,
    /// When the operation ended, as jj recorded it: what R1.16's index
    /// stores, so a row and its rebuild from the log agree.
    pub end_time: SystemTime,
}

impl std::fmt::Debug for SnapshotCommit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotCommit")
            .field("op_id", &self.op_id)
            .field("commit_id", &self.commit_id)
            .field("change_id", &self.change_id)
            .field("divergent", &self.divergent)
            .finish_non_exhaustive()
    }
}

/// The transaction half of a snapshot: records `plan` on `head` in one
/// operation marked as a snapshot, with `meta`. Runs wherever the
/// repository's head lives; `head` may be newer than the operation the scan
/// started from.
pub async fn commit_snapshot(
    head: &Arc<ReadonlyRepo>,
    plan: &SnapshotPlan,
    meta: &OpMeta,
) -> Result<SnapshotCommit, SnapshotError> {
    let write_start = Instant::now();
    let store = head.store();
    let name = WorkspaceNameBuf::from(plan.workspace_name.as_str());
    let tree = MergedTree::new(
        Arc::clone(store),
        plan.tree_ids.clone(),
        plan.labels.clone(),
    );
    let mut tx = head.start_transaction();
    tx.set_is_snapshot(true);
    tx.set_workspace_name(&name);
    let current = tx.base_repo().view().get_wc_commit_id(&name).cloned();
    let (new_commit, divergent) = if let Some(parents) = &plan.recover {
        let parents = parents
            .iter()
            .map(|id| commit_id(head, id))
            .collect::<Result<Vec<_>, _>>()?;
        let commit = tx.repo_mut().new_commit(parents, tree).write().await?;
        (commit, false)
    } else {
        let predecessor = commit_by_hex(head, &plan.rewrite_of).await?;
        let divergent = plan.divergent || current.as_ref() != Some(predecessor.id());
        let commit = tx
            .repo_mut()
            .rewrite_commit(&predecessor)
            .set_tree(tree)
            .write()
            .await?;
        (commit, divergent)
    };
    tx.repo_mut().set_wc_commit(name, new_commit.id().clone())?;
    tx.repo_mut().rebase_descendants().await?;
    let write = write_start.elapsed();

    let commit_start = Instant::now();
    let repo = meta.commit(tx).await?;
    Ok(SnapshotCommit {
        op_id: OperationIdHex::of(repo.op_id()),
        commit_id: CommitIdHex::of(new_commit.id()),
        change_id: ChangeIdHex::of(new_commit.change_id()),
        rewrote: plan.recover.is_none().then(|| plan.rewrite_of.clone()),
        divergent,
        end_time: super::oplog::system_time(repo.operation().metadata().time.end),
        repo,
        write,
        commit: commit_start.elapsed(),
    })
}

/// `id` as a jj commit id of this store's length.
fn commit_id(repo: &ReadonlyRepo, id: &CommitIdHex) -> Result<CommitId, SnapshotError> {
    CommitId::try_from_hex(id.as_str())
        .filter(|commit_id| commit_id.as_bytes().len() == repo.store().commit_id_length())
        .ok_or_else(|| SnapshotError::ExpectedCommitMissing(id.clone()))
}

/// The commit with this id; [`SnapshotError::ExpectedCommitMissing`] if the
/// store has none.
async fn commit_by_hex(repo: &ReadonlyRepo, id: &CommitIdHex) -> Result<Commit, SnapshotError> {
    let commit_id = commit_id(repo, id)?;
    match repo.store().get_commit_async(&commit_id).await {
        Ok(commit) => Ok(commit),
        Err(BackendError::ObjectNotFound { .. }) => {
            Err(SnapshotError::ExpectedCommitMissing(id.clone()))
        }
        Err(error) => Err(error.into()),
    }
}

/// The workspace's working-copy commit in the view of the operation its
/// working copy was last updated at: the commit it last snapshotted, when
/// the caller does not say.
async fn last_own_commit(
    repo: &ReadonlyRepo,
    wc_op: &OperationId,
    name: &WorkspaceName,
) -> Result<Commit, SnapshotError> {
    let operation = repo.loader().load_operation(wc_op).await?;
    let view = operation.view().await?;
    let id = view
        .get_wc_commit_id(name)
        .ok_or_else(|| ReadError::NoWorkingCopyCommit(name.as_str().to_owned()))?;
    Ok(repo
        .store()
        .get_commit_async(id)
        .await
        .map_err(ReadError::from)?)
}

/// How a path differs between two trees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// Absent before, present after.
    Added,
    /// Present in both with different content or type.
    Modified,
    /// Present before, absent after.
    Removed,
}

/// One path that differs between the working-copy commit and its parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedPath {
    /// Repository-relative path with `/` separators.
    pub path: String,
    /// How it changed.
    pub kind: ChangeKind,
}

/// Failure to diff two trees.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DiffError {
    /// The working-copy commit could not be read.
    #[error(transparent)]
    Read(#[from] ReadError),
    /// A tree or file could not be read.
    #[error("failed to read a tree")]
    Backend(#[from] BackendError),
}

impl Classify for DiffError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Read(error) => error.kind(),
            Self::Backend(error) => classify_by_source(error),
        }
    }
}

impl Repo {
    /// Snapshots the working copy into this workspace's working-copy commit
    /// and records an operation marked as a snapshot, following jj-cli's
    /// `snapshot_working_copy`. `description` becomes the operation's
    /// description.
    ///
    /// New files above `snapshot.max-new-file-size` are left untracked and
    /// reported in [`SnapshotOutcome::untracked`]. Files already tracked are
    /// always snapshotted. A stale working copy is handled as
    /// [`Repo::begin_snapshot`] describes; a working copy newer than the
    /// loaded repository is [`SnapshotError::RepoBehind`].
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn snapshot(&mut self, description: &str) -> Result<SnapshotOutcome, SnapshotError> {
        self.snapshot_unlogged(description)
            .await
            .inspect_err(|error| crate::log_failure!(error))
    }

    /// [`Repo::snapshot`] without its span or failure event, for operations
    /// that snapshot as one of their steps and report failures themselves.
    pub(crate) async fn snapshot_unlogged(
        &mut self,
        description: &str,
    ) -> Result<SnapshotOutcome, SnapshotError> {
        let begun = Instant::now();
        let head = Arc::clone(&self.repo);
        let name = self.workspace_name().to_owned();
        let pending = self
            .begin_snapshot(None, None, &IgnoreSources::default(), None)
            .await?;
        let scan = pending.scan_time();
        let untracked = pending.untracked().to_vec();
        if !untracked.is_empty() {
            tracing::warn!(
                untracked_count = untracked.len(),
                "new files left untracked"
            );
        }

        if !pending.changed() {
            // Nothing to record, but save the working-copy state anyway, as
            // jj-cli does, so refreshed file metadata is not rescanned.
            let commit_start = Instant::now();
            let op_id = pending.head_op().clone();
            let wc_commit = pending
                .wc_commit()
                .cloned()
                .ok_or(ReadError::NoWorkingCopyCommit(name))?;
            pending.finish(&op_id).await?;
            tracing::debug!(
                op_id = %op_id,
                elapsed_ms = elapsed_ms(begun),
                "working copy unchanged"
            );
            return Ok(SnapshotOutcome {
                changed: false,
                op_id,
                wc_commit,
                untracked,
                stats: SnapshotTiming {
                    scan,
                    write: Duration::ZERO,
                    commit: commit_start.elapsed(),
                },
            });
        }

        let plan = pending.plan().clone();
        let meta = OpMeta::new(OpKind::Snapshot, description);
        let committed = commit_snapshot(&head, &plan, &meta).await?;
        let finish_start = Instant::now();
        let finished = pending.finish(&committed.op_id).await;
        // The operation is published once committed. Adopt it even if saving
        // the working-copy state failed, so the next snapshot sees a stale
        // working copy instead of building a sibling operation on the old one.
        self.repo = Arc::clone(&committed.repo);
        finished?;
        tracing::info!(
            op_id = %committed.op_id,
            commit_id = %committed.commit_id,
            elapsed_ms = elapsed_ms(begun),
            "snapshot taken"
        );
        Ok(SnapshotOutcome {
            changed: true,
            op_id: committed.op_id,
            wc_commit: committed.commit_id,
            untracked,
            stats: SnapshotTiming {
                scan,
                write: committed.write,
                commit: committed.commit + finish_start.elapsed(),
            },
        })
    }

    /// Locks the working copy, checks it against the loaded repository, and
    /// scans it. `expected_commit` is the commit the caller last
    /// snapshotted this workspace into; `None` takes it from the view of
    /// the operation the working copy was last updated at.
    ///
    /// The freshness check is jj's `WorkingCopyFreshness::check_stale`,
    /// with Reef's policy for each answer (ADR 0018):
    ///
    /// - fresh: plan a rewrite of the view's working-copy commit;
    /// - the working copy is newer than the loaded repository, or a sibling
    ///   of it: [`SnapshotError::RepoBehind`]; reload at head and retry;
    /// - stale (another operation rewrote the working-copy commit) with no
    ///   edits since the last update, or with files that already match the
    ///   rewritten commit: check it out on disk
    ///   ([`PendingSnapshot::adopted`]); nothing to commit;
    /// - stale with edits, and `carry` names this move (its `to` is the
    ///   view's commit and its `from` is `expected_commit`): plan a rewrite
    ///   of `to` with the three-way merge of `to`'s tree and the scanned
    ///   one over the carry's base ([`SnapshotPlan::carried`]), or, when the
    ///   merge is `to`'s tree, check it out as above (ADR 0039);
    /// - stale with edits otherwise: plan a rewrite of `expected_commit`
    ///   instead, which makes the change divergent
    ///   ([`SnapshotPlan::divergent`]);
    /// - no working-copy commit in the view (the workspace was forgotten):
    ///   plan a new commit on `expected_commit`'s parents
    ///   ([`SnapshotPlan::recover`]).
    ///
    /// `prefixes`, workspace-relative paths, narrows the scan to them and
    /// everything under them when the working copy is fresh and the view has
    /// its working-copy commit (see the module docs); `None` scans
    /// everything. A path that cannot be narrowed to (the root, `..`, not
    /// UTF-8, or on macOS one whose spelling on disk cannot be read) scans
    /// everything. Pass prefixes only for a working copy loaded with the
    /// repository's own settings, without Watchman.
    #[allow(
        clippy::too_many_lines,
        reason = "one match over ADR 0018's arms; the carried arm (ADR 0039) tipped it over"
    )]
    pub async fn begin_snapshot(
        &mut self,
        expected_commit: Option<&CommitIdHex>,
        carry: Option<&Carry>,
        extra: &IgnoreSources<'_>,
        prefixes: Option<&[PathBuf]>,
    ) -> Result<PendingSnapshot<'_>, SnapshotError> {
        let max_new_file_size = max_new_file_size(self.workspace.settings())?;
        let base_ignores = self.ignores(extra)?;
        let repo = Arc::clone(&self.repo);
        let name = self.workspace.workspace_name().to_owned();
        let head_op = OperationIdHex::of(repo.op_id());
        let narrow = match prefixes {
            Some(prefixes) => self.narrow_to(prefixes)?,
            None => None,
        };

        let start = Instant::now();
        let mut locked = self.workspace.start_working_copy_mutation().await?;
        let mut load = start.elapsed();
        let view_wc = match repo.view().get_wc_commit_id(&name) {
            Some(id) => Some(
                repo.store()
                    .get_commit_async(id)
                    .await
                    .map_err(ReadError::from)?,
            ),
            None => None,
        };
        // Without a working-copy commit in the view the check still
        // classifies the two operations; the commit only feeds its tree
        // comparison, which then does not matter (every non-behind answer
        // recovers).
        let probe = view_wc
            .clone()
            .unwrap_or_else(|| repo.store().root_commit());
        let wc_op = locked.locked_wc().old_operation_id().clone();
        let stale = is_stale(&mut locked, &probe, &repo, &head_op).await?;

        let old_tree = locked.locked_wc().old_tree().clone();
        let options = SnapshotOptions {
            base_ignores,
            progress: None,
            start_tracking_matcher: &EverythingMatcher,
            force_tracking_matcher: &NothingMatcher,
            max_new_file_size,
        };
        // Stale or forgotten working copies adopt, diverge, or recover from
        // a full scan.
        let narrow = narrow.filter(|_| !stale && view_wc.is_some());
        let (new_tree, stats, walk, narrowed) = scan(&mut locked, narrow, &repo, &options).await?;
        load += narrowed
            .as_ref()
            .map_or(Duration::ZERO, |narrowed| narrowed.load);
        let untracked = untracked(stats.untracked_paths);
        let (tree_ids, labels) = new_tree.tree_ids_and_labels();
        let (mut tree_ids, mut labels) = (tree_ids.clone(), labels.clone());
        let mut carried = None;

        let own_last = async || match expected_commit {
            Some(id) => commit_by_hex(&repo, id).await,
            None => last_own_commit(&repo, &wc_op, &name).await,
        };
        let mut adopted = false;
        let mut updated_files = 0;
        let (rewrite_of, changed, divergent, recover) = match &view_wc {
            None => {
                let last = own_last().await?;
                let parents = last.parent_ids().iter().map(CommitIdHex::of).collect();
                (CommitIdHex::of(last.id()), true, false, Some(parents))
            }
            Some(wc) if !stale => {
                let changed = new_tree.tree_ids_and_labels() != wc.tree().tree_ids_and_labels();
                (CommitIdHex::of(wc.id()), changed, false, None)
            }
            Some(wc)
                if new_tree.tree_ids_and_labels() == old_tree.tree_ids_and_labels()
                    || new_tree.tree_ids_and_labels() == wc.tree().tree_ids_and_labels() =>
            {
                // Nothing of the workspace's own would be lost: follow the
                // rewrite. The second case is a working copy whose state was
                // not saved after its own snapshot was committed (a failed
                // finish, a crash): the files already are the commit's.
                updated_files = check_out(&mut locked, wc).await?;
                adopted = true;
                (CommitIdHex::of(wc.id()), false, false, None)
            }
            Some(wc) => {
                let last = own_last().await?;
                let trees = (&old_tree, &new_tree);
                match carried_tree(&repo, (wc, &last), carry, trees).await? {
                    Some(merged) if merged.tree_ids() == wc.tree_ids() => {
                        // The edits were already the write's.
                        updated_files = check_out(&mut locked, wc).await?;
                        adopted = true;
                        (CommitIdHex::of(wc.id()), false, false, None)
                    }
                    Some(merged) => {
                        carried = Some(ScannedTree { tree_ids, labels });
                        let (merged_ids, merged_labels) = merged.tree_ids_and_labels();
                        (tree_ids, labels) = (merged_ids.clone(), merged_labels.clone());
                        (CommitIdHex::of(wc.id()), true, false, None)
                    }
                    None => {
                        let divergent = last.id() != wc.id();
                        (CommitIdHex::of(last.id()), true, divergent, None)
                    }
                }
            }
        };
        Ok(PendingSnapshot {
            locked,
            plan: SnapshotPlan {
                workspace_name: name.as_str().to_owned(),
                rewrite_of,
                tree_ids,
                labels,
                divergent,
                recover,
                carried,
            },
            changed,
            adopted,
            head_op,
            wc_commit: view_wc.as_ref().map(|wc| CommitIdHex::of(wc.id())),
            wc_change: view_wc.as_ref().map(|wc| ChangeIdHex::of(wc.change_id())),
            untracked,
            scan: start.elapsed(),
            load,
            walk,
            updated_files,
            narrowed,
            checked_out: false,
        })
    }

    /// How to load the tree state for a scan narrowed to `prefixes`; `None`
    /// to scan everything (see [`Repo::begin_snapshot`]).
    fn narrow_to(&self, prefixes: &[PathBuf]) -> Result<Option<NarrowedLoad>, SnapshotError> {
        let Some(local) = self
            .workspace
            .working_copy()
            .downcast_ref::<LocalWorkingCopy>()
        else {
            return Ok(None);
        };
        let root = self.workspace_root().to_owned();
        let Some(changed_files) = monitor_paths(&root, prefixes) else {
            return Ok(None);
        };
        let mut settings = TreeStateSettings::try_from_user_settings(self.workspace.settings())
            .map_err(|error| WorkingCopyStateError {
                message: "Failed to read the tree state settings".to_owned(),
                err: error.into(),
            })?;
        settings.fsmonitor_settings = FsmonitorSettings::Test { changed_files };
        Ok(Some(NarrowedLoad {
            root,
            state_path: local.state_path().to_owned(),
            settings,
        }))
    }

    /// The tree this workspace's working copy recorded last, read from its
    /// state on disk. The in-memory working copy keeps the tree it was
    /// locked with, which a narrowed snapshot does not update (see the
    /// module docs).
    pub(crate) fn recorded_tree(&self) -> Result<MergedTree, WorkingCopyStateError> {
        let wc = self.workspace.working_copy();
        let state_path = wc
            .downcast_ref::<LocalWorkingCopy>()
            .map(LocalWorkingCopy::state_path)
            // A missing tree state would be created by the load, unlocked.
            .filter(|path| path.join("tree_state").is_file());
        let Some(state_path) = state_path else {
            return Ok(wc.tree()?.clone());
        };
        let wrap =
            |message: &str, err: Box<dyn std::error::Error + Send + Sync>| WorkingCopyStateError {
                message: message.to_owned(),
                err,
            };
        let settings = TreeStateSettings::try_from_user_settings(self.workspace.settings())
            .map_err(|error| wrap("Failed to read the tree state settings", error.into()))?;
        let tree_state = TreeState::load(
            Arc::clone(self.repo.store()),
            self.workspace_root().to_owned(),
            state_path.to_owned(),
            &settings,
        )
        .map_err(|error| wrap("Failed to read working copy state", error.into()))?;
        Ok(tree_state.current_tree().clone())
    }

    /// Paths that differ between the working-copy commit and its parent, in
    /// path order. Reflects the last snapshot, not the files on disk.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn changed_paths_in_wc(&self) -> Result<Vec<ChangedPath>, DiffError> {
        self.changed_paths_in_wc_unlogged()
            .await
            .inspect_err(|error| crate::log_failure!(error))
    }

    async fn changed_paths_in_wc_unlogged(&self) -> Result<Vec<ChangedPath>, DiffError> {
        let wc_commit = self.wc_commit()?;
        let parent_tree = wc_commit.parent_tree(self.repo.as_ref()).await?;
        let wc_tree = wc_commit.tree();
        let mut diff = parent_tree.diff_stream(&wc_tree, &EverythingMatcher);
        let mut changed = Vec::new();
        while let Some(entry) = diff.next().await {
            let values = entry.values?;
            let kind = if values.before.is_absent() {
                ChangeKind::Added
            } else if values.after.is_absent() {
                ChangeKind::Removed
            } else {
                ChangeKind::Modified
            };
            changed.push(ChangedPath {
                path: entry.path.as_internal_file_string().to_owned(),
                kind,
            });
        }
        Ok(changed)
    }
}

/// Walks the working copy: narrowed through its own load of the tree state
/// when `narrow` says how, else through the locked working copy. Returns
/// the scanned tree, jj's stats, the walk's time, and the narrowed load.
async fn scan(
    locked: &mut LockedWorkspace<'_>,
    narrow: Option<NarrowedLoad>,
    repo: &ReadonlyRepo,
    options: &SnapshotOptions<'_>,
) -> Result<(MergedTree, SnapshotStats, Duration, Option<Narrowed>), SnapshotError> {
    let Some(narrow) = narrow else {
        let walk_start = Instant::now();
        let (new_tree, stats) = locked.locked_wc().snapshot(options).await?;
        return Ok((new_tree, stats, walk_start.elapsed(), None));
    };
    let load_start = Instant::now();
    let mut tree_state = TreeState::load(
        Arc::clone(repo.store()),
        narrow.root,
        narrow.state_path,
        &narrow.settings,
    )?;
    let load = load_start.elapsed();
    let walk_start = Instant::now();
    let (dirty, stats) = tree_state.snapshot(options).await?;
    let walk = walk_start.elapsed();
    let new_tree = tree_state.current_tree().clone();
    let narrowed = Narrowed {
        tree_state,
        dirty,
        load,
    };
    Ok((new_tree, stats, walk, Some(narrowed)))
}

/// Checks `commit` out in the locked working copy; returns the files it
/// updated, added, or removed.
async fn check_out(
    locked: &mut LockedWorkspace<'_>,
    commit: &Commit,
) -> Result<u32, CheckoutError> {
    let stats = locked.locked_wc().check_out(commit).await?;
    Ok(stats.updated_files + stats.added_files + stats.removed_files)
}

/// A carried snapshot's tree, when `carry` names this move: `to` is `wc`,
/// the view's commit (the write's), and `from` is `last`, the commit the
/// working copy was checked out from. The tree is the merge of `wc`'s and
/// the scanned `disk` over the carry's base, by default `recorded`, the tree
/// the working copy last recorded (ADR 0039). `None` otherwise.
async fn carried_tree(
    repo: &ReadonlyRepo,
    (wc, last): (&Commit, &Commit),
    carry: Option<&Carry>,
    (recorded, disk): (&MergedTree, &MergedTree),
) -> Result<Option<MergedTree>, BackendError> {
    let Some(carry) = carry.filter(|carry| {
        carry.to.as_str() == wc.id().hex() && carry.from.as_str() == last.id().hex()
    }) else {
        return Ok(None);
    };
    let base = match &carry.base {
        Some(base) => MergedTree::new(
            Arc::clone(repo.store()),
            base.tree_ids.clone(),
            base.labels.clone(),
        ),
        None => recorded.clone(),
    };
    MergedTree::merge(Merge::from_vec(vec![
        (wc.tree(), format!("{} (rewritten)", wc.conflict_label())),
        (base, format!("{} (snapshotted)", last.conflict_label())),
        (disk.clone(), "working copy".to_owned()),
    ]))
    .await
    .map(Some)
}

/// Whether the locked working copy is stale against `repo`, by jj's
/// freshness check; [`SnapshotError::RepoBehind`] when it is newer than
/// `repo` or a sibling of it (ADR 0018). `probe` is the view's working-copy
/// commit.
async fn is_stale(
    locked: &mut LockedWorkspace<'_>,
    probe: &Commit,
    repo: &ReadonlyRepo,
    head_op: &OperationIdHex,
) -> Result<bool, SnapshotError> {
    let behind = |kind, locked: &mut LockedWorkspace<'_>| SnapshotError::RepoBehind {
        kind,
        wc_op: OperationIdHex::of(locked.locked_wc().old_operation_id()),
        repo_op: head_op.clone(),
    };
    match WorkingCopyFreshness::check_stale(locked.locked_wc(), probe, repo).await? {
        WorkingCopyFreshness::Fresh => Ok(false),
        WorkingCopyFreshness::WorkingCopyStale => Ok(true),
        WorkingCopyFreshness::Updated(_) => Err(behind(Behind::WorkingCopyNewer, locked)),
        WorkingCopyFreshness::SiblingOperation => Err(behind(Behind::SiblingOperation, locked)),
    }
}

/// Where and how a narrowed scan loads the tree state.
struct NarrowedLoad {
    root: PathBuf,
    state_path: PathBuf,
    /// The repository's, with jj's test monitor reporting the paths.
    settings: TreeStateSettings,
}

/// The changed files jj's test monitor reports for `prefixes`: each path,
/// which covers it as a file, and the `.gitignore` under it, which jj's
/// `make_fsmonitor_matcher` turns into a rescan of everything under the
/// path, which covers it as a directory. On macOS also each path's spelling
/// on disk where it differs, since jj compares paths byte for byte and a
/// case-insensitive volume reports the spelling a writer used. `None` when a
/// path cannot be narrowed to (see [`Repo::begin_snapshot`]).
fn monitor_paths(root: &Path, prefixes: &[PathBuf]) -> Option<Vec<PathBuf>> {
    #[cfg(target_os = "macos")]
    let spelling = spelling::Resolver::new(root);
    #[cfg(not(target_os = "macos"))]
    let _ = root;
    let mut paths = Vec::with_capacity(prefixes.len() * 2);
    let mut push = |path: &Path| {
        paths.push(path.to_owned());
        paths.push(path.join(".gitignore"));
    };
    for prefix in prefixes {
        let repo_path = RepoPathBuf::from_relative_path(prefix).ok()?;
        if repo_path.is_root() {
            return None;
        }
        push(prefix);
        #[cfg(target_os = "macos")]
        {
            let spelled = spelling.on_disk(prefix)?;
            if spelled != *prefix {
                push(&spelled);
            }
        }
    }
    Some(paths)
}

/// The spelling of a path on a case-insensitive volume (R1.30).
#[cfg(target_os = "macos")]
mod spelling {
    use std::ffi::{CString, OsStr};
    use std::io;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::{Path, PathBuf};

    /// Resolves paths under one root.
    pub(super) struct Resolver<'a> {
        root: &'a Path,
        /// The root as the volume spells it; `None` if it cannot be read.
        spelled_root: Option<PathBuf>,
    }

    impl<'a> Resolver<'a> {
        pub(super) fn new(root: &'a Path) -> Self {
            Self {
                root,
                spelled_root: get_path(root).ok(),
            }
        }

        /// `relative` as the volume spells it: the longest part of it that
        /// exists, spelled from disk, then the rest as given. `None` when it
        /// cannot be read (a permission, a hard link to outside the root).
        pub(super) fn on_disk(&self, relative: &Path) -> Option<PathBuf> {
            let mut existing = relative.to_path_buf();
            let mut missing = Vec::new();
            loop {
                if existing.as_os_str().is_empty() {
                    return Some(relative.to_owned());
                }
                match std::fs::symlink_metadata(self.root.join(&existing)) {
                    Ok(_) => break,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                        ) =>
                    {
                        missing.push(existing.file_name()?.to_owned());
                        existing.pop();
                    }
                    Err(_) => return None,
                }
            }
            let spelled = get_path(&self.root.join(&existing)).ok()?;
            let mut spelled = spelled
                .strip_prefix(self.spelled_root.as_deref()?)
                .ok()?
                .to_path_buf();
            spelled.extend(missing.iter().rev());
            Some(spelled)
        }
    }

    /// `fcntl(F_GETPATH)` on `path` itself (a symlink is not followed): the
    /// path as the volume spells it.
    fn get_path(path: &Path) -> io::Result<PathBuf> {
        let c_path = CString::new(path.as_os_str().as_bytes())?;
        let flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_SYMLINK;
        #[allow(unsafe_code, reason = "FFI; the path is a NUL-terminated string")]
        // SAFETY: `c_path` is a valid C string for the whole call.
        let fd = unsafe { libc::open(c_path.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        #[allow(unsafe_code, reason = "taking ownership of a descriptor just opened")]
        // SAFETY: `fd` is open and owned by nothing else.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut buf = vec![0_u8; usize::try_from(libc::PATH_MAX).unwrap_or(1024)];
        #[allow(
            unsafe_code,
            reason = "FFI; the buffer is PATH_MAX bytes, as F_GETPATH needs"
        )]
        // SAFETY: `F_GETPATH` writes at most `PATH_MAX` bytes, NUL included,
        // into `buf`, which is that long.
        let result = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        let len = buf.iter().position(|&byte| byte == 0).unwrap_or(buf.len());
        Ok(PathBuf::from(OsStr::from_bytes(&buf[..len])))
    }
}

/// Reef's form of the paths a snapshot left untracked.
fn untracked(
    paths: impl IntoIterator<Item = (RepoPathBuf, UntrackedReason)>,
) -> Vec<(String, UntrackedReasonKind)> {
    paths
        .into_iter()
        .map(|(path, reason)| {
            let kind = match reason {
                UntrackedReason::FileTooLarge { size, max_size } => {
                    UntrackedReasonKind::FileTooLarge { size, max_size }
                }
                UntrackedReason::FileNotAutoTracked => UntrackedReasonKind::NotAutoTracked,
            };
            (path.as_internal_file_string().to_owned(), kind)
        })
        .collect()
}
