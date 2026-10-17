//! Reef workspaces as jj workspaces (ADR 0014), and resolving what a user
//! names as a revision.
//!
//! A Reef workspace is a jj workspace of the user's repository with its own
//! working-copy commit, named `reef-<id>` by the daemon. Its working-copy
//! state (jj's `checkout` and `tree_state` files, and the working-copy lock)
//! lives in a directory the caller chooses, outside the tree, so the
//! workspace directory holds only files: no `.jj`. The workspace is
//! registered in jj's workspace store, so `jj workspace list` in the user's
//! checkout shows it.
//!
//! These methods run on the [`Repo`] of the user's own workspace, which
//! hands its repository loader to every Reef workspace it creates or opens.

use std::io;
use std::path::{Path, PathBuf};

use jj_lib::backend::{BackendError, CommitId};
use jj_lib::commit::Commit;
use jj_lib::file_util::PathError;
use jj_lib::index::IndexError;
use jj_lib::local_working_copy::LocalWorkingCopy;
use jj_lib::object_id::{HexPrefix, ObjectId as _, PrefixResolution};
use jj_lib::op_store::OperationId;
use jj_lib::ref_name::{RefName, WorkspaceNameBuf};
use jj_lib::repo::{CheckOutCommitError, EditCommitError, Repo as _, RepoLoaderError};
use jj_lib::transaction::TransactionCommitError;
use jj_lib::working_copy::WorkingCopyStateError;
use jj_lib::workspace::{Workspace, WorkspaceLoadError};
use jj_lib::workspace_store::WorkspaceStoreError;

use super::repo::{
    ChangeIdHex, ChangeSummary, CommitIdHex, OpenError, OperationIdHex, ReadError, Repo,
};
use crate::error::{Classify, ErrorKind, classify_by_source};

/// The prefix of every jj workspace name Reef creates, `reef-<id>`
/// (ADR 0014). Reef forgets only workspaces named with it.
pub const REEF_WORKSPACE_PREFIX: &str = "reef-";

/// Where a new workspace's working-copy commit starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartPoint {
    /// Edit this commit: it becomes the workspace's working-copy commit, so
    /// the workspace is on its change (`MutableRepo::edit`, as `jj edit`).
    Edit(CommitIdHex),
    /// A new empty commit on top of this one, on a new change
    /// (`MutableRepo::check_out`, as `jj new`).
    NewOn(CommitIdHex),
}

/// What [`Repo::resolve_revision`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The commit.
    pub commit_id: CommitIdHex,
    /// Its change.
    pub change_id: ChangeIdHex,
    /// Which kind of reference matched.
    pub via: ResolvedVia,
}

/// Which kind of reference [`Repo::resolve_revision`] matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvedVia {
    /// A change id prefix, in hex or in jj's reverse hex.
    ChangeId,
    /// A commit id prefix.
    CommitId,
    /// A local bookmark name.
    Bookmark,
}

/// Failure to resolve a revision.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResolveError {
    /// Nothing matched: no change or commit id prefix and no local bookmark.
    #[error("no change, commit, or bookmark matches {0:?}")]
    NoMatch(String),
    /// The text is a prefix of more than one change id, or of more than one
    /// commit id.
    #[error("id prefix {0:?} is ambiguous")]
    AmbiguousPrefix(String),
    /// The change has more than one visible commit.
    #[error("change {change_id} is divergent: it has {} visible commits", commits.len())]
    DivergentChange {
        /// The change.
        change_id: ChangeIdHex,
        /// Every visible commit of the change.
        commits: Vec<CommitIdHex>,
    },
    /// The bookmark points at more than one commit.
    #[error("bookmark {0:?} is conflicted")]
    ConflictedBookmark(String),
    /// The index could not be searched.
    #[error("failed to search the index")]
    Index(#[from] IndexError),
    /// A matched commit could not be read.
    #[error("failed to read a commit")]
    Backend(#[from] BackendError),
}

impl Classify for ResolveError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NoMatch(_) => ErrorKind::NotFound,
            Self::AmbiguousPrefix(_) => ErrorKind::InvalidArgument,
            Self::DivergentChange { .. } | Self::ConflictedBookmark(_) => ErrorKind::Conflict,
            Self::Index(error) => classify_by_source(error),
            Self::Backend(error) => classify_by_source(error),
        }
    }
}

/// Failure to read a commit the caller named by id.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CommitLookupError {
    /// The id is not as long as this repository's commit ids.
    #[error("{0} is not a commit id of this repository")]
    InvalidId(CommitIdHex),
    /// No commit has this id.
    #[error("commit {0} not found")]
    NotFound(CommitIdHex),
    /// The commit could not be read.
    #[error("failed to read commit {id}")]
    Backend {
        /// The commit.
        id: CommitIdHex,
        /// The underlying error.
        #[source]
        source: BackendError,
    },
}

impl Classify for CommitLookupError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::InvalidId(_) => ErrorKind::InvalidArgument,
            Self::NotFound(_) => ErrorKind::NotFound,
            Self::Backend { source, .. } => classify_by_source(source),
        }
    }
}

/// Failure to create a workspace.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CreateWorkspaceError {
    /// The view already has a working-copy commit for this workspace name.
    #[error("workspace {0} already exists")]
    WorkspaceExists(String),
    /// The workspace root does not exist or could not be resolved.
    #[error("failed to resolve workspace root {}", path.display())]
    Root {
        /// The root as given.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The state directory already exists. Nothing was written.
    #[error("working-copy state directory {} already exists", .0.display())]
    StateDirExists(PathBuf),
    /// The state directory could not be created.
    #[error("failed to create working-copy state directory {}", path.display())]
    StateDir {
        /// The state directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The starting commit could not be read.
    #[error(transparent)]
    Lookup(#[from] CommitLookupError),
    /// The working-copy state could not be written.
    #[error("failed to initialize the working-copy state")]
    Init(#[from] WorkingCopyStateError),
    /// A new working-copy commit could not be written on the starting point.
    #[error("failed to check out the starting commit")]
    CheckOut(#[from] CheckOutCommitError),
    /// The starting commit could not become the working-copy commit.
    #[error("failed to edit the starting commit")]
    Edit(#[from] EditCommitError),
    /// Rebasing after the change failed.
    #[error("failed to rebase descendants")]
    Backend(#[from] BackendError),
    /// The operation could not be committed.
    #[error("failed to commit the create-workspace operation")]
    Commit(#[from] TransactionCommitError),
    /// The workspace could not be registered in jj's workspace store.
    #[error("failed to register the workspace with jj")]
    WorkspaceStore(#[from] WorkspaceStoreError),
    /// The workspace root could not be canonicalized by jj.
    #[error("failed to open the workspace root")]
    Path(#[from] PathError),
}

impl Classify for CreateWorkspaceError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::WorkspaceExists(_) | Self::StateDirExists(_) => ErrorKind::AlreadyExists,
            Self::Root { source, .. } if source.kind() == io::ErrorKind::NotFound => {
                ErrorKind::NotFound
            }
            Self::Edit(EditCommitError::RewriteRootCommit(_))
            | Self::CheckOut(CheckOutCommitError::EditCommit(
                EditCommitError::RewriteRootCommit(_),
            )) => ErrorKind::Conflict,
            Self::Lookup(error) => error.kind(),
            Self::Root { source, .. } | Self::StateDir { source, .. } => classify_by_source(source),
            Self::Init(error) => classify_by_source(error),
            Self::CheckOut(error) => classify_by_source(error),
            Self::Edit(error) => classify_by_source(error),
            Self::Backend(error) => classify_by_source(error),
            Self::Commit(error) => classify_by_source(error),
            Self::WorkspaceStore(error) => classify_by_source(error),
            Self::Path(error) => classify_by_source(error),
        }
    }
}

/// Failure to forget a workspace.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ForgetWorkspaceError {
    /// The view has no working-copy commit for this workspace name.
    #[error("workspace {0} not found")]
    NoSuchWorkspace(String),
    /// The name does not start with [`REEF_WORKSPACE_PREFIX`]: it is the
    /// user's `default` or another workspace Reef did not create, which Reef
    /// never forgets.
    #[error("cannot forget workspace {0}, which is not a Reef workspace")]
    NotReefWorkspace(String),
    /// The name is this [`Repo`]'s own workspace.
    #[error("cannot forget workspace {0}, which this repository was opened from")]
    CurrentWorkspace(String),
    /// The working-copy commit could not be abandoned.
    #[error("failed to remove the working-copy commit")]
    Edit(#[from] EditCommitError),
    /// Rebasing after the change failed.
    #[error("failed to rebase descendants")]
    Backend(#[from] BackendError),
    /// The operation could not be committed.
    #[error("failed to commit the forget-workspace operation")]
    Commit(#[from] TransactionCommitError),
    /// The workspace could not be removed from jj's workspace store.
    #[error("failed to unregister the workspace with jj")]
    WorkspaceStore(#[from] WorkspaceStoreError),
}

impl Classify for ForgetWorkspaceError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NoSuchWorkspace(_) => ErrorKind::NotFound,
            Self::NotReefWorkspace(_) | Self::CurrentWorkspace(_) => ErrorKind::InvalidArgument,
            Self::Edit(error) => classify_by_source(error),
            Self::Backend(error) => classify_by_source(error),
            Self::Commit(error) => classify_by_source(error),
            Self::WorkspaceStore(error) => classify_by_source(error),
        }
    }
}

/// Failure to reset a workspace's working-copy state.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResetError {
    /// The commit could not be read.
    #[error(transparent)]
    Lookup(#[from] CommitLookupError),
    /// The working-copy state could not be locked or saved.
    #[error("failed to lock or save the working-copy state")]
    WorkingCopyState(#[from] WorkingCopyStateError),
    /// The working-copy state could not be reset.
    #[error("failed to reset the working-copy state")]
    Reset(#[from] jj_lib::working_copy::ResetError),
}

impl Classify for ResetError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Lookup(error) => error.kind(),
            Self::WorkingCopyState(error) => classify_by_source(error),
            Self::Reset(error) => classify_by_source(error),
        }
    }
}

impl Repo {
    /// Resolves `text` as a change id prefix, then a commit id prefix, then a
    /// local bookmark name, as the daemon's `change` and `new_on` fields do.
    /// There is no revset language.
    ///
    /// A change id prefix may be hex or jj's reverse hex (`k` to `z`, as
    /// `jj log` prints). Text that is a prefix of both a change id and a
    /// commit id resolves as the change, as jj prefers it. A prefix of more
    /// than one change, or of none but more than one commit, is
    /// [`ResolveError::AmbiguousPrefix`] and is not tried as a bookmark. A
    /// change with several visible commits is
    /// [`ResolveError::DivergentChange`]; a change with only hidden commits
    /// does not match. Commit id prefixes match hidden commits too.
    ///
    /// # Warning
    ///
    /// A bookmark whose name is all hex digits (`cafe`), or all letters `k`
    /// to `z` (`works`), is tried as an id prefix first, so it resolves to
    /// the matching change or commit if there is one, and an ambiguous
    /// prefix is an error without falling back to the bookmark. The brief
    /// (R1.01) fixes this order; such a bookmark is reachable only while no
    /// id starts with its name.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn resolve_revision(&self, text: &str) -> Result<Resolved, ResolveError> {
        self.resolve_revision_unlogged(text)
            .await
            .inspect_err(|error| crate::log_failure!(error))
    }

    async fn resolve_revision_unlogged(&self, text: &str) -> Result<Resolved, ResolveError> {
        if !text.is_empty() {
            // The two alphabets are disjoint, so at most one of these parses.
            let hex = HexPrefix::try_from_hex(text);
            let reverse_hex = HexPrefix::try_from_reverse_hex(text);
            if let Some(prefix) = hex.as_ref().or(reverse_hex.as_ref())
                && let Some(resolved) = self.resolve_change_prefix(text, prefix).await?
            {
                return Ok(resolved);
            }
            if let Some(prefix) = &hex {
                match self.repo.index().resolve_commit_id_prefix(prefix).await? {
                    PrefixResolution::SingleMatch(id) => {
                        let commit = self.repo.store().get_commit_async(&id).await?;
                        return Ok(resolved(&commit, ResolvedVia::CommitId));
                    }
                    PrefixResolution::AmbiguousMatch => {
                        return Err(ResolveError::AmbiguousPrefix(text.to_owned()));
                    }
                    PrefixResolution::NoMatch => {}
                }
            }
        }
        let target = self.repo.view().get_local_bookmark(RefName::new(text));
        if target.is_absent() {
            return Err(ResolveError::NoMatch(text.to_owned()));
        }
        let id = target
            .as_normal()
            .ok_or_else(|| ResolveError::ConflictedBookmark(text.to_owned()))?;
        let commit = self.repo.store().get_commit_async(id).await?;
        Ok(resolved(&commit, ResolvedVia::Bookmark))
    }

    /// The visible commit of the one change matching `prefix`, or `None` if
    /// no change, or only hidden commits, match.
    async fn resolve_change_prefix(
        &self,
        text: &str,
        prefix: &HexPrefix,
    ) -> Result<Option<Resolved>, ResolveError> {
        let targets = match self.repo.resolve_change_id_prefix(prefix).await? {
            PrefixResolution::NoMatch => return Ok(None),
            PrefixResolution::AmbiguousMatch => {
                return Err(ResolveError::AmbiguousPrefix(text.to_owned()));
            }
            PrefixResolution::SingleMatch(targets) => targets,
        };
        let Some(visible) = targets.into_visible() else {
            return Ok(None);
        };
        let commit = self.repo.store().get_commit_async(&visible[0]).await?;
        if visible.len() > 1 {
            return Err(ResolveError::DivergentChange {
                change_id: ChangeIdHex::of(commit.change_id()),
                commits: visible.iter().map(CommitIdHex::of).collect(),
            });
        }
        Ok(Some(resolved(&commit, ResolvedVia::ChangeId)))
    }

    /// Creates a jj workspace named `name` whose tree is at `root` (which must
    /// exist; its contents are not touched) and whose working-copy state is
    /// at `state_dir` (created, with any missing parents; it must not
    /// exist). Records one operation with `description`. Returns a
    /// [`Repo`] for the new workspace, sharing this repository's loader, and
    /// the operation's id.
    ///
    /// This [`Repo`] adopts the new operation, as [`Repo::snapshot`] does.
    /// The working copy starts with the empty tree recorded, whatever is on
    /// disk: the caller seeds its state from a base (R1.05) or calls
    /// [`Repo::reset_to`].
    ///
    /// If writing the working-copy state or registering the workspace
    /// fails after the operation is committed, the state directory is
    /// removed but the operation stands: the view keeps a working-copy
    /// commit for `name`, and the caller cleans up with
    /// [`Repo::forget_workspace`]. A failure before the operation leaves
    /// nothing behind.
    #[tracing::instrument(
        skip_all,
        fields(repo = %self.workspace_root().display(), path = %root.display())
    )]
    pub async fn create_workspace(
        &mut self,
        name: &str,
        root: &Path,
        state_dir: &Path,
        start: StartPoint,
        description: &str,
    ) -> Result<(Repo, OperationIdHex), CreateWorkspaceError> {
        let (repo, op_id) = self
            .create_workspace_unlogged(name, root, state_dir, start, description)
            .await
            .inspect_err(|error| crate::log_failure!(error))?;
        tracing::info!(op_id = %op_id, "workspace created");
        Ok((repo, op_id))
    }

    async fn create_workspace_unlogged(
        &mut self,
        name: &str,
        root: &Path,
        state_dir: &Path,
        start: StartPoint,
        description: &str,
    ) -> Result<(Repo, OperationIdHex), CreateWorkspaceError> {
        let name = WorkspaceNameBuf::from(name);
        if self.repo.view().get_wc_commit_id(&name).is_some() {
            return Err(CreateWorkspaceError::WorkspaceExists(
                name.as_str().to_owned(),
            ));
        }
        // jj's workspace store wants the root absolute and normalized like the
        // repository path, and `Workspace::new` canonicalizes it the same way
        // (with `dunce`, which differs from `std::fs::canonicalize` only on
        // Windows).
        let root = std::fs::canonicalize(root).map_err(|source| CreateWorkspaceError::Root {
            path: root.to_owned(),
            source,
        })?;
        let (target, new_on) = match &start {
            StartPoint::Edit(id) => (id, false),
            StartPoint::NewOn(id) => (id, true),
        };
        let commit = self.commit_by_id(target).await?;
        create_state_dir(state_dir)?;

        let committed = async {
            let mut tx = self.repo.start_transaction();
            if new_on {
                tx.repo_mut().check_out(name.clone(), &commit).await?;
            } else {
                tx.repo_mut().edit(name.clone(), &commit).await?;
            }
            tx.repo_mut().rebase_descendants().await?;
            Ok::<_, CreateWorkspaceError>(tx.commit(description).await?)
        }
        .await;
        self.repo = match committed {
            Ok(new_repo) => new_repo,
            Err(error) => {
                // Nothing has been written into it yet, so it is still empty.
                std::fs::remove_dir(state_dir).ok();
                return Err(error);
            }
        };

        let op_id = self.repo.op_id().clone();
        let attached = self.attach_new_workspace(&name, &root, state_dir, &op_id);
        let workspace = match attached {
            Ok(workspace) => workspace,
            Err(error) => {
                // The operation stands; only the half-written state goes.
                std::fs::remove_dir_all(state_dir).ok();
                return Err(error);
            }
        };
        let repo = Repo {
            workspace,
            repo: self.repo.clone(),
        };
        Ok((repo, OperationIdHex::of(&op_id)))
    }

    /// The steps of [`Repo::create_workspace`] after its operation: the
    /// working-copy state, the jj `Workspace`, and its registration.
    fn attach_new_workspace(
        &self,
        name: &WorkspaceNameBuf,
        root: &Path,
        state_dir: &Path,
        op_id: &OperationId,
    ) -> Result<Workspace, CreateWorkspaceError> {
        let working_copy = LocalWorkingCopy::init(
            self.repo.store().clone(),
            root.to_owned(),
            state_dir.to_owned(),
            op_id.clone(),
            name.clone(),
            self.workspace.settings(),
        )?;
        let workspace = Workspace::new(
            root,
            self.repo_path().to_owned(),
            Box::new(working_copy),
            self.workspace.repo_loader().clone(),
        )?;
        self.repo
            .loader()
            .workspace_store()
            .add(name, workspace.workspace_root())?;
        Ok(workspace)
    }

    /// Reopens a workspace made by [`Repo::create_workspace`], after a
    /// restart, from its root and state directory. The returned [`Repo`] is
    /// loaded at this one's operation; [`Repo::begin_snapshot`] decides what
    /// to do if its working copy was last updated at another (ADR 0018).
    #[tracing::instrument(
        skip_all,
        fields(repo = %self.workspace_root().display(), path = %root.display())
    )]
    pub fn open_workspace(&self, root: &Path, state_dir: &Path) -> Result<Repo, OpenError> {
        self.open_workspace_unlogged(root, state_dir)
            .inspect_err(|error| crate::log_failure!(error))
    }

    fn open_workspace_unlogged(&self, root: &Path, state_dir: &Path) -> Result<Repo, OpenError> {
        if !state_dir.join("checkout").is_file() {
            return Err(WorkspaceLoadError::NoWorkspaceHere(state_dir.to_owned()).into());
        }
        // As in `create_workspace`: one canonical root for the working copy and
        // the workspace. `std::fs::canonicalize` and the `dunce` crate jj uses
        // in `Workspace::new` differ only on Windows (UNC prefixes).
        let root = std::fs::canonicalize(root).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                WorkspaceLoadError::NoWorkspaceHere(root.to_owned())
            } else {
                WorkspaceLoadError::Path(PathError {
                    path: root.to_owned(),
                    source,
                })
            }
        })?;
        let working_copy = LocalWorkingCopy::load(
            self.repo.store().clone(),
            root.clone(),
            state_dir.to_owned(),
            self.workspace.settings(),
        )
        .map_err(WorkspaceLoadError::from)?;
        let workspace = Workspace::new(
            &root,
            self.repo_path().to_owned(),
            Box::new(working_copy),
            self.workspace.repo_loader().clone(),
        )
        .map_err(WorkspaceLoadError::from)?;
        Ok(Repo {
            workspace,
            repo: self.repo.clone(),
        })
    }

    /// Reloads this workspace's repository at the operation its working
    /// copy was last updated at, as recorded in its state directory.
    /// Returns that operation. The daemon does not need this since R1.13:
    /// it loads every workspace at head before a snapshot and lets the
    /// staleness policy decide (ADR 0018).
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn reload_at_working_copy_op(&mut self) -> Result<OperationIdHex, OpenError> {
        let op_id = self.workspace.working_copy().operation_id().clone();
        let loaded = async {
            let loader = self.repo.loader();
            let operation = loader
                .load_operation(&op_id)
                .await
                .map_err(RepoLoaderError::from)?;
            Ok::<_, OpenError>(loader.load_at(&operation).await?)
        }
        .await
        .inspect_err(|error| crate::log_failure!(error))?;
        self.repo = loaded;
        Ok(OperationIdHex::of(&op_id))
    }

    /// The working-copy commit of this workspace in the loaded view, with
    /// its change id.
    pub fn wc_change(&self) -> Result<ChangeSummary, ReadError> {
        let commit = self.wc_commit()?;
        Ok(ChangeSummary {
            change_id: ChangeIdHex::of(commit.change_id()),
            commit_id: CommitIdHex::of(commit.id()),
            description: commit.description().to_owned(),
            is_wc: true,
        })
    }

    /// The names of the workspaces whose working-copy commit is `commit` in
    /// the loaded view, sorted. A commit id that is not one of this
    /// repository's has none.
    pub fn workspaces_on_commit(&self, commit: &CommitIdHex) -> Vec<String> {
        let Some(id) = CommitId::try_from_hex(commit.as_str()) else {
            return Vec::new();
        };
        let mut names: Vec<String> = self
            .repo
            .view()
            .workspaces_for_wc_commit_id(&id)
            .into_iter()
            .map(|name| name.as_str().to_owned())
            .collect();
        names.sort();
        names
    }

    /// The parents of the commit with this full id, in order.
    pub async fn commit_parents(
        &self,
        commit: &CommitIdHex,
    ) -> Result<Vec<CommitIdHex>, CommitLookupError> {
        let commit = self.commit_by_id(commit).await?;
        Ok(commit.parent_ids().iter().map(CommitIdHex::of).collect())
    }

    /// `jj workspace forget`: removes the workspace's working-copy pointer
    /// (abandoning its working-copy commit if that is empty, undescribed, a
    /// head, and not referenced elsewhere) and its entry in jj's workspace
    /// store. Records one operation with `description`, which this [`Repo`]
    /// adopts. The workspace's directory and state directory are not
    /// touched. Only names starting with [`REEF_WORKSPACE_PREFIX`] can be
    /// forgotten, whichever workspace's [`Repo`] calls this, so the user's
    /// `default` is safe.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn forget_workspace(
        &mut self,
        name: &str,
        description: &str,
    ) -> Result<OperationIdHex, ForgetWorkspaceError> {
        let op_id = self
            .forget_workspace_unlogged(name, description)
            .await
            .inspect_err(|error| crate::log_failure!(error))?;
        tracing::info!(op_id = %op_id, "workspace forgotten");
        Ok(op_id)
    }

    async fn forget_workspace_unlogged(
        &mut self,
        name: &str,
        description: &str,
    ) -> Result<OperationIdHex, ForgetWorkspaceError> {
        if !name.starts_with(REEF_WORKSPACE_PREFIX) {
            return Err(ForgetWorkspaceError::NotReefWorkspace(name.to_owned()));
        }
        if name == self.workspace_name() {
            return Err(ForgetWorkspaceError::CurrentWorkspace(name.to_owned()));
        }
        let name = WorkspaceNameBuf::from(name);
        if self.repo.view().get_wc_commit_id(&name).is_none() {
            return Err(ForgetWorkspaceError::NoSuchWorkspace(
                name.as_str().to_owned(),
            ));
        }
        let mut tx = self.repo.start_transaction();
        tx.repo_mut().remove_workspace(&name).await?;
        tx.repo_mut().rebase_descendants().await?;
        self.repo = tx.commit(description).await?;
        self.repo.loader().workspace_store().forget(&[&name])?;
        Ok(OperationIdHex::of(self.repo.op_id()))
    }

    /// Records in the working-copy state that the files on disk are
    /// `commit`'s tree, without reading them: jj marks every file's state
    /// unknown (`LockedWorkingCopy::reset`), so the next snapshot rehashes
    /// every file and, if they match, records no operation. The fallback for
    /// seeding a new workspace when no base `tree_state` can be copied.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn reset_to(&mut self, commit: &CommitIdHex) -> Result<(), ResetError> {
        self.reset_to_unlogged(commit)
            .await
            .inspect_err(|error| crate::log_failure!(error))
    }

    async fn reset_to_unlogged(&mut self, commit: &CommitIdHex) -> Result<(), ResetError> {
        let commit = self.commit_by_id(commit).await?;
        let op_id = self.repo.op_id().clone();
        let mut locked_ws = self.workspace.start_working_copy_mutation().await?;
        locked_ws.locked_wc().reset(&commit).await?;
        locked_ws.finish(op_id).await?;
        Ok(())
    }

    /// Reads the commit with this full id.
    pub(crate) async fn commit_by_id(&self, id: &CommitIdHex) -> Result<Commit, CommitLookupError> {
        let commit_id = CommitId::try_from_hex(id.as_str())
            .filter(|commit_id| commit_id.as_bytes().len() == self.repo.store().commit_id_length())
            .ok_or_else(|| CommitLookupError::InvalidId(id.clone()))?;
        match self.repo.store().get_commit_async(&commit_id).await {
            Ok(commit) => Ok(commit),
            Err(BackendError::ObjectNotFound { .. }) => {
                Err(CommitLookupError::NotFound(id.clone()))
            }
            Err(source) => Err(CommitLookupError::Backend {
                id: id.clone(),
                source,
            }),
        }
    }
}

fn resolved(commit: &Commit, via: ResolvedVia) -> Resolved {
    Resolved {
        commit_id: CommitIdHex::of(commit.id()),
        change_id: ChangeIdHex::of(commit.change_id()),
        via,
    }
}

/// Creates `state_dir`, and its parents if needed, refusing one that exists.
fn create_state_dir(state_dir: &Path) -> Result<(), CreateWorkspaceError> {
    let failed = |source| CreateWorkspaceError::StateDir {
        path: state_dir.to_owned(),
        source,
    };
    if let Some(parent) = state_dir.parent() {
        std::fs::create_dir_all(parent).map_err(failed)?;
    }
    match std::fs::create_dir(state_dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Err(CreateWorkspaceError::StateDirExists(state_dir.to_owned()))
        }
        Err(error) => Err(failed(error)),
    }
}
