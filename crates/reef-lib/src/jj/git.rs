//! Bringing a colocated repository's git refs into jj's view (R1.05).
//!
//! In a colocated repository, `git branch`, `git commit`, and `git fetch`
//! move git refs that jj sees only after importing them, as the `jj` CLI
//! does before every command. The daemon imports before it resolves a
//! revision a client names, so a branch made with git resolves as a
//! bookmark.

use jj_lib::backend::BackendError;
use jj_lib::config::ConfigGetError;
use jj_lib::git::{GitImportError, GitImportOptions, GitSettings};
use jj_lib::repo::Repo as _;
use jj_lib::transaction::TransactionCommitError;

use super::oplog::OpMeta;
use super::repo::{OperationIdHex, Repo};
use crate::error::{Classify, ErrorKind, classify_by_source};

/// What [`Repo::import_git_refs`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportRefsOutcome {
    /// The operation recording the import, or `None` when git's refs
    /// already matched jj's view and nothing was written.
    pub op_id: Option<OperationIdHex>,
    /// Commits abandoned because git no longer reaches them.
    pub abandoned_commits: usize,
    /// Git refs that could not be imported, by full name.
    pub failed_refs: Vec<String>,
}

/// Failure to import git refs.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ImportRefsError {
    /// The repository's `git.*` settings could not be read.
    #[error("invalid git settings")]
    Settings(#[from] ConfigGetError),
    /// Reading git's refs or the commits they point at failed.
    #[error("failed to import git refs")]
    Import(#[from] GitImportError),
    /// Rebasing descendants of abandoned commits failed.
    #[error("failed to rebase descendants")]
    Backend(#[from] BackendError),
    /// The operation could not be committed.
    #[error("failed to commit the import operation")]
    Commit(#[from] TransactionCommitError),
}

impl Classify for ImportRefsError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Settings(_) | Self::Import(GitImportError::UnexpectedBackend(_)) => {
                ErrorKind::InvalidArgument
            }
            Self::Import(error) => classify_by_source(error),
            Self::Backend(error) => classify_by_source(error),
            Self::Commit(error) => classify_by_source(error),
        }
    }
}

impl Repo {
    /// Imports the git repository's refs into jj's view in a transaction
    /// recorded with `meta`, committed only if it changes anything,
    /// and adopts the new operation, as the `jj` CLI does before each
    /// command in a colocated repository. Local branches become local
    /// bookmarks; remote-tracking bookmarks are not auto-tracked. Only
    /// meaningful for a repository with a git store (see
    /// [`Repo::is_colocated`]); otherwise
    /// [`ImportRefsError::Import`] with an unexpected-backend error.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn import_git_refs(
        &mut self,
        meta: &OpMeta,
    ) -> Result<ImportRefsOutcome, ImportRefsError> {
        let outcome = self
            .import_git_refs_unlogged(meta)
            .await
            .inspect_err(|error| crate::log_failure!(error))?;
        if let Some(op_id) = &outcome.op_id {
            tracing::info!(
                %op_id,
                abandoned_count = outcome.abandoned_commits,
                "git refs imported"
            );
        }
        if !outcome.failed_refs.is_empty() {
            tracing::warn!(
                failed_ref_count = outcome.failed_refs.len(),
                "some git refs could not be imported; ImportRefsOutcome::failed_refs names them"
            );
        }
        Ok(outcome)
    }

    async fn import_git_refs_unlogged(
        &mut self,
        meta: &OpMeta,
    ) -> Result<ImportRefsOutcome, ImportRefsError> {
        let git_settings = GitSettings::from_settings(self.workspace.settings())?;
        let options = GitImportOptions {
            abandon_unreachable_commits: git_settings.abandon_unreachable_commits,
            record_synthetic_predecessors: git_settings.record_synthetic_predecessors,
            remote_auto_track_bookmarks: std::collections::HashMap::new(),
        };
        let mut tx = self.repo.start_transaction();
        let stats = jj_lib::git::import_refs(tx.repo_mut(), &options).await?;
        let failed_refs = stats
            .failed_ref_names
            .iter()
            .map(ToString::to_string)
            .collect();
        if !tx.repo().has_changes() {
            return Ok(ImportRefsOutcome {
                op_id: None,
                abandoned_commits: 0,
                failed_refs,
            });
        }
        tx.repo_mut().rebase_descendants().await?;
        self.repo = meta.commit(tx).await?;
        Ok(ImportRefsOutcome {
            op_id: Some(OperationIdHex::of(self.repo.op_id())),
            abandoned_commits: stats.abandoned_commits.len(),
            failed_refs,
        })
    }
}

impl Repo {
    /// The branch the git remote `remote` names as its default: the target
    /// of `refs/remotes/<remote>/HEAD`, which `git clone` sets, without the
    /// `refs/remotes/<remote>/` prefix (`main`). `None` if the repository has
    /// no git store, the ref is missing or not symbolic, or reading it fails
    /// (logged at `warn`): a default to try, never an error.
    pub fn git_remote_head(&self, remote: &str) -> Option<String> {
        remote_head(self.repo.store(), remote)
    }
}

/// [`Repo::git_remote_head`] over a store, so a read at any operation can
/// find it (R2.02's `trunk()`).
pub(crate) fn remote_head(store: &jj_lib::store::Store, remote: &str) -> Option<String> {
    let git = jj_lib::git::get_git_repo(store).ok()?;
    let name = format!("refs/remotes/{remote}/HEAD");
    let reference = match git.try_find_reference(name.as_str()) {
        Ok(reference) => reference?,
        Err(error) => {
            tracing::warn!(
                error = %crate::error::ErrorChain(&error),
                "cannot read {name}; not using it as a default"
            );
            return None;
        }
    };
    let gix::refs::TargetRef::Symbolic(target) = reference.target() else {
        return None;
    };
    let prefix = format!("refs/remotes/{remote}/");
    let branch = target.as_bstr().strip_prefix(prefix.as_bytes())?;
    std::str::from_utf8(branch).ok().map(str::to_owned)
}
