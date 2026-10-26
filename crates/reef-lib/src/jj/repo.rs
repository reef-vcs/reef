//! Opening a jj workspace and reading its current state.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
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

use super::oplog::{OpKind, OpMeta};
use crate::error::{Classify, ErrorKind, classify_by_source};

macro_rules! hex_id {
    ($(#[$doc:meta])* $name:ident, $what:literal) => {
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

        impl FromStr for $name {
            type Err = ParseHexIdError;

            /// Accepts a non-empty, even number of lowercase hex digits, the
            /// form [`Display`](fmt::Display) writes. The length depends on
            /// the backend (ADR 0011), so it is not checked here.
            fn from_str(text: &str) -> Result<Self, Self::Err> {
                let valid = !text.is_empty()
                    && text.len() % 2 == 0
                    && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
                if valid {
                    Ok(Self(text.to_owned()))
                } else {
                    Err(ParseHexIdError {
                        what: $what,
                        text: text.to_owned(),
                    })
                }
            }
        }
    };
}

hex_id!(
    /// An operation id, in the lowercase hex jj prints.
    OperationIdHex,
    "operation"
);
hex_id!(
    /// A commit id, in lowercase hex. With the git backend this is the git
    /// commit hash.
    CommitIdHex,
    "commit"
);
hex_id!(
    /// A change id, in lowercase hex like the other ids. `jj log` shows
    /// change ids in "reverse hex" instead; [`ChangeIdHex::reverse_hex`]
    /// converts.
    ChangeIdHex,
    "change"
);

/// Text that is not an id of the expected kind in lowercase hex.
#[derive(Debug, thiserror::Error)]
#[error("{text:?} is not a valid {what} id")]
pub struct ParseHexIdError {
    /// The kind of id expected: `operation`, `commit`, or `change`.
    pub what: &'static str,
    /// The rejected text.
    pub text: String,
}

impl Classify for ParseHexIdError {
    fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidArgument
    }
}

impl OperationIdHex {
    pub(crate) fn of(id: &jj_lib::op_store::OperationId) -> Self {
        Self(id.hex())
    }

    /// The id as jj-lib's type. The text is valid hex by construction, so
    /// the empty fallback is never taken.
    pub(crate) fn to_id(&self) -> jj_lib::op_store::OperationId {
        jj_lib::op_store::OperationId::try_from_hex(&self.0)
            .unwrap_or_else(|| jj_lib::op_store::OperationId::new(Vec::new()))
    }
}

impl CommitIdHex {
    pub(crate) fn of(id: &jj_lib::backend::CommitId) -> Self {
        Self(id.hex())
    }

    /// The id as jj-lib's type. The text is valid hex by construction, so
    /// the empty fallback is never taken.
    pub(crate) fn to_id(&self) -> jj_lib::backend::CommitId {
        jj_lib::backend::CommitId::try_from_hex(&self.0)
            .unwrap_or_else(|| jj_lib::backend::CommitId::new(Vec::new()))
    }
}

impl ChangeIdHex {
    pub(crate) fn of(id: &jj_lib::backend::ChangeId) -> Self {
        Self(id.hex())
    }

    /// The change id in jj's "reverse hex", the spelling `jj log` shows
    /// humans: the digits `0` to `f` become the letters `z` to `k`.
    pub fn reverse_hex(&self) -> String {
        self.0
            .bytes()
            .map(|digit| match digit {
                b'0'..=b'9' => char::from(b'z' - (digit - b'0')),
                b'a'..=b'f' => char::from(b'p' - (digit - b'a')),
                other => char::from(other),
            })
            .collect()
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
    /// The directory has no `.git` (R1.25).
    #[error("{} is not a git repository: it has no .git directory", .0.display())]
    NotAGitRepo(PathBuf),
    /// The directory's `.git` is a file: a git worktree or submodule
    /// checkout, whose repository lives elsewhere (R1.25).
    #[error("{} is a git worktree or submodule: its .git is a file", .0.display())]
    GitFile(PathBuf),
    /// git's HEAD could not be imported, or the working-copy commit moved
    /// onto it.
    #[error("failed to check out git's HEAD")]
    Head(#[source] Box<dyn std::error::Error + Send + Sync>),
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
            Self::NotAGitRepo(_) | Self::GitFile(_) => ErrorKind::InvalidArgument,
            Self::Head(error) => classify_by_source(&**error),
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

    /// Creates a jj repository colocated with a new git repository at
    /// `path/.git` (git and jj share the working copy, as `jj git init
    /// --colocate` does), in the existing directory `path`, with one empty
    /// working-copy commit, and opens it. Files already in `path` are not
    /// snapshotted.
    #[tracing::instrument(skip_all, fields(path = %path.display()))]
    pub async fn init_colocated_git(
        path: &Path,
        settings: &UserSettings,
    ) -> Result<Self, InitError> {
        let (workspace, repo) =
            Workspace::init_colocated_git(settings, path, gix::hash::Kind::Sha1)
                .await
                .map_err(InitError::from)
                .inspect_err(|error| crate::log_failure!(error))?;
        let repo = Self { workspace, repo };
        tracing::info!(
            repo = %repo.workspace_root().display(),
            op_id = %repo.op_id(),
            "colocated repository created"
        );
        Ok(repo)
    }

    /// Creates the jj side of a colocated repository in the existing git
    /// repository at `root` (`root/.git` must be a directory), with one empty
    /// working-copy commit on top of git's HEAD, and opens it. The equivalent
    /// of `jj git init --colocate` in a checkout that already has `.git`
    /// (R1.25). The working-copy state records HEAD's tree without reading
    /// the files, so jj's next snapshot of `root` rehashes them. With an
    /// unborn HEAD the working-copy commit stays on the root commit. If a
    /// step fails, the `.jj` directory it made is removed.
    ///
    /// The check-out is one operation, `import git head`, of kind
    /// [`OpKind::Other`], with `agent` as its `reef.agent` (R2.03).
    #[tracing::instrument(skip_all, fields(path = %root.display()))]
    pub async fn init_in_git_repo(
        root: &Path,
        settings: &UserSettings,
        agent: Option<&str>,
    ) -> Result<Self, InitError> {
        let dot_git = root.join(".git");
        if !dot_git.is_dir() {
            let error = if dot_git.exists() {
                InitError::GitFile(root.to_owned())
            } else {
                InitError::NotAGitRepo(root.to_owned())
            };
            crate::log_failure!(&error);
            return Err(error);
        }
        let (workspace, repo) = Workspace::init_external_git(settings, root, &dot_git)
            .await
            .map_err(InitError::from)
            .inspect_err(|error| crate::log_failure!(error))?;
        let mut repo = Self { workspace, repo };
        let mut meta = OpMeta::new(OpKind::Other, "import git head");
        meta.agent = agent.map(str::to_owned);
        if let Err(source) = repo.check_out_git_head(&meta).await {
            drop(repo);
            // jj-lib removes `.jj` when one of its own steps fails; so does this.
            std::fs::remove_dir_all(root.join(".jj")).ok();
            let error = InitError::Head(source);
            crate::log_failure!(&error);
            return Err(error);
        }
        tracing::info!(
            repo = %repo.workspace_root().display(),
            op_id = %repo.op_id(),
            "repository created alongside git"
        );
        Ok(repo)
    }

    /// Moves this workspace's working-copy commit onto a new empty change
    /// on git's HEAD and records HEAD's tree as the working-copy state, as
    /// `jj git init --colocate` does. Nothing is written for an unborn HEAD.
    async fn check_out_git_head(
        &mut self,
        meta: &OpMeta,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let name = self.workspace.workspace_name().to_owned();
        let mut tx = self.repo.start_transaction();
        jj_lib::git::import_head(tx.repo_mut(), &name, self.workspace.workspace_root()).await?;
        let Some(head_id) = tx.repo().view().git_head(&name).as_normal().cloned() else {
            return Ok(());
        };
        let head = tx.repo().store().get_commit_async(&head_id).await?;
        let wc_commit = tx.repo_mut().check_out(name, &head).await?;
        tx.repo_mut().rebase_descendants().await?;
        // Working copy first, as jj does: the lock is held from the reset
        // until the state names the new operation.
        let mut locked = self.workspace.start_working_copy_mutation().await?;
        locked.locked_wc().reset(&wc_commit).await?;
        self.repo = meta.commit(tx).await?;
        locked.finish(self.repo.op_id().clone()).await?;
        Ok(())
    }

    /// Whether the repository's store is a git repository at
    /// `<workspace root>/.git`, shared with git's own working copy: a
    /// colocated repository, whose git refs [`Repo::import_git_refs`] brings
    /// into jj's view.
    pub fn is_colocated(&self) -> bool {
        let Ok(git_backend) = jj_lib::git::get_git_backend(self.repo.store()) else {
            return false;
        };
        let dot_git = self.workspace_root().join(".git");
        match (
            std::fs::canonicalize(git_backend.git_repo_path()),
            std::fs::canonicalize(&dot_git),
        ) {
            (Ok(store), Ok(dot_git)) => store == dot_git,
            _ => false,
        }
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

    /// The repository as loaded, which is `Send + Sync` and may be shared
    /// with other threads (ADR 0012); it does not follow later reloads.
    pub fn readonly(&self) -> Arc<ReadonlyRepo> {
        Arc::clone(&self.repo)
    }

    /// The repository as loaded: the head this workspace snapshots
    /// against.
    pub fn head(&self) -> &Arc<ReadonlyRepo> {
        &self.repo
    }

    /// Points this workspace at `repo`, a load of the same repository at
    /// another operation, such as the head a repository thread holds. The
    /// next [`Repo::begin_snapshot`] checks the working copy against it.
    pub fn set_head(&mut self, repo: Arc<ReadonlyRepo>) {
        self.repo = repo;
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

    /// The number of files, symlinks, and conflicts in this workspace's
    /// working-copy commit as of the loaded view: what a full scan of its
    /// working copy walks. Reads tree objects from the store, not the disk.
    /// R1.14's `watcher.backend = "auto"` compares it with
    /// `watcher.watchman_threshold_files`.
    pub fn tracked_file_count(&self) -> Result<u64, ReadError> {
        let tree = self.wc_commit()?.tree();
        let mut count = 0;
        for (_, value) in tree.entries() {
            value?;
            count += 1;
        }
        Ok(count)
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

    /// [`Repo::reload_at_head`] if another process or [`Repo`] has written
    /// an operation since this one was loaded (the operation heads are no
    /// longer exactly the loaded operation); otherwise nothing, at the cost
    /// of listing the operation heads. Returns whether it reloaded.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn reload_if_moved(&mut self) -> Result<bool, OpenError> {
        let heads = self
            .repo
            .op_heads_store()
            .get_op_heads()
            .await
            .map_err(|error| OpenError::Repo(error.into()))
            .inspect_err(|error| crate::log_failure!(error))?;
        if let [head] = heads.as_slice()
            && head == self.repo.op_id()
        {
            return Ok(false);
        }
        self.reload_at_head().await?;
        Ok(true)
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

#[cfg(test)]
mod tests {
    use jj_lib::backend::ChangeId;

    use super::*;

    #[test]
    fn hex_ids_parse_lowercase_hex() {
        assert_eq!("0a1b".parse::<CommitIdHex>().unwrap().as_str(), "0a1b");
        assert!("0a1b".parse::<ChangeIdHex>().is_ok());
        assert!("".parse::<OperationIdHex>().is_err());
        assert!("abc".parse::<OperationIdHex>().is_err(), "odd length");
        assert!("0A1B".parse::<CommitIdHex>().is_err(), "uppercase");
        let error = "kzpq".parse::<ChangeIdHex>().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidArgument);
        assert_eq!(error.to_string(), "\"kzpq\" is not a valid change id");
    }

    #[test]
    fn reverse_hex_matches_jj() {
        let id = ChangeId::try_from_hex("0123456789abcdeffedcba9876543210").unwrap();
        let hex = ChangeIdHex::of(&id);
        assert_eq!(hex.as_str(), "0123456789abcdeffedcba9876543210");
        assert_eq!(hex.reverse_hex(), id.reverse_hex());
        assert_eq!(hex.reverse_hex(), "zyxwvutsrqponmlkklmnopqrstuvwxyz");
    }
}
