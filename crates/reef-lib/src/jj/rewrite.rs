//! The write side over commits (R2.03, ADR 0031): describing and
//! abandoning a commit, with the checks every such write makes first.
//!
//! A write resolves its revision with [`writable`], which refuses what it
//! must not rewrite (an immutable or hidden commit, or one other than the
//! caller expected) and lists the jj workspaces the write moves, then runs
//! [`Repo::describe_commit`] or [`Repo::abandon_commit`] in one operation.

use jj_lib::backend::{BackendError, CommitId};
use jj_lib::commit::Commit;
use jj_lib::index::IndexError;
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::revset::RevsetExpression;
use jj_lib::rewrite::{RebaseOptions, RewriteRefsOptions};
use jj_lib::transaction::TransactionCommitError;

use super::log::Describer;
use super::oplog::OpMeta;
use super::repo::{ChangeIdHex, CommitIdHex, OperationIdHex, Repo};
use super::revset::{self, RevisionError, RevsetScope};
use crate::error::{Classify, ErrorKind, classify_by_source};

/// A commit a write may rewrite, as [`writable`] found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Writable {
    /// The commit.
    pub commit_id: CommitIdHex,
    /// Its change.
    pub change_id: ChangeIdHex,
    /// The change has more than one visible commit.
    pub divergent: bool,
    /// The jj workspaces whose working-copy commit is the commit or one of
    /// its descendants: those a describe or an abandon moves, sorted.
    pub workspaces: Vec<String>,
}

/// What [`Repo::describe_commit`] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Described {
    /// The operation.
    pub op_id: OperationIdHex,
    /// The rewritten commit.
    pub commit_id: CommitIdHex,
}

/// What [`Repo::abandon_commit`] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Abandoned {
    /// The operation.
    pub op_id: OperationIdHex,
    /// For one commit of a divergent change, the commit that took over its
    /// descendants, bookmarks and working copies; `None` for a plain
    /// abandon.
    pub successor: Option<CommitIdHex>,
}

/// Failure of a describe or an abandon.
#[derive(Debug, thiserror::Error)]
pub enum RewriteError {
    /// The revision did not resolve to one commit.
    #[error("resolve the revision")]
    Revision(#[from] RevisionError),
    /// `expected_commit_id` is not a full commit id in lowercase hex.
    #[error("{0:?} is not a full commit id in lowercase hex")]
    InvalidCommitId(String),
    /// The revision names another commit than the caller expected.
    #[error("the revision names commit {actual}, not the expected {expected}")]
    Mismatch {
        /// What the caller expected.
        expected: CommitIdHex,
        /// What the revision names now.
        actual: CommitIdHex,
    },
    /// The commit is in `immutable()`.
    #[error("commit {0} is immutable")]
    Immutable(CommitIdHex),
    /// The commit is not visible at head; rewriting it would bring it back.
    #[error("commit {0} is hidden; rewriting it would make it visible again")]
    Hidden(CommitIdHex),
    /// A description with a NUL.
    #[error("a description cannot contain a NUL")]
    InvalidDescription,
    /// Reading or writing a commit failed.
    #[error("failed to read or write a commit")]
    Backend(#[from] BackendError),
    /// The index could not answer.
    #[error("failed to query the index")]
    Index(#[from] IndexError),
    /// Committing the operation failed.
    #[error("failed to commit the operation")]
    Commit(#[from] TransactionCommitError),
}

impl Classify for RewriteError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Revision(error) => error.kind(),
            Self::InvalidCommitId(_) | Self::InvalidDescription => ErrorKind::InvalidArgument,
            Self::Mismatch { .. } | Self::Immutable(_) | Self::Hidden(_) => ErrorKind::Conflict,
            Self::Backend(error) => classify_by_source(error),
            Self::Index(error) => classify_by_source(error),
            Self::Commit(error) => classify_by_source(error),
        }
    }
}

/// `text` as an expected commit id: `None` when empty; otherwise a full
/// commit id of `repo`'s store in lowercase hex, else
/// [`RewriteError::InvalidCommitId`]. Prefixes are refused: they would
/// weaken the check.
pub fn parse_expected(
    repo: &ReadonlyRepo,
    text: &str,
) -> Result<Option<CommitIdHex>, RewriteError> {
    if text.is_empty() {
        return Ok(None);
    }
    let full = repo.store().commit_id_length() * 2;
    match text.parse::<CommitIdHex>() {
        Ok(id) if text.len() == full => Ok(Some(id)),
        _ => Err(RewriteError::InvalidCommitId(text.to_owned())),
    }
}

/// A description as Reef stores it: with a trailing newline when it is
/// not empty and lacks one, as jj's CLI stores descriptions. A NUL is
/// [`RewriteError::InvalidDescription`].
pub fn normalize_description(text: &str) -> Result<String, RewriteError> {
    if text.contains('\0') {
        return Err(RewriteError::InvalidDescription);
    }
    let mut text = text.to_owned();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

/// Resolves `revision` in `repo` for a describe or an abandon: exactly one
/// commit (R2.02's resolution), `expected` if given, mutable, and visible.
pub async fn writable(
    repo: &ReadonlyRepo,
    revision: &str,
    expected: Option<&CommitIdHex>,
    scope: &RevsetScope<'_>,
) -> Result<Writable, RewriteError> {
    let resolved = revset::resolve_revision(repo, revision, scope).await?;
    if let Some(expected) = expected
        && *expected != resolved.commit_id
    {
        return Err(RewriteError::Mismatch {
            expected: expected.clone(),
            actual: resolved.commit_id,
        });
    }
    let info = Describer::new(repo, scope)?
        .describe(&resolved.commit_id)
        .await?;
    if info.immutable {
        return Err(RewriteError::Immutable(resolved.commit_id));
    }
    if info.hidden {
        return Err(RewriteError::Hidden(resolved.commit_id));
    }
    let workspaces = workspaces_on_descendants(repo, &resolved.commit_id.to_id()).await?;
    Ok(Writable {
        commit_id: resolved.commit_id,
        change_id: resolved.change_id,
        divergent: info.divergent,
        workspaces,
    })
}

/// Resolves `revision` in `repo` as the parent of a new change (R2.04):
/// [`writable`] without the immutability check, since building on trunk
/// is the usual case. A hidden commit is refused: building on it would make
/// it visible again. `workspaces` are those whose working-copy commit is
/// the commit itself, sorted.
pub async fn buildable(
    repo: &ReadonlyRepo,
    revision: &str,
    expected: Option<&CommitIdHex>,
    scope: &RevsetScope<'_>,
) -> Result<Writable, RewriteError> {
    let resolved = revset::resolve_revision(repo, revision, scope).await?;
    if let Some(expected) = expected
        && *expected != resolved.commit_id
    {
        return Err(RewriteError::Mismatch {
            expected: expected.clone(),
            actual: resolved.commit_id,
        });
    }
    let info = Describer::new(repo, scope)?
        .describe(&resolved.commit_id)
        .await?;
    if info.hidden {
        return Err(RewriteError::Hidden(resolved.commit_id));
    }
    let workspaces = workspaces_at(repo, &resolved.commit_id);
    Ok(Writable {
        commit_id: resolved.commit_id,
        change_id: resolved.change_id,
        divergent: info.divergent,
        workspaces,
    })
}

/// The jj workspaces whose working-copy commit is `commit` itself, sorted
/// (the view keeps them sorted).
pub fn workspaces_at(repo: &ReadonlyRepo, commit: &CommitIdHex) -> Vec<String> {
    let id = commit.to_id();
    repo.view()
        .wc_commit_ids()
        .iter()
        .filter(|(_, wc)| **wc == id)
        .map(|(name, _)| name.as_str().to_owned())
        .collect()
}

/// The jj workspaces whose working-copy commit is `commit` or descends
/// from it, sorted (the view keeps them sorted).
async fn workspaces_on_descendants(
    repo: &ReadonlyRepo,
    commit: &CommitId,
) -> Result<Vec<String>, IndexError> {
    let mut names = Vec::new();
    for (name, wc) in repo.view().wc_commit_ids() {
        if repo.index().is_ancestor(commit, wc).await? {
            names.push(name.as_str().to_owned());
        }
    }
    Ok(names)
}

/// `workspaces` (jj names) ordered descendants first: one whose
/// working-copy commit descends from another's comes before it. Snapshotted
/// in this order, each commit is rewritten before a snapshot of one of its
/// ancestors rebases it, which would make its workspace stale with edits,
/// and diverge (ADR 0018). Names not in the view are left out.
pub async fn descendants_first(
    repo: &ReadonlyRepo,
    workspaces: &[String],
) -> Result<Vec<String>, RewriteError> {
    let view = repo.view();
    let commits: Vec<(&String, &CommitId)> = workspaces
        .iter()
        .filter_map(|name| Some((name, view.get_wc_commit_id(WorkspaceName::new(name))?)))
        .collect();
    let mut keyed = Vec::with_capacity(commits.len());
    for (name, commit) in &commits {
        // How many of the others' commits are proper ancestors of this one:
        // a descendant always has more than its ancestor.
        let mut ancestors = 0usize;
        for (_, other) in &commits {
            if other != commit && repo.index().is_ancestor(other, commit).await? {
                ancestors += 1;
            }
        }
        keyed.push((std::cmp::Reverse(ancestors), (*name).clone()));
    }
    keyed.sort();
    Ok(keyed.into_iter().map(|(_, name)| name).collect())
}

/// One working copy in a view, as a write's follow-up needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkingCopy {
    /// The jj workspace.
    pub workspace: String,
    /// Its working-copy commit.
    pub commit_id: CommitIdHex,
    /// That commit's change.
    pub change_id: ChangeIdHex,
    /// That commit's tree, as comparable text.
    pub tree: String,
    /// The change has more than one visible commit.
    pub divergent: bool,
}

/// Every working copy in `repo`'s view, by jj workspace name.
pub async fn working_copies(repo: &ReadonlyRepo) -> Result<Vec<WorkingCopy>, RewriteError> {
    let mut copies = Vec::new();
    for (name, id) in repo.view().wc_commit_ids() {
        let commit = repo.store().get_commit_async(id).await?;
        let divergent = repo
            .resolve_change_id(commit.change_id())
            .await
            .map_err(|error| RewriteError::Revision(RevisionError::Other(error.into())))?
            .is_some_and(|targets| targets.is_divergent());
        copies.push(WorkingCopy {
            workspace: name.as_str().to_owned(),
            commit_id: CommitIdHex::of(commit.id()),
            change_id: ChangeIdHex::of(commit.change_id()),
            tree: commit
                .tree_ids()
                .iter()
                .map(ObjectId::hex)
                .collect::<Vec<_>>()
                .join(","),
            divergent,
        });
    }
    Ok(copies)
}

impl Repo {
    /// Sets `commit`'s description to `text` ([`normalize_description`]d)
    /// in one operation recorded with `meta`, which this [`Repo`] adopts;
    /// `None`, writing nothing, when the description is already that.
    /// Descendants are rebased onto the new commit with their trees
    /// unchanged; bookmarks and working copies follow. The author stays;
    /// the committer is this repository's identity, now. The caller checks
    /// the commit with [`writable`] first.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), commit_id = %commit))]
    pub async fn describe_commit(
        &mut self,
        commit: &CommitIdHex,
        text: &str,
        meta: &OpMeta,
    ) -> Result<Option<Described>, RewriteError> {
        let text = normalize_description(text)?;
        let old = self.repo.store().get_commit_async(&commit.to_id()).await?;
        if old.description() == text {
            return Ok(None);
        }
        let mut tx = self.repo.start_transaction();
        let new = tx
            .repo_mut()
            .rewrite_commit(&old)
            .set_description(text)
            .write()
            .await?;
        tx.repo_mut().rebase_descendants().await?;
        self.repo = meta.commit(tx).await?;
        Ok(Some(Described {
            op_id: OperationIdHex::of(self.repo.op_id()),
            commit_id: CommitIdHex::of(new.id()),
        }))
    }

    /// Abandons `commit` in one operation recorded with `meta`, which this
    /// [`Repo`] adopts. The caller checks the commit with [`writable`]
    /// first.
    ///
    /// When `commit`'s change has another visible commit that does not
    /// descend from it, this resolves the divergence: that commit (the
    /// newest by committer time, then by commit id, when there are several)
    /// takes over, as if `commit` had been rewritten into it. Descendants
    /// are rebased onto it, bookmarks move to it, and working copies edit
    /// it. Otherwise it is `jj abandon`: descendants are rebased onto the
    /// commit's parents, local bookmarks on it are deleted, and working
    /// copies on it share one new empty commit on its parents.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), commit_id = %commit))]
    pub async fn abandon_commit(
        &mut self,
        commit: &CommitIdHex,
        meta: &OpMeta,
    ) -> Result<Abandoned, RewriteError> {
        let old = self.repo.store().get_commit_async(&commit.to_id()).await?;
        let successor = self.successor(&old).await?;
        let mut tx = self.repo.start_transaction();
        if let Some(successor) = &successor {
            tx.repo_mut()
                .set_rewritten_commit(old.id().clone(), successor.id().clone());
            tx.repo_mut().rebase_descendants().await?;
        } else {
            tx.repo_mut().record_abandoned_commit(&old);
            let options = RebaseOptions {
                rewrite_refs: RewriteRefsOptions {
                    delete_abandoned_bookmarks: true,
                },
                ..RebaseOptions::default()
            };
            tx.repo_mut()
                .rebase_descendants_with_options(&RevsetExpression::none(), &options, |_, _| {})
                .await?;
        }
        self.repo = meta.commit(tx).await?;
        Ok(Abandoned {
            op_id: OperationIdHex::of(self.repo.op_id()),
            successor: successor.map(|commit| CommitIdHex::of(commit.id())),
        })
    }

    /// The commit that takes `abandoned`'s place when its change is
    /// divergent: the change's other visible commits that do not descend
    /// from it, newest by committer time, then highest commit id.
    async fn successor(&self, abandoned: &Commit) -> Result<Option<Commit>, RewriteError> {
        let repo = &self.repo;
        let targets = repo
            .resolve_change_id(abandoned.change_id())
            .await
            .map_err(|error| RewriteError::Revision(RevisionError::Other(error.into())))?;
        let Some(targets) = targets else {
            return Ok(None);
        };
        let mut candidates = Vec::new();
        for (_, id) in targets.visible_with_offsets() {
            if id == abandoned.id() || repo.index().is_ancestor(abandoned.id(), id).await? {
                continue;
            }
            candidates.push(repo.store().get_commit_async(id).await?);
        }
        Ok(candidates.into_iter().max_by(|a, b| {
            let time = |commit: &Commit| commit.committer().timestamp.timestamp;
            time(a)
                .cmp(&time(b))
                .then_with(|| a.id().hex().cmp(&b.id().hex()))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_get_a_trailing_newline() {
        assert_eq!(normalize_description("msg").unwrap(), "msg\n");
        assert_eq!(normalize_description("msg\n").unwrap(), "msg\n");
        assert_eq!(normalize_description("").unwrap(), "");
        assert_eq!(
            normalize_description("two\n\nlines").unwrap(),
            "two\n\nlines\n"
        );
        assert!(matches!(
            normalize_description("a\0b"),
            Err(RewriteError::InvalidDescription)
        ));
    }
}
