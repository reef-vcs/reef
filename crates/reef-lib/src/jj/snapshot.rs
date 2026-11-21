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
//! A stale working copy with edits of its own is not always ADR 0018's
//! divergence. When Reef itself moved the workspace's working-copy commit
//! (a write, issue #80 and ADR 0039, or another workspace's snapshot that
//! rebased it, ADR 0084), the snapshot records the three-way merge of the
//! commit's tree and the files on disk, over the tree of the commit the
//! working copy was checked out from, as a rewrite of the view's commit,
//! instead of diverging. Who moved it is read from the operation log
//! ([`moved_by_reef`]), at the head the snapshot scans with or commits on:
//! nothing is handed from the mover to the workspace. The merged tree also
//! holds the mover's changes, which are not on disk:
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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
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
use jj_lib::operation::Operation;
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
use super::revset::{self, ImmutableHead, RevisionError};
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
    /// A file a carried snapshot's check-out would overwrite changed on disk
    /// after the scan; nothing was checked out (ADR 0039).
    #[error("{0} changed after the scan; not overwriting it")]
    ChangedAfterScan(String),
    /// Whether the commit to rewrite is immutable could not be learned
    /// (issue #248).
    #[error("failed to check whether the working-copy commit is immutable")]
    Immutability(#[source] RevisionError),
}

impl Classify for SnapshotError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::RepoBehind { .. }
            | Self::ChangedAfterScan(_)
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
            Self::Immutability(error) => error.kind(),
        }
    }
}

/// A carried snapshot's commit whose check-out failed: the commit holds
/// `disk`, the tree that snapshot scanned, and the files do not hold the
/// rest of the commit yet (ADR 0039). Kept by the workspace and given to
/// its next [`Repo::begin_snapshot`], which measures its edits from `disk`
/// rather than from the commit the working copy was checked out from, so
/// they are not taken twice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unfinished {
    /// The commit that snapshot wrote.
    pub commit: CommitIdHex,
    /// The tree it scanned.
    pub disk: ScannedTree,
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
    /// `Some(disk)`: the scan was carried onto `rewrite_of`, the commit
    /// Reef moved the workspace to ([`moved_by_reef`]): `tree_ids` is the merge,
    /// and `disk` is the tree on disk it merged, to merge over again if the
    /// check-out fails ([`Unfinished`]).
    pub carried: Option<ScannedTree>,
    /// `Some`: `rewrite_of` is immutable (on `trunk()` or under a tag), so
    /// the scanned tree is recorded as a new commit on it, with a new change
    /// id, and the working copy moves there, rather than rewriting a commit
    /// that has landed (issue #248). `divergent` is then false.
    pub started_on: Option<StartedOn>,
}

/// The immutable commit a snapshot started a new change on (issue #248).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartedOn {
    /// The commit: the new commit's parent.
    pub commit: CommitIdHex,
    /// What made it immutable.
    pub head: ImmutableHead,
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
    /// Why the scan was not narrowed, when it was not.
    full_reason: Option<FullScanReason>,
    /// [`PendingSnapshot::check_out_carried`] ran.
    checked_out: bool,
    /// For a carried scan, each file its check-out would overwrite or
    /// remove, as the scan left it.
    stamps: Vec<(PathBuf, Option<Stamp>)>,
    /// The tree the scan read on disk.
    scanned: MergedTree,
    /// The workspace's root.
    root: PathBuf,
}

/// Why a scan walked every tracked file rather than the paths given to
/// [`Repo::begin_snapshot`] (R2.23).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FullScanReason {
    /// No paths were given.
    NotAsked,
    /// A path cannot be narrowed to (the root, `..`, not UTF-8, a macOS
    /// spelling that cannot be read), or the working copy is not jj's local
    /// one.
    NotNarrowable,
    /// The working copy is stale against the view: ADR 0018's adopt,
    /// divergent and carried arms scan everything.
    Stale,
    /// The view has no working-copy commit for the workspace: the recover
    /// arm scans everything.
    NoWorkingCopyCommit,
}

/// What a file on disk looked like, to tell whether it changed: a writer
/// changes at least one of these.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    symlink: bool,
    #[cfg(unix)]
    inode: (u64, i64, i64),
}

impl Stamp {
    /// `path`'s stamp; `None` when nothing is there.
    fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::symlink_metadata(path).ok()?;
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt as _;
            (metadata.ino(), metadata.ctime(), metadata.ctime_nsec())
        };
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            symlink: metadata.file_type().is_symlink(),
            #[cfg(unix)]
            inode,
        })
    }
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
    /// or removed. A file among them that changed on disk since the scan is
    /// [`SnapshotError::ChangedAfterScan`], with nothing checked out: jj's
    /// check-out would overwrite it unread. A commit merged again when it
    /// was committed ([`onto_moved`]) has paths the scan took no stamp of:
    /// each must still hold, byte for byte, what the scan read.
    pub async fn check_out_carried(
        &mut self,
        repo: &ReadonlyRepo,
        commit: &CommitIdHex,
    ) -> Result<u32, SnapshotError> {
        let commit = commit_by_hex(repo, commit).await?;
        if let Some((path, _)) = self
            .stamps
            .iter()
            .find(|(path, stamp)| Stamp::of(path) != *stamp)
        {
            return Err(SnapshotError::ChangedAfterScan(path.display().to_string()));
        }
        if let Some(path) = self.changed_unstamped(&commit.tree()).await? {
            return Err(SnapshotError::ChangedAfterScan(path.display().to_string()));
        }
        self.updated_files = check_out(&mut self.locked, &commit).await?;
        self.checked_out = true;
        Ok(self.updated_files)
    }

    /// The first file the check-out of `merged` would overwrite or remove,
    /// with no stamp, that no longer holds what the scan read. The
    /// check-out diffs from the locked working copy's tree: the scanned
    /// tree after a full scan, but the tree before the scan after a
    /// narrowed one, whose paths are then checked too.
    async fn changed_unstamped(
        &mut self,
        merged: &MergedTree,
    ) -> Result<Option<PathBuf>, BackendError> {
        let stamped: HashSet<&Path> = self.stamps.iter().map(|(path, _)| path.as_path()).collect();
        let store = self.scanned.store();
        let mut seen = HashSet::new();
        let mut diff = self.scanned.diff(merged, &EverythingMatcher);
        while let Some(entry) = diff.next().await {
            let before = entry.values?.before;
            // jj's check-out skips a file it does not track.
            if !before.is_present() {
                continue;
            }
            let path = entry.path.to_fs_path_unchecked(&self.root);
            if !stamped.contains(path.as_path())
                && !holds(store, &entry.path, &before, &path).await?
            {
                return Ok(Some(path));
            }
            seen.insert(entry.path);
        }
        if self.narrowed.is_none() {
            return Ok(None);
        }
        let old = self.locked.locked_wc().old_tree().clone();
        let mut diff = old.diff(merged, &EverythingMatcher);
        while let Some(entry) = diff.next().await {
            entry.values?;
            if seen.contains(&entry.path) {
                continue;
            }
            let path = entry.path.to_fs_path_unchecked(&self.root);
            if stamped.contains(path.as_path()) {
                continue;
            }
            let scanned = self.scanned.path_value(&entry.path).await?;
            let holds = if scanned.is_present() {
                holds(store, &entry.path, &scanned, &path).await?
            } else {
                std::fs::symlink_metadata(&path).is_err()
            };
            if !holds {
                return Ok(Some(path));
            }
        }
        Ok(None)
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

    /// Why the scan was not narrowed; `None` when it was.
    pub fn full_reason(&self) -> Option<FullScanReason> {
        self.full_reason
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
    /// Its change: `rewrite_of`'s, or a new one when recovering or starting
    /// on an immutable commit.
    pub change_id: ChangeIdHex,
    /// Its tree, in lowercase hex; `None` when the tree is conflicted
    /// (R3.04's provenance records it).
    pub tree_id: Option<String>,
    /// The commit it rewrote: `rewrite_of`; `None` when recovering or
    /// starting on an immutable commit.
    pub rewrote: Option<CommitIdHex>,
    /// The immutable commit it started a new change on, instead of
    /// rewriting it (#248): the plan's, or one that landed between the scan
    /// and the commit.
    pub started_on: Option<CommitIdHex>,
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
    let mut new_parents = match (&plan.recover, &plan.started_on) {
        (Some(parents), _) => Some(parents.clone()),
        (None, Some(started_on)) => Some(vec![started_on.commit.clone()]),
        (None, None) => None,
    };
    // `head` may be newer than the scan: a commit that landed since (a
    // fetch fast-forwarded trunk onto it, a bookmark set by hand) is never
    // rewritten either (#248).
    if new_parents.is_none()
        && revset::immutable_head(head, &plan.rewrite_of)
            .await
            .map_err(SnapshotError::Immutability)?
            .is_some()
    {
        new_parents = Some(vec![plan.rewrite_of.clone()]);
    }
    let started_on =
        (plan.recover.is_none() && new_parents.is_some()).then(|| plan.rewrite_of.clone());
    let (new_commit, divergent) = if let Some(parents) = &new_parents {
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
        tree_id: new_commit
            .tree_ids()
            .as_resolved()
            .map(jj_lib::object_id::ObjectId::hex),
        rewrote: new_parents.is_none().then(|| plan.rewrite_of.clone()),
        started_on,
        divergent,
        end_time: super::oplog::system_time(repo.operation().metadata().time.end),
        repo,
        write,
        commit: commit_start.elapsed(),
    })
}

/// `plan` recorded on `moved` instead of its `rewrite_of`: an ancestor's
/// snapshot rebased `rewrite_of` onto `moved`, in the same change, after
/// the scan (#91). The tree is the three-way merge of `moved`'s and the
/// plan's over `rewrite_of`'s, so the scan's edits and the rebase both
/// stay; where they touch the same lines, the commit has jj's conflict and
/// the check-out writes its markers. The plan is then carried
/// ([`SnapshotPlan::carried`]): its commit has files that are not on disk
/// yet, which [`PendingSnapshot::check_out_carried`] writes.
pub async fn onto_moved(
    head: &ReadonlyRepo,
    plan: &SnapshotPlan,
    moved: &CommitIdHex,
) -> Result<SnapshotPlan, SnapshotError> {
    let was = commit_by_hex(head, &plan.rewrite_of).await?;
    let now = commit_by_hex(head, moved).await?;
    let scanned = MergedTree::new(
        Arc::clone(head.store()),
        plan.tree_ids.clone(),
        plan.labels.clone(),
    );
    let merged = MergedTree::merge(Merge::from_vec(vec![
        (now.tree(), format!("{} (rebased)", now.conflict_label())),
        (was.tree(), format!("{} (scanned)", was.conflict_label())),
        (scanned, "working copy".to_owned()),
    ]))
    .await?;
    let (tree_ids, labels) = merged.tree_ids_and_labels();
    Ok(SnapshotPlan {
        workspace_name: plan.workspace_name.clone(),
        rewrite_of: moved.clone(),
        tree_ids: tree_ids.clone(),
        labels: labels.clone(),
        divergent: false,
        recover: None,
        carried: Some(plan.carried.clone().unwrap_or_else(|| ScannedTree {
            tree_ids: plan.tree_ids.clone(),
            labels: plan.labels.clone(),
        })),
        started_on: None,
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

        let mut pending = pending;
        let plan = pending.plan().clone();
        let meta = OpMeta::new(OpKind::Snapshot, description);
        let committed = commit_snapshot(&head, &plan, &meta).await?;
        let finish_start = Instant::now();
        let finished = match plan.carried {
            // The commit has the mover's changes, which are not on disk.
            Some(_) => pending
                .check_out_carried(&committed.repo, &committed.commit_id)
                .await
                .map(drop),
            None => Ok(()),
        };
        let finished = match finished {
            Ok(()) => pending.finish(&committed.op_id).await,
            Err(error) => Err(error),
        };
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
    /// - stale (another operation rewrote the working-copy commit) with
    ///   files that already match the rewritten commit, or with no edits
    ///   since the last update and a view's commit that came from the
    ///   commit it last snapshotted (`expected_commit`, or the one at the
    ///   working copy's operation), read from the operation log (#344, ADR
    ///   0087):
    ///   check it out on disk
    ///   ([`PendingSnapshot::adopted`]); nothing to commit;
    /// - stale with edits, and Reef moved the working-copy commit from
    ///   `expected_commit` to the view's ([`moved_by_reef`]): plan a rewrite of
    ///   the view's commit with the three-way merge of its tree and the
    ///   scanned one over `expected_commit`'s tree, or over `unfinished`'s
    ///   when the move includes that commit ([`SnapshotPlan::carried`]),
    ///   or, when the merge is the view's tree, check it out as above (ADR
    ///   0039, ADR 0084);
    /// - stale otherwise: plan a rewrite of `expected_commit` instead,
    ///   which makes the change divergent ([`SnapshotPlan::divergent`]) and
    ///   leaves the files on disk as they are;
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
        unfinished: Option<&Unfinished>,
        extra: &IgnoreSources<'_>,
        prefixes: Option<&[PathBuf]>,
    ) -> Result<PendingSnapshot<'_>, SnapshotError> {
        let max_new_file_size = max_new_file_size(self.workspace.settings())?;
        let base_ignores = self.ignores(extra)?;
        let root = self.workspace_root().to_owned();
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
        let full_reason = match (&prefixes, &narrow) {
            (None, _) => Some(FullScanReason::NotAsked),
            (Some(_), None) => Some(FullScanReason::NotNarrowable),
            (Some(_), Some(_)) if stale => Some(FullScanReason::Stale),
            (Some(_), Some(_)) if view_wc.is_none() => Some(FullScanReason::NoWorkingCopyCommit),
            (Some(_), Some(_)) => None,
        };
        let narrow = narrow.filter(|_| full_reason.is_none());
        let (new_tree, stats, walk, narrowed) = scan(&mut locked, narrow, &repo, &options).await?;
        load += narrowed
            .as_ref()
            .map_or(Duration::ZERO, |narrowed| narrowed.load);
        let untracked = untracked(stats.untracked_paths);
        let (tree_ids, labels) = new_tree.tree_ids_and_labels();
        let (mut tree_ids, mut labels) = (tree_ids.clone(), labels.clone());
        let mut carried = None;
        let mut stamps = Vec::new();

        let own_last = async || match expected_commit {
            Some(id) => commit_by_hex(&repo, id).await,
            None => last_own_commit(&repo, &wc_op, &name).await,
        };
        let mut adopted = false;
        let mut updated_files = 0;
        let (rewrite_of, changed, mut divergent, mut recover) = match &view_wc {
            None => {
                let last = own_last().await?;
                let parents = last.parent_ids().iter().map(CommitIdHex::of).collect();
                (CommitIdHex::of(last.id()), true, false, Some(parents))
            }
            Some(wc) if !stale => {
                let changed = new_tree.tree_ids_and_labels() != wc.tree().tree_ids_and_labels();
                (CommitIdHex::of(wc.id()), changed, false, None)
            }
            Some(wc) if new_tree.tree_ids_and_labels() == wc.tree().tree_ids_and_labels() => {
                // A working copy whose state was not saved after its own
                // snapshot was committed (a failed finish, a crash): the
                // files already are the commit's.
                updated_files = check_out(&mut locked, wc).await?;
                adopted = true;
                (CommitIdHex::of(wc.id()), false, false, None)
            }
            Some(wc) => {
                let last = own_last().await?;
                let merged = if new_tree.tree_ids_and_labels() == old_tree.tree_ids_and_labels() {
                    // With no edits, nothing of the workspace's own is lost
                    // by following a rewrite of the commit it last
                    // snapshotted. A commit made from an older one would
                    // take that snapshot's files off disk (#344): diverge.
                    came_from(&repo, &name, last.id()).await?.then(|| wc.tree())
                } else {
                    carried_tree(&repo, &name, (wc, &last), unfinished, &new_tree).await?
                };
                match merged {
                    Some(merged) if merged.tree_ids() == wc.tree_ids() => {
                        // The edits, if any, were already the commit's.
                        updated_files = check_out(&mut locked, wc).await?;
                        adopted = true;
                        (CommitIdHex::of(wc.id()), false, false, None)
                    }
                    Some(merged) => {
                        stamps = overwritten(&root, &new_tree, &merged).await?;
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
        // Never rewrite a commit that has landed (issue #248): record the
        // scan as a new change on it instead, as the jj CLI does when a
        // working-copy commit becomes immutable. Recovering from one builds
        // on it rather than beside it.
        let mut started_on = None;
        if changed
            && let Some(head) = revset::immutable_head(&repo, &rewrite_of)
                .await
                .map_err(SnapshotError::Immutability)?
        {
            if recover.is_some() {
                recover = Some(vec![rewrite_of.clone()]);
            } else {
                divergent = false;
                started_on = Some(StartedOn {
                    commit: rewrite_of.clone(),
                    head,
                });
            }
        }
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
                started_on,
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
            full_reason,
            checked_out: false,
            stamps,
            scanned: new_tree,
            root,
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
        let parent_tree = wc_commit.parent_tree(self.repo.index()).await?;
        let wc_tree = wc_commit.tree();
        let mut diff = parent_tree.diff(&wc_tree, &EverythingMatcher);
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

/// A carried snapshot's tree, when Reef moved the working copy from `last`,
/// the commit it was checked out from, to `wc`, the view's commit
/// ([`followed`]): the merge of `wc`'s tree and the scanned `disk` over
/// `last`'s tree, or over `unfinished`'s when the move includes its commit
/// (ADR 0039). `None` otherwise, and when `wc` is `last`.
async fn carried_tree(
    repo: &ReadonlyRepo,
    name: &WorkspaceName,
    (wc, last): (&Commit, &Commit),
    unfinished: Option<&Unfinished>,
    disk: &MergedTree,
) -> Result<Option<MergedTree>, SnapshotError> {
    if wc.id() == last.id() {
        return Ok(None);
    }
    let Some(followed) = followed(repo, name, last.id(), unfinished).await? else {
        return Ok(None);
    };
    let base = match followed.base {
        Some(base) => MergedTree::new(Arc::clone(repo.store()), base.tree_ids, base.labels),
        None => last.tree(),
    };
    let merged = MergedTree::merge(Merge::from_vec(vec![
        (wc.tree(), format!("{} (rewritten)", wc.conflict_label())),
        (base, format!("{} (snapshotted)", last.conflict_label())),
        (disk.clone(), "working copy".to_owned()),
    ]))
    .await?;
    Ok(Some(merged))
}

/// The most operations since a commit that [`followed`] judges.
const FOLLOW_LIMIT: usize = 1024;

/// What [`followed`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Followed {
    /// The tree to measure the workspace's edits from, when the move
    /// includes the [`Unfinished`] commit; `None` for `from`'s tree.
    base: Option<ScannedTree>,
}

/// Whether Reef moved workspace `name`'s working-copy commit from `from`
/// to the commit `repo`'s view has for it, so that edits made on `from`
/// belong on that commit (ADR 0039, ADR 0084). Read from the operation
/// log: every operation since `from` ([`Walk::moves`]) that moved the
/// commit must be one Reef wrote, and must have kept the workspace on its
/// change, or put it on a new empty change on it (a landing, ADR 0071);
/// and those moves must lead from `from` to the view's commit. The shape of
/// the log does not matter: operations that ran beside each other, and the
/// merges of their heads, are read like any others. These moves do not
/// count, and give `None`, ADR 0018's divergence:
///
/// - another jj client's operation, whichever commit a merge then kept;
/// - Reef's import of git refs that did not record the move as a rewrite
///   of the commit it left (#368);
/// - a merge of operation heads that left the workspace on a commit none
///   of the heads had (it rebased the commit over another client's
///   rewrite): no move leads to that commit;
/// - another workspace's snapshot of the same change (ADR 0018);
/// - this workspace's own snapshot, other than `unfinished`'s (a crash
///   between its commit and its row), or `unfinished`'s under a later undo;
/// - a move with more than [`FOLLOW_LIMIT`] operations since `from`, or
///   further back than the operations the store still has.
async fn followed(
    repo: &ReadonlyRepo,
    name: &WorkspaceName,
    from: &CommitId,
    unfinished: Option<&Unfinished>,
) -> Result<Option<Followed>, SnapshotError> {
    let walk = Walk {
        repo,
        name,
        from,
        unfinished,
        limit: Some(FOLLOW_LIMIT),
    };
    let Some(Since { ops, mut moves }) = walk.moves().await? else {
        return Ok(None);
    };
    // A merge's rebase is no move of Reef's: no carry leads through it.
    moves.retain(|step| !step.merged);
    let mut trail = Trail::default();
    for step in &moves {
        if !walk.reefs_move(step, &mut trail).await? {
            return Ok(None);
        }
    }
    let led = leads(&ops, &moves, from, repo.view().get_wc_commit_id(name));
    Ok(led.then_some(Followed { base: trail.base }))
}

/// Whether `moves` lead from `from` to `to` through `ops`, the operations
/// since `from` in [`Walk::moves`]' order: each move starts at `from` or at
/// a commit that a move in its own operation's history reached. A move on
/// another branch of the operation graph does not count, whatever the
/// order puts first: the operation never saw it (#357).
fn leads(ops: &[Operation], moves: &[Move], from: &CommitId, to: Option<&CommitId>) -> bool {
    let mut by_op: HashMap<&OperationId, Vec<&Move>> = HashMap::new();
    for step in moves {
        by_op.entry(step.op.id()).or_default().push(step);
    }
    // Each operation's child that the loop below reaches last: the first in
    // `ops`. It takes the operation's set, so a run of moves grows one set.
    let mut last = HashMap::new();
    for op in ops {
        for parent in op.parent_ids() {
            last.entry(parent).or_insert(op.id());
        }
    }
    // The commits reached by each operation's end, until its last child.
    // One before `from` reached `from` only.
    let before = Rc::new(HashSet::from([from]));
    let mut reached: HashMap<&OperationId, Rc<HashSet<&CommitId>>> = HashMap::new();
    for op in ops.iter().rev() {
        let mut here: Option<Rc<HashSet<&CommitId>>> = None;
        for parent in op.parent_ids() {
            let theirs = if last.get(parent) == Some(&op.id()) {
                reached.remove(parent)
            } else {
                reached.get(parent).cloned()
            };
            let theirs = theirs.unwrap_or_else(|| Rc::clone(&before));
            match &mut here {
                None => here = Some(theirs),
                Some(here) if !Rc::ptr_eq(here, &theirs) => {
                    Rc::make_mut(here).extend(theirs.iter().copied());
                }
                Some(_) => {}
            }
        }
        let mut here = here.unwrap_or_else(|| Rc::clone(&before));
        let steps = by_op.get(op.id()).into_iter().flatten();
        let now: Vec<&CommitId> = steps
            .filter(|step| here.contains(&step.was))
            .map(|step| &step.now)
            .collect();
        if !now.is_empty() {
            Rc::make_mut(&mut here).extend(now);
        }
        reached.insert(op.id(), here);
    }
    let head = ops.first().map_or(&before, |head| &reached[head.id()]);
    to.is_some_and(|to| head.contains(to))
}

/// Whether the commit `repo`'s view has for workspace `name` came from
/// `from`, the commit the workspace last snapshotted (#344): the operations
/// since `from` moved the workspace there from `from` ([`leads`]). A move
/// another jj client wrote, or Reef's import of git refs, counts only when
/// it rewrote the commit it started at ([`origins`]), as a describe, a
/// rebase or an import of a git-side amend does, and so does a merge of
/// operation heads that rebased the workspace's commit. Checking
/// the commit out then follows a decision someone made about `from`: an
/// operation that had `from` rewrote it (and may have dropped files, as
/// `jj restore` does), or Reef moved the workspace off it on purpose (an
/// undo, an abandon, `reef move`).
///
/// False for another client's `jj undo`, `jj op restore`, `jj op revert`
/// or `jj abandon` of `from` (#357), and for an import of git refs that
/// abandoned `from` because git no longer reaches it (#368): each puts the
/// workspace on a commit made from nothing of `from`'s, and checking it
/// out would take `from`'s files off disk.
///
/// False for a commit another jj client made from an older commit, having
/// loaded the head before `from` was written, that the merge of its
/// operation head with Reef's kept. jj keeps, for a workspace both heads
/// moved, the commit of the head stamped earlier, so a client whose clock
/// is behind wins. False too when the walk cannot tell: the store no
/// longer has an operation it needs, or the workspace was forgotten and
/// added again since `from` ([`Walk::moves`]).
async fn came_from(
    repo: &ReadonlyRepo,
    name: &WorkspaceName,
    from: &CommitId,
) -> Result<bool, SnapshotError> {
    // No limit: another client's move queues no follow, so a busy
    // repository can write any number of operations before this snapshot,
    // and `read_back` has read them all by the time a limit would count.
    let walk = Walk {
        repo,
        name,
        from,
        unfinished: None,
        limit: None,
    };
    let Some(Since { ops, mut moves }) = walk.moves().await? else {
        return Ok(false);
    };
    moves.retain(|step| reef_decided(&step.op) || origins(&step.op, &step.now).contains(&step.was));
    Ok(leads(
        &ops,
        &moves,
        from,
        repo.view().get_wc_commit_id(name),
    ))
}

/// Whether Reef wrote `op` ([`super::oplog::classify`]), other than an
/// import of git refs.
fn reef_decided(op: &Operation) -> bool {
    let class = super::oplog::classify(&super::oplog::entry(op));
    class
        .kind
        .is_some_and(|kind| !matches!(kind, OpKind::External | OpKind::ImportRefs))
}

/// The operations since a commit ([`Walk::moves`]), and the moves among
/// them.
struct Since {
    /// Each before every operation in its history: the head first.
    ops: Vec<Operation>,
    /// In the order of `ops`.
    moves: Vec<Move>,
}

/// An operation that moved the workspace's commit.
struct Move {
    op: Operation,
    was: CommitId,
    now: CommitId,
    /// A merge of operation heads, whose rebase made `now`, a commit none
    /// of the heads had, from `was`.
    merged: bool,
}

/// What the moves ordered before the one [`Walk::reefs_move`] judges
/// included ([`Walk::moves`]): the moves it is in the history of, and any
/// beside it.
#[derive(Clone, Debug, Default)]
struct Trail {
    /// The tree the workspace's [`Unfinished`] snapshot scanned, once that
    /// snapshot was among them.
    base: Option<ScannedTree>,
    /// An undo was among them.
    undone: bool,
}

/// [`followed`]'s question.
struct Walk<'a> {
    repo: &'a ReadonlyRepo,
    name: &'a WorkspaceName,
    from: &'a CommitId,
    unfinished: Option<&'a Unfinished>,
    /// The most operations since `from` it judges; `None` for no bound.
    limit: Option<usize>,
}

/// An operation the store no longer has reads as `None`.
fn kept<T>(read: Result<T, OpStoreError>) -> Result<Option<T>, OpStoreError> {
    match read {
        Ok(read) => Ok(Some(read)),
        Err(OpStoreError::ObjectNotFound { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The workspace's commit in the views read, by operation.
struct Commits<'a> {
    name: &'a WorkspaceName,
    read: HashMap<OperationId, Option<CommitId>>,
}

impl Commits<'_> {
    async fn at(&mut self, op: &Operation) -> Result<Option<CommitId>, SnapshotError> {
        if let Some(commit) = self.read.get(op.id()) {
            return Ok(commit.clone());
        }
        let commit = op.view().await?.get_wc_commit_id(self.name).cloned();
        self.read.insert(op.id().clone(), commit.clone());
        Ok(commit)
    }
}

/// The part of the operation log [`Walk::read_back`] has read: a graph,
/// from the head down.
#[derive(Default)]
struct Log {
    /// The operations loaded: those read, and those waiting.
    ops: HashMap<OperationId, Operation>,
    /// Read: their parents are loaded, or lost.
    read: HashSet<OperationId>,
    /// Loaded and not read yet.
    waiting: Vec<OperationId>,
    /// Parents of operations read that the store no longer has.
    lost: HashSet<OperationId>,
    /// Known to be before `from`: an operation whose view has `from`, or
    /// one in the history of an operation known to be before.
    before: HashSet<OperationId>,
}

impl Log {
    /// Loads `id` to wait for its read, unless the log has it.
    async fn load(&mut self, repo: &ReadonlyRepo, id: &OperationId) -> Result<(), OpStoreError> {
        if self.ops.contains_key(id) || self.lost.contains(id) {
            return Ok(());
        }
        match kept(repo.loader().load_operation(id).await)? {
            Some(op) => {
                self.ops.insert(id.clone(), op);
                self.waiting.push(id.clone());
            }
            None => {
                self.lost.insert(id.clone());
            }
        }
        Ok(())
    }

    /// Records that `op`, which was read, is before `from`: so is its
    /// history, as far as it was read.
    fn is_before(&mut self, op: &OperationId) {
        self.before.insert(op.clone());
        let mut found = vec![op.clone()];
        while let Some(id) = found.pop() {
            for parent in self.ops[&id].parent_ids() {
                if self.before.insert(parent.clone()) && self.read.contains(parent) {
                    found.push(parent.clone());
                }
            }
        }
    }

    /// Where in `waiting` the operation with the latest end time is, of
    /// those known to be before `from`, or of the others.
    fn latest(&self, before: bool) -> Option<usize> {
        let end = |at: &usize| {
            let id = &self.waiting[*at];
            (self.ops[id].metadata().time.end, id)
        };
        (0..self.waiting.len())
            .filter(|at| self.before.contains(&self.waiting[*at]) == before)
            .max_by_key(end)
    }

    /// The operations read that are not known to be before `from`.
    fn since(&self) -> impl Iterator<Item = &OperationId> {
        self.read.iter().filter(|id| !self.before.contains(*id))
    }

    /// Whether no operation of [`Log::since`] can turn out to be before
    /// `from`. One could only be in the history of an operation not read
    /// yet. So every such operation, or one the store lost, must be known
    /// to be before `from`, and must be in the history of each operation
    /// of `since`: nothing is in its own history. That is checked for the
    /// lowest of `since`, those with no parent in it; the history of the
    /// others includes theirs.
    fn settled(&self) -> bool {
        let unread = || self.waiting.iter().chain(&self.lost);
        let before = |id: &OperationId| self.before.contains(id);
        unread().all(before)
            && self
                .since()
                .filter(|id| self.ops[*id].parent_ids().iter().all(before))
                .all(|lowest| {
                    let history = self.history(lowest);
                    unread().all(|id| history.contains(id))
                })
    }

    /// The history of `of`, as far as it was read.
    fn history<'a>(&'a self, of: &'a OperationId) -> HashSet<&'a OperationId> {
        let mut history = HashSet::new();
        let mut found = vec![of];
        while let Some(id) = found.pop() {
            if !self.read.contains(id) {
                continue;
            }
            for parent in self.ops[id].parent_ids() {
                if history.insert(parent) {
                    found.push(parent);
                }
            }
        }
        history
    }

    /// The operations of [`Log::since`], each before every operation in
    /// its history, in an order the graph alone decides. `head` is the one
    /// that is in no other's history: each of the others is a parent of
    /// one of them, since a parent of an operation before `from` is before
    /// it too. `None` if one is left out all the same.
    fn since_in_order(&self, head: &OperationId) -> Option<Vec<&Operation>> {
        let since: HashSet<&OperationId> = self.since().collect();
        let parents = |id: &OperationId| {
            let parents = self.ops[id].parent_ids().iter();
            parents.filter(|parent| since.contains(parent))
        };
        let mut children: HashMap<&OperationId, usize> = HashMap::new();
        for parent in since.iter().flat_map(|id| parents(id)) {
            *children.entry(parent).or_default() += 1;
        }
        let mut ready: Vec<&OperationId> = since.get(head).copied().into_iter().collect();
        let mut ordered = Vec::new();
        while let Some(id) = ready.pop() {
            ordered.push(&self.ops[id]);
            for parent in parents(id) {
                let left = children.entry(parent).or_default();
                *left -= 1;
                if *left == 0 {
                    ready.push(parent);
                }
            }
        }
        (ordered.len() == since.len()).then_some(ordered)
    }
}

/// The moves of `op`, a merge of operation heads. Keeping one head's commit
/// for the workspace is no move: that head's own operations moved it, and
/// [`leads`] checks that they lead to the commit kept. A commit none of the
/// heads had is one the merge's rebase made, from the predecessors jj
/// recorded for it in `op`. A merge of three heads or more rebases after
/// each head it merges in, so a predecessor can be a commit `op` wrote
/// itself and rebased again: the move is from that commit's predecessors.
async fn merged(
    op: &Operation,
    log: &Log,
    commits: &mut Commits<'_>,
) -> Result<Vec<Move>, SnapshotError> {
    let Some(now) = commits.at(op).await? else {
        return Ok(Vec::new());
    };
    for parent in op.parent_ids() {
        let Some(parent) = log.ops.get(parent) else {
            return Ok(Vec::new());
        };
        if commits.at(parent).await?.as_ref() == Some(&now) {
            return Ok(Vec::new());
        }
    }
    let moves = origins(op, &now).into_iter().map(|was| Move {
        op: op.clone(),
        was,
        now: now.clone(),
        merged: true,
    });
    Ok(moves.collect())
}

/// The commits `op` made `now` from: the predecessors jj recorded for it in
/// `op`, and for a commit `op` wrote and rewrote again into `now`, that
/// commit's. Empty when `op` did not write `now` as a rewrite: an undo, an
/// operation restore, or a new commit.
fn origins(op: &Operation, now: &CommitId) -> HashSet<CommitId> {
    let mut found = op.predecessors_for_commit(now).unwrap_or_default().to_vec();
    let (mut seen, mut origins) = (HashSet::new(), HashSet::new());
    while let Some(was) = found.pop() {
        if !seen.insert(was.clone()) {
            continue;
        }
        match op.predecessors_for_commit(&was) {
            Some(earlier) => found.extend_from_slice(earlier),
            None => {
                origins.insert(was);
            }
        }
    }
    origins
}

impl Walk<'_> {
    /// The operations since `from`, each before every operation in its
    /// history, and the moves among them, in the same order: the operations
    /// with one parent whose view has another commit for the workspace, and
    /// the merges whose rebase made the workspace's commit ([`merged`]). An
    /// operation is since `from` when it is
    /// in the head's history and in the history of no operation there
    /// whose view has `from` for the workspace. So an operation of another
    /// client that loaded the head before `from` was written, and committed
    /// later, is one. That is a property of the graph: what the clocks
    /// said when the operations were written does not change it. `None`
    /// when the workspace was added or forgotten since, and when the
    /// operations since `from` cannot be judged: there are more than
    /// `limit` of them, the log could not be read back far enough
    /// to know them ([`Walk::read_back`]), or they could not all be put in
    /// order ([`Log::since_in_order`]).
    async fn moves(&self) -> Result<Option<Since>, SnapshotError> {
        let mut commits = Commits {
            name: self.name,
            read: HashMap::new(),
        };
        let Some(log) = self.read_back(&mut commits).await? else {
            return Ok(None);
        };
        if self.limit.is_some_and(|limit| log.since().count() > limit) {
            return Ok(None);
        }
        let Some(since) = log.since_in_order(self.repo.op_id()) else {
            return Ok(None);
        };
        let mut moves = Vec::new();
        for op in &since {
            let [parent] = op.parent_ids() else {
                moves.extend(merged(op, &log, &mut commits).await?);
                continue;
            };
            let Some(parent) = log.ops.get(parent) else {
                return Ok(None);
            };
            match (commits.at(parent).await?, commits.at(op).await?) {
                (was, now) if was == now => {}
                (Some(was), Some(now)) => moves.push(Move {
                    op: (*op).clone(),
                    was,
                    now,
                    merged: false,
                }),
                _ => return Ok(None),
            }
        }
        let ops = since.into_iter().cloned().collect();
        Ok(Some(Since { ops, moves }))
    }

    /// Reads the operation log back from the head until the operations
    /// since `from` are known ([`Log::since`], once [`Log::settled`]): the
    /// operations before `from` are read as far as that takes, without
    /// their views. `None` when it cannot be known: the store no longer
    /// has an operation it takes.
    ///
    /// The order of the reads changes what they cost, and not what they
    /// find, so nothing bounds them: a bound would make the answer depend
    /// on the order. The operation with the latest end time is read next, as jj
    /// orders the log (`jj_lib::op_walk`): with right clocks, that reads an
    /// operation before `from` after the one that shows it is. A clock
    /// that was wrong can keep an operation since `from` waiting behind
    /// every operation before `from`, so after 1, 2, 4 and so on of those
    /// in a row, the latest of the others is read. It can also put
    /// operations before `from` ahead of the one that shows they are, and
    /// each of those is then read with its view.
    async fn read_back(&self, commits: &mut Commits<'_>) -> Result<Option<Log>, SnapshotError> {
        let mut log = Log::default();
        let head = self.repo.operation();
        log.ops.insert(head.id().clone(), head.clone());
        log.waiting.push(head.id().clone());
        let (mut patience, mut left) = (0_usize, 0_usize);
        let settled = loop {
            let at = match (log.latest(false), log.latest(true)) {
                (None, None) => break log.settled(),
                (Some(since), None) => since,
                (since, Some(before)) => {
                    let later = |since: &usize| {
                        let end = |at: usize| log.ops[&log.waiting[at]].metadata().time.end;
                        end(*since) > end(before)
                    };
                    if let Some(since) = since.filter(later) {
                        since
                    } else if left > 0 {
                        left -= 1;
                        before
                    } else {
                        patience = (2 * patience).max(1);
                        left = patience;
                        match since {
                            Some(since) => since,
                            None if log.settled() => break true,
                            None => {
                                left -= 1;
                                before
                            }
                        }
                    }
                }
            };
            let id = log.waiting.swap_remove(at);
            let op = log.ops[&id].clone();
            for parent in op.parent_ids() {
                log.load(self.repo, parent).await?;
            }
            log.read.insert(id.clone());
            if log.before.contains(&id) || commits.at(&op).await?.as_ref() == Some(self.from) {
                log.is_before(&id);
            }
        };
        Ok(settled.then_some(log))
    }

    /// Whether `step` is a move of Reef's that kept the workspace's edits
    /// in their change. An import of git refs counts only when it recorded
    /// the move as a rewrite ([`origins`]), as in [`came_from`] (#368).
    async fn reefs_move(&self, step: &Move, trail: &mut Trail) -> Result<bool, SnapshotError> {
        let Move { op, was, now, .. } = step;
        let class = super::oplog::classify(&super::oplog::entry(op));
        let mover = class
            .workspace_id
            .map(|id| format!("{}{id}", super::workspace::REEF_WORKSPACE_PREFIX));
        let reefs = match class.kind {
            None | Some(OpKind::External) => false,
            Some(OpKind::Snapshot) if mover.as_deref() == Some(self.name.as_str()) => {
                let unfinished = self.unfinished.filter(|own| {
                    trail.base.is_none() && !trail.undone && own.commit.as_str() == now.hex()
                });
                if let Some(own) = unfinished {
                    trail.base = Some(own.disk.clone());
                }
                trail.base.is_some()
            }
            Some(OpKind::Snapshot) => match mover {
                Some(mover) => !same_change(self.repo, op, &mover, now).await?,
                None => false,
            },
            Some(OpKind::ImportRefs) => origins(op, now).contains(was),
            Some(kind) => {
                trail.undone |= kind == OpKind::Undo;
                true
            }
        };
        Ok(reefs && kept_change(self.repo, was, now).await?)
    }
}

/// Whether Reef moved workspace `workspace`'s working-copy commit from
/// `from` to the commit `repo`'s view has for it, read from the operation
/// log (ADR 0084): every operation since that moved it is one Reef wrote,
/// an import of git refs among them only where it recorded a rewrite, and
/// none is another workspace's snapshot of the same change (ADR 0018).
/// A snapshot planned on `from` is then recorded on the view's commit
/// ([`onto_moved`]); [`Repo::begin_snapshot`] asks the same of the commit
/// its working copy was checked out from.
pub async fn moved_by_reef(
    repo: &ReadonlyRepo,
    workspace: &str,
    from: &CommitIdHex,
) -> Result<bool, SnapshotError> {
    let from = commit_id(repo, from)?;
    let followed = followed(repo, WorkspaceName::new(workspace), &from, None).await?;
    Ok(followed.is_some())
}

/// Whether workspace `mover`'s working-copy commit after `op` is in the
/// change of `commit`: `op`, the mover's snapshot, rewrote the commit both
/// were on (ADR 0018). True when the mover has none: not known to be
/// another change's.
async fn same_change(
    repo: &ReadonlyRepo,
    op: &Operation,
    mover: &str,
    commit: &CommitId,
) -> Result<bool, SnapshotError> {
    let view = op.view().await?;
    let Some(movers) = view.get_wc_commit_id(WorkspaceName::new(mover)) else {
        return Ok(true);
    };
    let movers = repo.store().get_commit_async(movers).await?;
    let commit = repo.store().get_commit_async(commit).await?;
    Ok(movers.change_id() == commit.change_id())
}

/// Whether a move from `was` to `now` kept the workspace's edits in their
/// change: `now` is in `was`'s change, or is a new empty change on it (a
/// landing that rebased the change, ADR 0071), where edits made meanwhile
/// go, never into the landed commit.
async fn kept_change(
    repo: &ReadonlyRepo,
    was: &CommitId,
    now: &CommitId,
) -> Result<bool, SnapshotError> {
    let was = repo.store().get_commit_async(was).await?;
    let now = repo.store().get_commit_async(now).await?;
    if was.change_id() == now.change_id() {
        return Ok(true);
    }
    let [parent] = now.parent_ids() else {
        return Ok(false);
    };
    let parent = repo.store().get_commit_async(parent).await?;
    Ok(parent.change_id() == was.change_id() && parent.tree_ids() == now.tree_ids())
}

/// The files on disk under `root` that checking `merged` out over the
/// scanned `disk` tree would overwrite or remove, with their stamps now.
async fn overwritten(
    root: &Path,
    disk: &MergedTree,
    merged: &MergedTree,
) -> Result<Vec<(PathBuf, Option<Stamp>)>, BackendError> {
    let mut diff = disk.diff(merged, &EverythingMatcher);
    let mut stamps = Vec::new();
    while let Some(entry) = diff.next().await {
        // A path absent from the scan is not overwritten: jj's check-out
        // skips a file it does not track.
        if entry.values?.before.is_present() {
            let path = entry.path.to_fs_path_unchecked(root);
            let stamp = Stamp::of(&path);
            stamps.push((path, stamp));
        }
    }
    Ok(stamps)
}

/// Whether `disk` holds `value`, `path`'s value in the scanned tree: a
/// file with the same bytes, or a symlink to the same target. Anything else
/// (a conflict, a directory, a submodule) reads as changed, which only
/// costs a merge at the next snapshot.
async fn holds(
    store: &Arc<jj_lib::store::Store>,
    path: &jj_lib::repo_path::RepoPath,
    value: &jj_lib::backend::MergedTreeValue,
    disk: &Path,
) -> Result<bool, BackendError> {
    use futures::AsyncReadExt as _;
    use jj_lib::backend::TreeValue;
    let Ok(metadata) = std::fs::symlink_metadata(disk) else {
        return Ok(false);
    };
    match value.as_resolved() {
        Some(Some(TreeValue::File { id, .. })) if metadata.is_file() => {
            let Ok(on_disk) = std::fs::read(disk) else {
                return Ok(false);
            };
            let mut reader = store.read_file(path, id).await?;
            let mut stored = Vec::with_capacity(on_disk.len());
            reader
                .read_to_end(&mut stored)
                .await
                .map_err(|error| BackendError::ReadFile {
                    path: path.to_owned(),
                    id: id.clone(),
                    source: error.into(),
                })?;
            Ok(stored == on_disk)
        }
        Some(Some(TreeValue::Symlink(id))) if metadata.is_symlink() => {
            let Ok(target) = std::fs::read_link(disk) else {
                return Ok(false);
            };
            let stored = store.read_symlink(path, id).await?;
            Ok(target.to_str() == Some(stored.as_str()))
        }
        _ => Ok(false),
    }
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
