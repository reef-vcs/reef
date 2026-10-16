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
use jj_lib::workspace::{Workspace, WorkspaceLoadError};

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
    /// The workspace exists but could not be loaded.
    #[error("failed to load the jj workspace")]
    Load(#[from] WorkspaceLoadError),
    /// The repository could not be loaded at its head operation.
    #[error("failed to load the jj repository")]
    Repo(#[from] RepoLoaderError),
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

/// An open jj workspace and its repository, loaded at some operation.
pub struct Repo {
    pub(crate) workspace: Workspace,
    pub(crate) repo: Arc<ReadonlyRepo>,
}

impl Repo {
    /// Opens the jj workspace containing `path`, searching parent directories
    /// for `.jj` the way the `jj` CLI does, and loads the repository at its
    /// head operation. Divergent operation heads are merged on load.
    pub async fn open(path: &Path, settings: &UserSettings) -> Result<Self, OpenError> {
        // jj-lib's default workspace loader only looks in the given directory;
        // the parent search is jj-cli's `find_workspace_dir`.
        let root = path
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
    pub async fn reload_at_head(&mut self) -> Result<(), OpenError> {
        self.repo = self.repo.reload_at_head().await?;
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
