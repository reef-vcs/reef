//! Snapshotting the working copy into the working-copy commit.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use jj_lib::backend::BackendError;
use jj_lib::gitignore::{GitIgnoreError, GitIgnoreFile};
use jj_lib::matchers::{EverythingMatcher, NothingMatcher};
use jj_lib::repo::{Repo as _, RewriteRootCommit};
use jj_lib::repo_path::RepoPath;
use jj_lib::transaction::TransactionCommitError;
use jj_lib::working_copy::{SnapshotOptions, UntrackedReason, WorkingCopyStateError};

use super::repo::{CommitIdHex, OperationIdHex, ReadError, Repo};
use super::settings::{SettingsError, max_new_file_size};

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

/// Failure to snapshot the working copy.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SnapshotError {
    /// The working copy was last updated at a different operation than the
    /// one this [`Repo`] is loaded at. Nothing was written. Reload or update
    /// the working copy first; R1.13 decides which.
    #[error("working copy is at operation {wc_op} but the repo is loaded at {repo_op}")]
    Stale {
        /// The operation recorded in the working-copy state.
        wc_op: OperationIdHex,
        /// The operation this [`Repo`] is loaded at.
        repo_op: OperationIdHex,
    },
    /// Settings needed for the snapshot were invalid.
    #[error("invalid snapshot settings")]
    Settings(#[from] SettingsError),
    /// An ignore file could not be read.
    #[error("failed to read ignore files")]
    Ignores(#[from] GitIgnoreError),
    /// The working-copy state could not be locked, read, or saved.
    #[error("failed to lock or save the working-copy state")]
    WorkingCopyState(#[from] WorkingCopyStateError),
    /// Walking the working copy failed.
    #[error("failed to snapshot the working copy")]
    Snapshot(#[from] jj_lib::working_copy::SnapshotError),
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

impl Repo {
    /// Snapshots the working copy into this workspace's working-copy commit
    /// and records an operation marked as a snapshot, following jj-cli's
    /// `snapshot_working_copy`. `description` becomes the operation's
    /// description.
    ///
    /// New files above `snapshot.max-new-file-size` are left untracked and
    /// reported in [`SnapshotOutcome::untracked`]. Files already tracked are
    /// always snapshotted. A stale working copy is an error, not recovered.
    pub async fn snapshot(&mut self, description: &str) -> Result<SnapshotOutcome, SnapshotError> {
        let max_new_file_size = max_new_file_size(self.workspace.settings())?;
        let base_ignores = self.base_ignores()?;
        let repo = self.repo.clone();
        let workspace_name = self.workspace.workspace_name().to_owned();

        let start = Instant::now();
        let mut locked_ws = self.workspace.start_working_copy_mutation().await?;
        let wc_op = locked_ws.locked_wc().old_operation_id().clone();
        if &wc_op != repo.op_id() {
            return Err(SnapshotError::Stale {
                wc_op: OperationIdHex::of(&wc_op),
                repo_op: OperationIdHex::of(repo.op_id()),
            });
        }
        let wc_commit_id = repo
            .view()
            .get_wc_commit_id(&workspace_name)
            .ok_or_else(|| ReadError::NoWorkingCopyCommit(workspace_name.as_str().to_owned()))?
            .clone();
        let wc_commit = repo
            .store()
            .get_commit_async(&wc_commit_id)
            .await
            .map_err(ReadError::from)?;

        let options = SnapshotOptions {
            base_ignores,
            progress: None,
            start_tracking_matcher: &EverythingMatcher,
            force_tracking_matcher: &NothingMatcher,
            max_new_file_size,
        };
        let (new_tree, snapshot_stats) = locked_ws.locked_wc().snapshot(&options).await?;
        let scan = start.elapsed();
        let untracked = snapshot_stats
            .untracked_paths
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
            .collect();

        if new_tree.tree_ids_and_labels() == wc_commit.tree().tree_ids_and_labels() {
            // Nothing to record, but save the working-copy state anyway, as
            // jj-cli does, so refreshed file metadata is not rescanned.
            let commit_start = Instant::now();
            locked_ws.finish(repo.op_id().clone()).await?;
            return Ok(SnapshotOutcome {
                changed: false,
                op_id: OperationIdHex::of(repo.op_id()),
                wc_commit: CommitIdHex::of(&wc_commit_id),
                untracked,
                stats: SnapshotTiming {
                    scan,
                    write: Duration::ZERO,
                    commit: commit_start.elapsed(),
                },
            });
        }

        let write_start = Instant::now();
        let mut tx = repo.start_transaction();
        tx.set_is_snapshot(true);
        tx.set_workspace_name(&workspace_name);
        let mut_repo = tx.repo_mut();
        let new_wc_commit = mut_repo
            .rewrite_commit(&wc_commit)
            .set_tree(new_tree)
            .write()
            .await?;
        mut_repo.set_wc_commit(workspace_name, new_wc_commit.id().clone())?;
        mut_repo.rebase_descendants().await?;
        let write = write_start.elapsed();

        let commit_start = Instant::now();
        let new_repo = tx.commit(description).await?;
        // The operation is published once committed. Adopt it before saving the
        // working-copy state, so that if saving fails the next snapshot reports
        // `Stale` instead of building a divergent operation on the old one.
        let new_op_id = new_repo.op_id().clone();
        self.repo = new_repo;
        locked_ws.finish(new_op_id).await?;
        let commit = commit_start.elapsed();

        let outcome = SnapshotOutcome {
            changed: true,
            op_id: OperationIdHex::of(self.repo.op_id()),
            wc_commit: CommitIdHex::of(new_wc_commit.id()),
            untracked,
            stats: SnapshotTiming {
                scan,
                write,
                commit,
            },
        };
        Ok(outcome)
    }

    /// Paths that differ between the working-copy commit and its parent, in
    /// path order. Reflects the last snapshot, not the files on disk.
    pub async fn changed_paths_in_wc(&self) -> Result<Vec<ChangedPath>, DiffError> {
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

    /// Ignore rules that apply before the repository's own `.gitignore`
    /// files, which the working copy reads itself: git's `core.excludesFile`
    /// (or `$XDG_CONFIG_HOME/git/ignore`) and the git repo's `info/exclude`.
    /// Mirrors jj-cli's `base_ignores`.
    fn base_ignores(&self) -> Result<Arc<GitIgnoreFile>, GitIgnoreError> {
        let mut ignores = GitIgnoreFile::empty();
        let Ok(git_backend) = jj_lib::git::get_git_backend(self.repo.store()) else {
            return Ok(ignores);
        };
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let git_repo = git_backend.git_repo();
        let excludes_file = git_repo
            .config_snapshot()
            .string("core.excludesFile")
            .and_then(|value| {
                let value = std::str::from_utf8(&value).ok()?;
                let path = jj_lib::file_util::expand_home_path(value, home.as_deref());
                // A relative path is read from the work tree, as git does.
                Some(self.workspace_root().join(path))
            })
            .or_else(|| xdg_config_home(home.as_deref()).map(|dir| dir.join("git").join("ignore")));
        if let Some(path) = excludes_file {
            ignores = ignores.chain_with_file(RepoPath::root(), path)?;
        }
        ignores.chain_with_file(
            RepoPath::root(),
            git_backend.git_repo_path().join("info").join("exclude"),
        )
    }
}

fn xdg_config_home(home: Option<&std::path::Path>) -> Option<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => home.map(|home| home.join(".config")),
    }
}
