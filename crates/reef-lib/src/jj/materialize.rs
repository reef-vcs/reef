//! Writing a commit's tree into a directory through jj's own working copy,
//! for the daemon's base cache (R1.02, ADR 0015), and reading back the
//! `tree_state` jj records for it.
//!
//! A base is materialized once per tree per machine with a throwaway
//! [`LocalWorkingCopy`]: initialized on the empty tree, checked out to the
//! commit, finished. jj writes every file and records each one's mtime and
//! size in `tree_state`, which then describes the directory exactly and can
//! seed the working copy of any workspace holding byte-identical files with
//! the same mtimes and sizes (R1.05). Conflicted trees are written with jj's
//! conflict markers, as jj writes them into any working copy.
//!
//! A base can also be built from a copy of another one
//! ([`Repo::materialize_tree_from`], R1.08): a clone of its directory and
//! state, on which jj's check-out writes only the files that differ.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jj_lib::commit::Commit;
use jj_lib::local_working_copy::LocalWorkingCopy;
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::working_copy::{CheckoutError, CheckoutStats, WorkingCopy as _, WorkingCopyStateError};
use prost::Message as _;

use super::repo::{CommitIdHex, Repo};
use super::workspace::CommitLookupError;
use crate::error::{Classify, ErrorKind, classify_by_source};

/// The name recorded in a materialization's `checkout` file. The working
/// copy is never added to the view, so the name is never seen by jj; the
/// `checkout` file is not copied into workspaces either.
const MATERIALIZE_WORKSPACE_NAME: &str = "reef-base";

/// Separator between the tree ids of a conflicted tree in [`tree_key`].
pub const TREE_KEY_SEPARATOR: char = '+';

/// The file jj's local working copy keeps its tree state in, inside the
/// state directory.
pub const TREE_STATE_FILE: &str = "tree_state";

/// What jj's check-out reported, as plain numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CheckoutStatsSummary {
    /// Files that existed before and were rewritten.
    pub updated_files: u32,
    /// Files written that did not exist before.
    pub added_files: u32,
    /// Files removed.
    pub removed_files: u32,
    /// Files jj left alone because something unexpected was in the way.
    /// Always zero when materializing into an empty directory.
    pub skipped_files: u32,
}

impl From<CheckoutStats> for CheckoutStatsSummary {
    fn from(stats: CheckoutStats) -> Self {
        Self {
            updated_files: stats.updated_files,
            added_files: stats.added_files,
            removed_files: stats.removed_files,
            skipped_files: stats.skipped_files,
        }
    }
}

/// Result of [`Repo::materialize_tree`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterializeStats {
    /// Files and symlinks written: the entries of the resulting `tree_state`.
    pub files: u64,
    /// The sum of their sizes as jj recorded them (a symlink's size is the
    /// length of its target).
    pub bytes: u64,
    /// Wall time of the whole materialization.
    pub elapsed: Duration,
    /// jj's own counts.
    pub checkout: CheckoutStatsSummary,
}

/// Failure to materialize a tree.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MaterializeError {
    /// The target or state directory exists and is not empty. Nothing was
    /// written.
    #[error("{} is not empty", .0.display())]
    NotEmpty(PathBuf),
    /// A directory could not be created or read.
    #[error("{op} {}", path.display())]
    Io {
        /// What was being done, such as `create directory`.
        op: &'static str,
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The commit could not be read.
    #[error(transparent)]
    Lookup(#[from] CommitLookupError),
    /// The throwaway working copy could not be initialized, locked, or
    /// saved.
    #[error("failed to initialize or save the working-copy state")]
    Init(#[from] WorkingCopyStateError),
    /// Writing the tree failed.
    #[error("failed to check out the tree")]
    CheckOut(#[from] CheckoutError),
    /// The `tree_state` jj wrote could not be read back.
    #[error(transparent)]
    TreeState(#[from] TreeStateReadError),
}

impl Classify for MaterializeError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NotEmpty(_) => ErrorKind::AlreadyExists,
            Self::Io { source, .. } => classify_by_source(source),
            Self::Lookup(error) => error.kind(),
            Self::Init(error) => classify_by_source(error),
            Self::CheckOut(error) => classify_by_source(error),
            Self::TreeState(error) => error.kind(),
        }
    }
}

/// One file's entry in a `tree_state`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeStateEntry {
    /// Repository-relative path with `/` separators.
    pub path: String,
    /// Modification time jj recorded, in milliseconds since the Unix epoch,
    /// truncated as jj truncates it.
    pub mtime_ms: i64,
    /// Size jj recorded: the file's length, or a symlink's target length.
    pub size: u64,
}

/// Failure to read a `tree_state` file.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TreeStateReadError {
    /// The file could not be read.
    #[error("read {}", path.display())]
    Io {
        /// The `tree_state` file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The file is not a protobuf `TreeState`.
    #[error("decode {}", path.display())]
    Decode {
        /// The `tree_state` file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: prost::DecodeError,
    },
}

impl Classify for TreeStateReadError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Io { source, .. } => classify_by_source(source),
            // Reef or jj wrote the file; a corrupt one is not the caller's doing.
            Self::Decode { .. } => ErrorKind::Internal,
        }
    }
}

/// The base cache key of `commit`'s tree: the hex id of a resolved tree, or
/// the hex ids of a conflicted tree's terms joined with
/// [`TREE_KEY_SEPARATOR`] in jj's `Merge` order (add, remove, add, ...).
pub fn tree_key(commit: &Commit) -> String {
    let ids: Vec<String> = commit.tree().tree_ids().iter().map(ObjectId::hex).collect();
    ids.join(&TREE_KEY_SEPARATOR.to_string())
}

/// Whether `key` has the shape [`tree_key`] produces: one or more non-empty
/// lowercase hex ids joined with [`TREE_KEY_SEPARATOR`]. A key that passes is
/// safe to use as a single path component.
pub fn is_tree_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .split(TREE_KEY_SEPARATOR)
            .all(|id| !id.is_empty() && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Reads `state_dir/tree_state`, as jj's local working copy writes it, and
/// returns every file's path, mtime, and size, in the file's order (sorted
/// by path when jj wrote it). Git submodules are left out: jj records them
/// with placeholder mtime and size, and writes nothing for them on disk.
///
/// Decodes jj-lib's own protobuf message
/// (`jj_lib::protos::local_working_copy::TreeState`); the rest of the state
/// (tree ids, sparse patterns, the watchman clock) is ignored.
pub fn tree_state_entries(state_dir: &Path) -> Result<Vec<TreeStateEntry>, TreeStateReadError> {
    let path = state_dir.join(TREE_STATE_FILE);
    let bytes = std::fs::read(&path).map_err(|source| TreeStateReadError::Io {
        path: path.clone(),
        source,
    })?;
    let proto = jj_lib::protos::local_working_copy::TreeState::decode(bytes.as_slice())
        .map_err(|source| TreeStateReadError::Decode { path, source })?;
    Ok(proto
        .file_states
        .into_iter()
        .filter(|entry| {
            entry.state.as_ref().is_none_or(|state| {
                state.file_type() != jj_lib::protos::local_working_copy::FileType::GitSubmodule
            })
        })
        .map(|entry| {
            let state = entry.state.unwrap_or_default();
            TreeStateEntry {
                path: entry.path,
                mtime_ms: state.mtime_millis_since_epoch,
                size: state.size,
            }
        })
        .collect())
}

impl Repo {
    /// Writes `commit`'s tree into `dir` using a throwaway working copy
    /// whose state goes to `state_dir`. Both are created if missing (with
    /// their parents) and must be empty if present. Afterwards
    /// `state_dir/tree_state` describes `dir` exactly: every file with the
    /// mtime and size it has on disk. `state_dir` also holds a `checkout`
    /// file naming this repository's current operation, which workspaces do
    /// not use.
    ///
    /// On failure, whatever was written stays; the caller removes both
    /// directories (the base cache writes into temporary ones).
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), path = %dir.display()))]
    pub async fn materialize_tree(
        &self,
        commit: &CommitIdHex,
        dir: &Path,
        state_dir: &Path,
    ) -> Result<MaterializeStats, MaterializeError> {
        let stats = self
            .materialize_tree_unlogged(commit, dir, state_dir)
            .await
            .inspect_err(|error| crate::log_failure!(error))?;
        tracing::info!(
            file_count = stats.files,
            total_bytes = stats.bytes,
            elapsed_ms = u64::try_from(stats.elapsed.as_millis()).unwrap_or(u64::MAX),
            "tree materialized"
        );
        Ok(stats)
    }

    /// Like [`Repo::materialize_tree`], but `dir` and `state_dir` already
    /// hold another tree and its working-copy state, as jj wrote them (a
    /// clone of another base and of its state directory, files with their
    /// mtimes and sizes). The throwaway working copy is loaded on them
    /// instead of initialized empty, so the check-out writes, replaces, and
    /// removes only the files that differ; the returned
    /// [`MaterializeStats::checkout`] counts those. Afterwards
    /// `state_dir/tree_state` describes `dir` exactly, as after
    /// `materialize_tree`.
    ///
    /// On failure the two directories are in an unknown state; the caller
    /// removes them.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), path = %dir.display()))]
    pub async fn materialize_tree_from(
        &self,
        commit: &CommitIdHex,
        dir: &Path,
        state_dir: &Path,
    ) -> Result<MaterializeStats, MaterializeError> {
        let stats = self
            .materialize_tree_from_unlogged(commit, dir, state_dir)
            .await
            .inspect_err(|error| crate::log_failure!(error))?;
        tracing::info!(
            file_count = stats.files,
            changed_count = stats.checkout.updated_files
                + stats.checkout.added_files
                + stats.checkout.removed_files,
            elapsed_ms = u64::try_from(stats.elapsed.as_millis()).unwrap_or(u64::MAX),
            "tree materialized from another"
        );
        Ok(stats)
    }

    async fn materialize_tree_unlogged(
        &self,
        commit: &CommitIdHex,
        dir: &Path,
        state_dir: &Path,
    ) -> Result<MaterializeStats, MaterializeError> {
        let begun = Instant::now();
        let commit = self.commit_by_id(commit).await?;
        create_empty_dir(dir)?;
        create_empty_dir(state_dir)?;
        let dir = canonical(dir)?;
        let working_copy = LocalWorkingCopy::init(
            self.repo.store().clone(),
            dir,
            state_dir.to_owned(),
            self.repo.op_id().clone(),
            WorkspaceNameBuf::from(MATERIALIZE_WORKSPACE_NAME),
            self.workspace.settings(),
        )?;
        self.check_out_and_count(working_copy, &commit, state_dir, begun)
            .await
    }

    async fn materialize_tree_from_unlogged(
        &self,
        commit: &CommitIdHex,
        dir: &Path,
        state_dir: &Path,
    ) -> Result<MaterializeStats, MaterializeError> {
        let begun = Instant::now();
        let commit = self.commit_by_id(commit).await?;
        let dir = canonical(dir)?;
        let working_copy = LocalWorkingCopy::load(
            self.repo.store().clone(),
            dir,
            state_dir.to_owned(),
            self.workspace.settings(),
        )?;
        self.check_out_and_count(working_copy, &commit, state_dir, begun)
            .await
    }

    /// Checks `working_copy` out to `commit`, finishes it at the current
    /// operation, and counts what its `tree_state` then records.
    async fn check_out_and_count(
        &self,
        working_copy: LocalWorkingCopy,
        commit: &Commit,
        state_dir: &Path,
        begun: Instant,
    ) -> Result<MaterializeStats, MaterializeError> {
        let op_id = self.repo.op_id().clone();
        let mut locked = working_copy.start_mutation().await?;
        let checkout = locked.check_out(commit).await?;
        locked.finish(op_id).await?;

        let entries = tree_state_entries(state_dir)?;
        Ok(MaterializeStats {
            files: entries.len() as u64,
            bytes: entries.iter().map(|entry| entry.size).sum(),
            elapsed: begun.elapsed(),
            checkout: checkout.into(),
        })
    }

    /// The base cache key ([`tree_key`]) of the commit with this full id.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), commit_id = %commit))]
    pub async fn tree_key(&self, commit: &CommitIdHex) -> Result<String, CommitLookupError> {
        let commit = self
            .commit_by_id(commit)
            .await
            .inspect_err(|error| crate::log_failure!(error))?;
        Ok(tree_key(&commit))
    }
}

/// `dir` resolved, as jj's working copy wants its root.
fn canonical(dir: &Path) -> Result<PathBuf, MaterializeError> {
    std::fs::canonicalize(dir).map_err(|source| MaterializeError::Io {
        op: "resolve",
        path: dir.to_owned(),
        source,
    })
}

/// Creates `dir` and its parents, or checks that an existing `dir` is
/// empty.
fn create_empty_dir(dir: &Path) -> Result<(), MaterializeError> {
    let io = |op, source| MaterializeError::Io {
        op,
        path: dir.to_owned(),
        source,
    };
    std::fs::create_dir_all(dir).map_err(|source| io("create directory", source))?;
    let mut entries = std::fs::read_dir(dir).map_err(|source| io("read directory", source))?;
    if entries.next().is_some() {
        return Err(MaterializeError::NotEmpty(dir.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_keys_are_checked() {
        assert!(is_tree_key("0a1b"));
        assert!(is_tree_key("0a+1b+2c"));
        for bad in ["", "+", "0a+", "+0a", "0A", "../x", "0a/1b", "0a++1b"] {
            assert!(!is_tree_key(bad), "{bad:?}");
        }
    }

    #[test]
    fn missing_tree_state_is_io() {
        let dir = tempfile::tempdir().unwrap();
        let error = tree_state_entries(dir.path()).unwrap_err();
        assert!(matches!(error, TreeStateReadError::Io { .. }), "{error:?}");
        assert_eq!(error.kind(), ErrorKind::Io);

        std::fs::write(dir.path().join(TREE_STATE_FILE), b"\xff\xff\xff").unwrap();
        let error = tree_state_entries(dir.path()).unwrap_err();
        assert!(
            matches!(error, TreeStateReadError::Decode { .. }),
            "{error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::Internal);
    }
}
