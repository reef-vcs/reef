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

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use jj_lib::backend::{BackendError, CommitId, TreeId};
use jj_lib::commit::Commit;
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::gitignore::GitIgnoreError;
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
    CheckoutError, SnapshotOptions, UntrackedReason, WorkingCopyFreshness, WorkingCopyStateError,
};
use jj_lib::workspace::LockedWorkspace;

use super::ignore::IgnoreSources;
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
            Self::OpStore(error) => classify_by_source(error),
            Self::Snapshot(error) => classify_by_source(error),
            Self::Checkout(error) => classify_by_source(error),
            Self::Backend(error) => classify_by_source(error),
            Self::Commit(error) => classify_by_source(error),
        }
    }
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
    updated_files: u32,
}

impl std::fmt::Debug for PendingSnapshot<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingSnapshot")
            .field("plan", &self.plan)
            .field("changed", &self.changed)
            .field("adopted", &self.adopted)
            .field("head_op", &self.head_op)
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

    /// New files the scan left untracked, by repository-relative path.
    pub fn untracked(&self) -> &[(String, UntrackedReasonKind)] {
        &self.untracked
    }

    /// How long locking, the freshness check, and the scan took.
    pub fn scan_time(&self) -> Duration {
        self.scan
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

    /// Files the adopt branch updated, added, or removed on disk; zero
    /// otherwise.
    pub fn updated_files(&self) -> u32 {
        self.updated_files
    }

    /// Saves the working-copy state at `op_id`: the new operation after
    /// [`commit_snapshot`], or [`PendingSnapshot::head_op`] when
    /// [`PendingSnapshot::changed`] is false. Releases the lock.
    pub async fn finish(self, op_id: &OperationIdHex) -> Result<(), SnapshotError> {
        let id = OperationId::try_from_hex(op_id.as_str())
            .ok_or_else(|| SnapshotError::InvalidOperationId(op_id.clone()))?;
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
    /// The rewritten commit was not the head's working-copy commit for the
    /// workspace, so the change now has more than one visible commit. True
    /// when the plan said so, and also when another workspace rewrote the
    /// commit between the scan and this transaction.
    pub divergent: bool,
    /// Writing the commit and rebasing descendants.
    pub write: Duration,
    /// Committing the operation.
    pub commit: Duration,
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
/// operation marked as a snapshot, with `description`. Runs wherever the
/// repository's head lives; `head` may be newer than the operation the scan
/// started from.
pub async fn commit_snapshot(
    head: &Arc<ReadonlyRepo>,
    plan: &SnapshotPlan,
    description: &str,
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
    let repo = tx.commit(description).await?;
    Ok(SnapshotCommit {
        op_id: OperationIdHex::of(repo.op_id()),
        commit_id: CommitIdHex::of(new_commit.id()),
        change_id: ChangeIdHex::of(new_commit.change_id()),
        divergent,
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
        let pending = self.begin_snapshot(None, &IgnoreSources::default()).await?;
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
        let committed = commit_snapshot(&head, &plan, description).await?;
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
    /// - stale with edits: plan a rewrite of `expected_commit` instead, which
    ///   makes the change divergent ([`SnapshotPlan::divergent`]);
    /// - no working-copy commit in the view (the workspace was forgotten):
    ///   plan a new commit on `expected_commit`'s parents
    ///   ([`SnapshotPlan::recover`]).
    pub async fn begin_snapshot(
        &mut self,
        expected_commit: Option<&CommitIdHex>,
        extra: &IgnoreSources<'_>,
    ) -> Result<PendingSnapshot<'_>, SnapshotError> {
        let max_new_file_size = max_new_file_size(self.workspace.settings())?;
        let base_ignores = self.ignores(extra)?;
        let repo = Arc::clone(&self.repo);
        let name = self.workspace.workspace_name().to_owned();
        let head_op = OperationIdHex::of(repo.op_id());

        let start = Instant::now();
        let mut locked = self.workspace.start_working_copy_mutation().await?;
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
        let behind = |kind| SnapshotError::RepoBehind {
            kind,
            wc_op: OperationIdHex::of(&wc_op),
            repo_op: head_op.clone(),
        };
        let stale =
            match WorkingCopyFreshness::check_stale(locked.locked_wc(), &probe, &repo).await? {
                WorkingCopyFreshness::Fresh => false,
                WorkingCopyFreshness::WorkingCopyStale => true,
                WorkingCopyFreshness::Updated(_) => return Err(behind(Behind::WorkingCopyNewer)),
                WorkingCopyFreshness::SiblingOperation => {
                    return Err(behind(Behind::SiblingOperation));
                }
            };

        let old_tree = locked.locked_wc().old_tree().clone();
        let options = SnapshotOptions {
            base_ignores,
            progress: None,
            start_tracking_matcher: &EverythingMatcher,
            force_tracking_matcher: &NothingMatcher,
            max_new_file_size,
        };
        let (new_tree, stats) = locked.locked_wc().snapshot(&options).await?;
        let untracked = untracked(stats.untracked_paths);
        let (tree_ids, labels) = new_tree.tree_ids_and_labels();
        let (tree_ids, labels) = (tree_ids.clone(), labels.clone());

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
                let stats = locked.locked_wc().check_out(wc).await?;
                adopted = true;
                updated_files = stats.updated_files + stats.added_files + stats.removed_files;
                (CommitIdHex::of(wc.id()), false, false, None)
            }
            Some(wc) => {
                let last = own_last().await?;
                let divergent = last.id() != wc.id();
                (CommitIdHex::of(last.id()), true, divergent, None)
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
            },
            changed,
            adopted,
            head_op,
            wc_commit: view_wc.as_ref().map(|wc| CommitIdHex::of(wc.id())),
            wc_change: view_wc.as_ref().map(|wc| ChangeIdHex::of(wc.change_id())),
            untracked,
            scan: start.elapsed(),
            updated_files,
        })
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
