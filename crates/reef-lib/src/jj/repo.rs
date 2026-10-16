//! Opening a jj workspace and reading its current state.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jj_lib::backend::BackendError;
use jj_lib::commit::Commit;
use jj_lib::default_backend_factories::{
    default_backend_factories, default_working_copy_factories,
};
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::{ReadonlyRepo, Repo as _, RepoLoaderError};
use jj_lib::settings::UserSettings;
use jj_lib::workspace::{Workspace, WorkspaceInitError, WorkspaceLoadError};

use crate::error::{Classify, ErrorKind, classify_by_source};

macro_rules! hex_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// The id as a hex string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

hex_id!(
    /// An operation id, in the lowercase hex jj prints.
    OperationIdHex
);
hex_id!(
    /// A commit id, in lowercase hex. With the git backend this is the git
    /// commit hash.
    CommitIdHex
);
hex_id!(
    /// A change id, in jj's "reverse hex" (`k` to `z`) form, as `jj log` shows it.
    ChangeIdHex
);

impl OperationIdHex {
    pub(crate) fn of(id: &jj_lib::op_store::OperationId) -> Self {
        Self(id.hex())
    }
}

impl CommitIdHex {
    pub(crate) fn of(id: &jj_lib::backend::CommitId) -> Self {
        Self(id.hex())
    }
}

impl ChangeIdHex {
    pub(crate) fn of(id: &jj_lib::backend::ChangeId) -> Self {
        Self(id.reverse_hex())
    }
}

/// One head commit of the repository view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeSummary {
    /// The change the commit belongs to.
    pub change_id: ChangeIdHex,
    /// The commit itself.
    pub commit_id: CommitIdHex,
    /// The full commit description.
    pub description: String,
    /// Whether this commit is the working-copy commit of this workspace.
    pub is_wc: bool,
}

/// Failure to open or reload a jj repository.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    /// Neither the path nor any of its ancestors contains a `.jj` directory.
    #[error("no jj repository at {0} or any parent directory")]
    NotAJjRepo(PathBuf),
    /// A relative path could not be made absolute because the current
    /// directory is unavailable.
    #[error("failed to resolve {path} against the current directory")]
    CurrentDir {
        /// The path as given.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The workspace exists but could not be loaded.
    #[error("failed to load the jj workspace")]
    Load(#[from] WorkspaceLoadError),
    /// The repository could not be loaded at its head operation.
    #[error("failed to load the jj repository")]
    Repo(#[from] RepoLoaderError),
}

/// Failure to create a jj repository.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InitError {
    /// The repository could not be created.
    #[error("failed to create the jj repository")]
    Init(#[from] WorkspaceInitError),
}

/// Failure to read a commit from the store.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReadError {
    /// The view has no working-copy commit for this workspace.
    #[error("workspace {0} has no working-copy commit")]
    NoWorkingCopyCommit(String),
    /// The backend failed to read a commit.
    #[error("failed to read a commit")]
    Backend(#[from] BackendError),
}

impl Classify for OpenError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NotAJjRepo(_)
            | Self::Load(
                WorkspaceLoadError::NoWorkspaceHere(_) | WorkspaceLoadError::RepoDoesNotExist(_),
            ) => ErrorKind::NotFound,
            Self::CurrentDir { .. } => ErrorKind::Io,
            Self::Load(error) => classify_by_source(error),
            Self::Repo(error) => classify_by_source(error),
        }
    }
}

impl Classify for InitError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Init(WorkspaceInitError::DestinationExists(_)) => ErrorKind::AlreadyExists,
            Self::Init(error) => classify_by_source(error),
        }
    }
}

impl Classify for ReadError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NoWorkingCopyCommit(_) => ErrorKind::NotFound,
            Self::Backend(error) => classify_by_source(error),
        }
    }
}

/// An open jj workspace and its repository, loaded at some operation.
pub struct Repo {
    pub(crate) workspace: Workspace,
    pub(crate) repo: Arc<ReadonlyRepo>,
}

impl Repo {
    /// Opens the jj workspace containing `path`, searching parent directories
    /// for `.jj` the way the `jj` CLI does, and loads the repository at its
    /// head operation. Divergent operation heads are merged on load. A
    /// relative `path` is resolved against the current directory.
    #[tracing::instrument(skip_all, fields(path = %path.display()))]
    pub async fn open(path: &Path, settings: &UserSettings) -> Result<Self, OpenError> {
        let repo = Self::open_unlogged(path, settings)
            .await
            .inspect_err(|error| crate::log_failure!(error))?;
        tracing::debug!(
            repo = %repo.workspace_root().display(),
            op_id = %repo.op_id(),
            "repository opened"
        );
        Ok(repo)
    }

    /// Creates a jj repository backed by an internal git store (in
    /// `.jj/repo/store/git`, with SHA-1 object ids) in the existing directory
    /// `path`, with one empty working-copy commit, and opens it. Files already
    /// in `path` are not snapshotted.
    #[tracing::instrument(skip_all, fields(path = %path.display()))]
    pub async fn init_internal_git(
        path: &Path,
        settings: &UserSettings,
    ) -> Result<Self, InitError> {
        let (workspace, repo) = Workspace::init_internal_git(settings, path, gix::hash::Kind::Sha1)
            .await
            .map_err(InitError::from)
            .inspect_err(|error| crate::log_failure!(error))?;
        let repo = Self { workspace, repo };
        tracing::info!(
            repo = %repo.workspace_root().display(),
            op_id = %repo.op_id(),
            "repository created"
        );
        Ok(repo)
    }

    async fn open_unlogged(path: &Path, settings: &UserSettings) -> Result<Self, OpenError> {
        let absolute = std::path::absolute(path).map_err(|source| OpenError::CurrentDir {
            path: path.to_owned(),
            source,
        })?;
        // jj-lib's default workspace loader only looks in the given directory;
        // the parent search is jj-cli's `find_workspace_dir`.
        let root = absolute
            .ancestors()
            .find(|dir| dir.join(".jj").is_dir())
            .ok_or_else(|| OpenError::NotAJjRepo(path.to_owned()))?;
        let workspace = Workspace::load(
            settings,
            root,
            &default_backend_factories(),
            &default_working_copy_factories(),
        )?;
        let repo = workspace.repo_loader().load_at_head().await?;
        Ok(Self { workspace, repo })
    }

    /// The directory holding `.jj`, which is the root of the working copy.
    pub fn workspace_root(&self) -> &Path {
        self.workspace.workspace_root()
    }

    /// The `.jj/repo` directory.
    pub fn repo_path(&self) -> &Path {
        self.workspace.repo_path()
    }

    /// The name of this workspace, `default` for the first one.
    pub fn workspace_name(&self) -> &str {
        self.workspace.workspace_name().as_str()
    }

    /// The operation the repository is currently loaded at.
    pub fn op_id(&self) -> OperationIdHex {
        OperationIdHex::of(self.repo.op_id())
    }

    /// This workspace's working-copy commit in the loaded view.
    pub fn wc_commit(&self) -> Result<Commit, ReadError> {
        let id = self
            .repo
            .view()
            .get_wc_commit_id(self.workspace.workspace_name())
            .ok_or_else(|| ReadError::NoWorkingCopyCommit(self.workspace_name().to_owned()))?;
        Ok(self.repo.store().get_commit(id)?)
    }

    /// Reloads the repository at the current head operation, picking up
    /// operations written by other processes or other [`Repo`] values.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn reload_at_head(&mut self) -> Result<(), OpenError> {
        self.repo = self
            .repo
            .reload_at_head()
            .await
            .map_err(OpenError::from)
            .inspect_err(|error| crate::log_failure!(error))?;
        tracing::debug!(op_id = %self.op_id(), "repository reloaded");
        Ok(())
    }

    /// The head commits of the loaded view, sorted by commit id.
    pub fn heads(&self) -> Result<Vec<ChangeSummary>, ReadError> {
        let wc_id = self
            .repo
            .view()
            .get_wc_commit_id(self.workspace.workspace_name());
        let mut ids: Vec<_> = self.repo.view().heads().iter().collect();
        ids.sort();
        ids.into_iter()
            .map(|id| {
                let commit = self.repo.store().get_commit(id)?;
                Ok(ChangeSummary {
                    change_id: ChangeIdHex::of(commit.change_id()),
                    commit_id: CommitIdHex::of(id),
                    description: commit.description().to_owned(),
                    is_wc: wc_id == Some(id),
                })
            })
            .collect()
    }
}
