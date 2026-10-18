//! Reading `reef.toml` and other files from a commit's tree, never from
//! the disk (ADR 0021).

use futures::AsyncReadExt as _;
use jj_lib::backend::{BackendError, FileId, TreeId, TreeValue};
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPath;

use super::materialize::{TREE_KEY_SEPARATOR, is_tree_key};
use super::repo::{CommitIdHex, Repo};
use super::workspace::CommitLookupError;
use crate::error::{Classify, ErrorKind, classify_by_source};
use crate::spec::{SPEC_FILE, Spec, SpecError};

/// Failure to read a file from a tree.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReadFileError {
    /// The commit could not be read.
    #[error(transparent)]
    Lookup(#[from] CommitLookupError),
    /// The tree key is not one [`tree_key`](super::materialize::tree_key)
    /// produces. A well-formed key of a tree the repository does not have
    /// fails at the read, as [`ReadFileError::Backend`].
    #[error("{0:?} is not a tree of this repository")]
    InvalidTree(String),
    /// The path is a symlink, directory, submodule, or conflict.
    #[error("{path} is a {kind}, not a regular file")]
    NotAFile {
        /// The path, repository-root relative.
        path: String,
        /// What it is instead.
        kind: &'static str,
    },
    /// The backend failed to read the tree or the file.
    #[error("failed to read {path}")]
    Backend {
        /// The path, repository-root relative.
        path: String,
        /// The underlying error.
        #[source]
        source: BackendError,
    },
    /// Reading the file's contents failed.
    #[error("failed to read {path}")]
    Io {
        /// The path, repository-root relative.
        path: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

impl Classify for ReadFileError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Lookup(error) => error.kind(),
            Self::InvalidTree(_) | Self::NotAFile { .. } => ErrorKind::InvalidArgument,
            Self::Backend { source, .. } => classify_by_source(source),
            Self::Io { .. } => ErrorKind::Io,
        }
    }
}

impl Repo {
    /// Reads `reef.toml` from `commit`'s tree. `Ok(None)` when the tree has
    /// none. Checks that every declared lockfile exists in the tree.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), commit_id = %commit))]
    pub async fn spec_at(&self, commit: &CommitIdHex) -> Result<Option<Spec>, SpecError> {
        let read = async {
            let commit = self
                .commit_by_id(commit)
                .await
                .map_err(|error| SpecError::Read(error.into()))?;
            spec_in(&commit.tree()).await
        };
        read.await.inspect_err(|error| crate::log_failure!(error))
    }

    /// [`Repo::spec_at`] for the tree with this base cache key
    /// ([`tree_key`](super::materialize::tree_key)): what a workspace whose
    /// base is that tree was created with, read again when the daemon
    /// restarts.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), tree_id = %tree_key))]
    pub async fn spec_in_tree(&self, tree_key: &str) -> Result<Option<Spec>, SpecError> {
        let read = async {
            let tree = self.tree_by_key(tree_key).map_err(SpecError::Read)?;
            spec_in(&tree).await
        };
        read.await.inspect_err(|error| crate::log_failure!(error))
    }

    /// The raw bytes of `path` in `commit`'s tree, for lockfile hashing
    /// (R1.20). `Ok(None)` when the tree has nothing there.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), commit_id = %commit))]
    pub async fn read_file_at(
        &self,
        commit: &CommitIdHex,
        path: &RepoPath,
    ) -> Result<Option<Vec<u8>>, ReadFileError> {
        let read = async {
            let commit = self.commit_by_id(commit).await?;
            read_file_in(&commit.tree(), path).await
        };
        read.await.inspect_err(|error| crate::log_failure!(error))
    }

    fn tree_by_key(&self, key: &str) -> Result<MergedTree, ReadFileError> {
        let invalid = || ReadFileError::InvalidTree(key.to_owned());
        if !is_tree_key(key) {
            return Err(invalid());
        }
        let ids = key
            .split(TREE_KEY_SEPARATOR)
            .map(|hex| TreeId::try_from_hex(hex).ok_or_else(invalid))
            .collect::<Result<Vec<_>, _>>()?;
        // A conflicted tree has an odd number of terms (add, remove, add, ...).
        if ids.len() % 2 == 0 {
            return Err(invalid());
        }
        let store = std::sync::Arc::clone(self.repo.store());
        Ok(MergedTree::new(
            store,
            Merge::from_vec(ids),
            ConflictLabels::unlabeled(),
        ))
    }
}

/// `reef.toml` in `tree`, parsed, with every lockfile checked.
async fn spec_in(tree: &MergedTree) -> Result<Option<Spec>, SpecError> {
    #[expect(clippy::expect_used, reason = "a constant, valid repository path")]
    let path = RepoPath::from_internal_string(SPEC_FILE).expect("reef.toml is a valid path");
    let Some(bytes) = read_file_in(tree, path).await.map_err(SpecError::Read)? else {
        return Ok(None);
    };
    let spec = Spec::parse_bytes(&bytes)?;
    for store in &spec.stores {
        let found = file_id(tree, store.lockfile_path())
            .await
            .map_err(SpecError::Read)?;
        if found.is_none() {
            return Err(SpecError::LockfileMissing {
                name: store.name.clone(),
                lockfile: store.lockfile.clone(),
            });
        }
    }
    Ok(Some(spec))
}

/// The bytes of the regular file at `path` in `tree`, `None` if absent.
async fn read_file_in(
    tree: &MergedTree,
    path: &RepoPath,
) -> Result<Option<Vec<u8>>, ReadFileError> {
    let Some(id) = file_id(tree, path).await? else {
        return Ok(None);
    };
    let name = || path.as_internal_file_string().to_owned();
    let mut reader =
        tree.store()
            .read_file(path, &id)
            .await
            .map_err(|source| ReadFileError::Backend {
                path: name(),
                source,
            })?;
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| ReadFileError::Io {
            path: name(),
            source,
        })?;
    Ok(Some(bytes))
}

/// The blob id of the regular file at `path` in `tree`, `None` if absent;
/// anything else there is [`ReadFileError::NotAFile`].
async fn file_id(tree: &MergedTree, path: &RepoPath) -> Result<Option<FileId>, ReadFileError> {
    let name = || path.as_internal_file_string().to_owned();
    let value = tree
        .path_value(path)
        .await
        .map_err(|source| ReadFileError::Backend {
            path: name(),
            source,
        })?;
    let not_a_file = |kind| ReadFileError::NotAFile { path: name(), kind };
    match value.into_resolved() {
        Ok(None) => Ok(None),
        Ok(Some(TreeValue::File { id, .. })) => Ok(Some(id)),
        Ok(Some(TreeValue::Symlink(_))) => Err(not_a_file("symlink")),
        Ok(Some(TreeValue::Tree(_))) => Err(not_a_file("directory")),
        Ok(Some(TreeValue::GitSubmodule(_))) => Err(not_a_file("submodule")),
        Err(_) => Err(not_a_file("conflict")),
    }
}
