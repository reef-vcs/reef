//! Working copies after a restore from backup (R1.18, ADR 0020). A backup
//! holds a repository's `.jj/repo` and never a working copy, which is a
//! cache of the files: the user's `default` working copy is recreated when
//! a backup is restored to a new root, and a Reef workspace's when its
//! state directory is gone.

use std::io;
use std::path::{Path, PathBuf};

use jj_lib::backend::BackendError;
use jj_lib::default_backend_factories::default_backend_factories;
use jj_lib::file_util::PathError;
use jj_lib::local_working_copy::LocalWorkingCopy;
use jj_lib::ref_name::{WorkspaceName, WorkspaceNameBuf};
use jj_lib::repo::{Repo as _, RepoLoader, RepoLoaderError, StoreLoadError};
use jj_lib::settings::UserSettings;
use jj_lib::working_copy::{CheckoutError, WorkingCopy as _, WorkingCopyStateError};
use jj_lib::workspace::Workspace;
use jj_lib::workspace_store::WorkspaceStoreError;

use super::repo::{ChangeIdHex, ChangeSummary, CommitIdHex, OperationIdHex, Repo};
use super::workspace::{CheckoutSummary, CreateWorkspaceError};
use crate::error::{Classify, ErrorKind, classify_by_source};

/// The file naming a working copy's implementation, beside its state.
pub const WORKING_COPY_TYPE_FILE: &str = "type";

/// Failure to recreate the `default` working copy.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RecreateWorkingCopyError {
    /// The root has a working copy already.
    #[error("{} already has a working copy", .0.display())]
    Exists(PathBuf),
    /// The repository's stores could not be loaded.
    #[error("failed to load the repository's stores")]
    Stores(#[from] StoreLoadError),
    /// The repository could not be loaded at its head operation.
    #[error("failed to load the repository at its head operation")]
    Load(#[from] RepoLoaderError),
    /// A directory or file could not be written.
    #[error("failed to write {}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The working-copy state could not be written.
    #[error("failed to initialize the working-copy state")]
    State(#[from] WorkingCopyStateError),
    /// The root could not be canonicalized by jj.
    #[error("failed to open the root")]
    Path(#[from] PathError),
    /// The workspace could not be registered in jj's workspace store.
    #[error("failed to register the workspace with jj")]
    WorkspaceStore(#[from] WorkspaceStoreError),
    /// The working-copy commit could not be read.
    #[error("failed to read the working-copy commit")]
    Backend(#[from] BackendError),
    /// The working-copy commit's files could not be written.
    #[error("failed to check out the working-copy commit")]
    Checkout(#[from] CheckoutError),
}

impl Classify for RecreateWorkingCopyError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Exists(_) => ErrorKind::AlreadyExists,
            Self::Io { source, .. } => classify_by_source(source),
            Self::Stores(error) => classify_by_source(error),
            Self::Load(error) => classify_by_source(error),
            Self::State(error) => classify_by_source(error),
            Self::Path(error) => classify_by_source(error),
            Self::WorkspaceStore(error) => classify_by_source(error),
            Self::Backend(error) => classify_by_source(error),
            Self::Checkout(error) => classify_by_source(error),
        }
    }
}

impl Repo {
    /// Creates the `default` working copy at `root`, whose `.jj/repo` was
    /// restored from a backup and whose `.jj/working_copy` does not exist,
    /// as `jj git init` would have left it: jj's local working copy at the
    /// head operation (`.jj/working_copy`, with its `type` file), `default`
    /// registered in the workspace store at `root`, and the files of
    /// `default`'s working-copy commit in the head view checked out. With
    /// no such commit, nothing is checked out. Returns the repository and
    /// what the checkout wrote.
    #[tracing::instrument(skip_all, fields(path = %root.display()))]
    pub async fn recreate_default_working_copy(
        root: &Path,
        settings: &UserSettings,
    ) -> Result<(Repo, CheckoutSummary), RecreateWorkingCopyError> {
        Self::recreate_default_unlogged(root, settings)
            .await
            .inspect_err(|error| crate::log_failure!(error))
    }

    async fn recreate_default_unlogged(
        root: &Path,
        settings: &UserSettings,
    ) -> Result<(Repo, CheckoutSummary), RecreateWorkingCopyError> {
        let jj_dir = root.join(".jj");
        let state_path = jj_dir.join("working_copy");
        if std::fs::symlink_metadata(&state_path).is_ok() {
            return Err(RecreateWorkingCopyError::Exists(root.to_owned()));
        }
        let repo_path = std::fs::canonicalize(jj_dir.join("repo")).map_err(|source| {
            RecreateWorkingCopyError::Io {
                path: jj_dir.join("repo"),
                source,
            }
        })?;
        let loader =
            RepoLoader::init_from_file_system(settings, &repo_path, &default_backend_factories())?;
        let repo = loader.load_at_head().await?;
        let io_error = |path: &Path| {
            let path = path.to_owned();
            move |source| RecreateWorkingCopyError::Io { path, source }
        };
        std::fs::create_dir(&state_path).map_err(io_error(&state_path))?;
        let root = std::fs::canonicalize(root).map_err(io_error(root))?;
        let name: WorkspaceNameBuf = WorkspaceName::DEFAULT.to_owned();
        let working_copy = LocalWorkingCopy::init(
            repo.store().clone(),
            root.clone(),
            state_path.clone(),
            repo.op_id().clone(),
            name.clone(),
            settings,
        )?;
        let type_file = state_path.join(WORKING_COPY_TYPE_FILE);
        std::fs::write(&type_file, working_copy.name()).map_err(io_error(&type_file))?;
        let mut workspace = Workspace::new(&root, repo_path, Box::new(working_copy), loader)?;
        workspace
            .repo_loader()
            .workspace_store()
            .add(&name, workspace.workspace_root())?;
        let mut summary = CheckoutSummary {
            updated: 0,
            added: 0,
            removed: 0,
            skipped: 0,
        };
        if let Some(id) = repo.view().get_wc_commit_id(&name) {
            let commit = repo.store().get_commit_async(id).await?;
            let stats = workspace
                .check_out(repo.op_id().clone(), None, &commit)
                .await?;
            summary = CheckoutSummary {
                updated: stats.updated_files,
                added: stats.added_files,
                removed: stats.removed_files,
                skipped: stats.skipped_files,
            };
        }
        // Debug: `reef restore` runs this in the CLI, which reports it.
        tracing::debug!(
            op_id = %OperationIdHex::of(repo.op_id()),
            added_count = summary.added,
            "default working copy recreated"
        );
        Ok((Repo { workspace, repo }, summary))
    }

    /// The working-copy commit of the workspace `name` in the loaded view,
    /// if it has one.
    pub fn workspace_commit(&self, name: &str) -> Option<ChangeSummary> {
        let id = self
            .repo
            .view()
            .get_wc_commit_id(&WorkspaceNameBuf::from(name))?;
        let commit = self.repo.store().get_commit(id).ok()?;
        Some(ChangeSummary {
            change_id: ChangeIdHex::of(commit.change_id()),
            commit_id: CommitIdHex::of(commit.id()),
            description: commit.description().to_owned(),
            is_wc: true,
        })
    }

    /// Gives the workspace `name`, which has a working-copy commit in the
    /// loaded view, a new working copy at `root` with its state in
    /// `state_dir` (which must not exist), at the loaded operation, and
    /// registers it in jj's workspace store at `root`: what
    /// [`Repo::create_workspace_with`] does after its operation, for a
    /// workspace whose state directory is gone (R1.18's rematerialization).
    /// No operation is written. The working copy records the empty tree, as
    /// a new one does: the caller seeds its state from a base or calls
    /// [`Repo::reset_to`].
    #[tracing::instrument(
        skip_all,
        fields(repo = %self.workspace_root().display(), path = %root.display())
    )]
    pub fn attach_workspace_with(
        &self,
        name: &str,
        root: &Path,
        state_dir: &Path,
        wc_settings: &UserSettings,
    ) -> Result<Repo, CreateWorkspaceError> {
        self.attach_workspace_unlogged(name, root, state_dir, wc_settings)
            .inspect_err(|error| crate::log_failure!(error))
    }

    fn attach_workspace_unlogged(
        &self,
        name: &str,
        root: &Path,
        state_dir: &Path,
        wc_settings: &UserSettings,
    ) -> Result<Repo, CreateWorkspaceError> {
        let name = WorkspaceNameBuf::from(name);
        if self.repo.view().get_wc_commit_id(&name).is_none() {
            return Err(CreateWorkspaceError::NoSuchWorkspace(
                name.as_str().to_owned(),
            ));
        }
        let root = std::fs::canonicalize(root).map_err(|source| CreateWorkspaceError::Root {
            path: root.to_owned(),
            source,
        })?;
        super::workspace::create_state_dir(state_dir)?;
        let op_id = self.repo.op_id().clone();
        let workspace = self
            .attach_new_workspace(&name, &root, state_dir, &op_id, wc_settings)
            .inspect_err(|_| {
                std::fs::remove_dir_all(state_dir).ok();
            })?;
        Ok(Repo {
            workspace,
            repo: self.repo.clone(),
        })
    }
}
