//! Landing a change on a local bookmark (R3.13, ADR 0064).
//!
//! [`preview`] answers, writing nothing the view keeps, what landing a
//! change's commit on a bookmark's head would do: a fast-forward when the
//! head is the commit's only parent, else a rebase onto the head, which
//! either merges cleanly ("commutes") or conflicts, and the paths both the
//! change and the destination changed since the change's parents (the
//! overlap). [`Repo::land`] does it in one operation: the rebase and its
//! descendants, a new empty working-copy commit for every workspace on the
//! landed commit (so a later snapshot there cannot move the bookmark past
//! the gate), the bookmark set to the landed commit, and the landing's
//! lifecycle transitions in [`OpMeta::transitions`].

use std::collections::BTreeSet;
use std::sync::Arc;

use futures::StreamExt as _;
use jj_lib::backend::{BackendError, CommitId};
use jj_lib::commit::Commit;
use jj_lib::index::IndexError;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefName;
use jj_lib::repo::{CheckOutCommitError, MutableRepo};
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::rewrite::{merge_commit_trees, rebase_commit};
use jj_lib::transaction::TransactionCommitError;

use super::oplog::OpMeta;
use super::repo::{ChangeIdHex, CommitIdHex, OperationIdHex, Repo};
use crate::error::{Classify, ErrorKind, classify_by_source};

/// The most conflicted paths a preview lists.
pub const MAX_CONFLICT_PATHS: usize = 100;
/// The most overlapping paths a preview lists.
pub const MAX_OVERLAP_PATHS: usize = 1_000;
/// The most paths a preview compares on each side before it stops
/// counting the overlap.
pub const MAX_COMPARED_PATHS: usize = 100_000;

/// What to land: a change at the commit it was queued at, on a local
/// bookmark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandTarget {
    /// The change.
    pub change: ChangeIdHex,
    /// The commit it was queued at; the only one that may land.
    pub commit: CommitIdHex,
    /// The destination: a local bookmark.
    pub bookmark: String,
}

/// What landing would do, as [`preview`] found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    /// The destination's head: the commit the change lands on.
    pub destination: CommitIdHex,
    /// Whether the change is rebased onto it; false for a fast-forward.
    pub rebased: bool,
    /// The landed tree's git id, when it has a single one (no conflict).
    pub landed_tree: Option<String>,
    /// The conflicted paths of the rebased tree, at most
    /// [`MAX_CONFLICT_PATHS`]; empty when the rebase commutes.
    pub conflicts: Vec<String>,
    /// How many conflicted paths there are.
    pub conflicts_total: usize,
    /// Paths both the change and the destination changed since the
    /// change's parents, sorted, at most [`MAX_OVERLAP_PATHS`].
    pub overlap: Vec<String>,
    /// How many there are (a lower bound when a side passed
    /// [`MAX_COMPARED_PATHS`]).
    pub overlap_total: usize,
}

impl Preview {
    /// Whether the rebase conflicts.
    pub fn conflicted(&self) -> bool {
        self.conflicts_total > 0
    }
}

/// What [`Repo::land`] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Landed {
    /// The operation.
    pub op_id: OperationIdHex,
    /// The commit now at the destination: the queued commit for a
    /// fast-forward, its rebase otherwise (same change).
    pub commit: CommitIdHex,
    /// Its git tree id.
    pub tree: Option<String>,
    /// Whether the change was rebased.
    pub rebased: bool,
    /// The jj workspaces given a new working-copy commit on the landed
    /// commit.
    pub new_working_copies: Vec<String>,
}

/// Why a landing cannot be previewed or written.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LandError {
    /// The bookmark does not exist.
    #[error("no local bookmark {0:?}")]
    DestinationNotFound(String),
    /// The bookmark is conflicted.
    #[error("bookmark {0:?} is conflicted")]
    DestinationConflicted(String),
    /// The bookmark moved since the preview.
    #[error("bookmark {bookmark:?} moved to {actual} since it was read at {expected}")]
    DestinationMoved {
        /// The bookmark.
        bookmark: String,
        /// Where it was.
        expected: CommitIdHex,
        /// Where it is.
        actual: CommitIdHex,
    },
    /// The change's visible commit is not the queued one, or it has
    /// several.
    #[error("change {change} is no longer only at commit {commit}")]
    Amended {
        /// The change.
        change: ChangeIdHex,
        /// The queued commit.
        commit: CommitIdHex,
    },
    /// A parent of the commit is not on the destination.
    #[error("commit {commit}'s parent {parent} is not on the destination")]
    UnlandedParents {
        /// The commit.
        commit: CommitIdHex,
        /// The first parent not on the destination.
        parent: CommitIdHex,
    },
    /// The commit itself has conflicts.
    #[error("commit {0} has conflicts")]
    Conflicted(CommitIdHex),
    /// The commit is already on the destination: the bookmark's head is it
    /// or one of its descendants.
    #[error("commit {0} is already on the destination")]
    AlreadyOnDestination(CommitIdHex),
    /// The rebase onto the destination conflicts (at write time).
    #[error("rebasing onto the destination conflicts")]
    RebaseConflicts,
    /// The store failed.
    #[error("read the repository")]
    Backend(#[from] BackendError),
    /// The index failed.
    #[error("read the index")]
    Index(#[from] IndexError),
    /// A workspace could not be given a new working-copy commit.
    #[error("check out a new working-copy commit")]
    CheckOut(#[from] CheckOutCommitError),
    /// The change could not be resolved.
    #[error("resolve the change: {0}")]
    Resolve(String),
    /// The operation could not be written.
    #[error("record the landing")]
    Commit(#[from] TransactionCommitError),
}

impl Classify for LandError {
    fn kind(&self) -> ErrorKind {
        match self {
            LandError::DestinationNotFound(_) => ErrorKind::NotFound,
            LandError::DestinationConflicted(_)
            | LandError::DestinationMoved { .. }
            | LandError::Amended { .. }
            | LandError::UnlandedParents { .. }
            | LandError::Conflicted(_)
            | LandError::AlreadyOnDestination(_)
            | LandError::RebaseConflicts => ErrorKind::Conflict,
            LandError::Resolve(_) => ErrorKind::Internal,
            other => classify_by_source(other),
        }
    }
}

/// The commit `bookmark` points at in `repo`.
pub fn destination_head(repo: &ReadonlyRepo, bookmark: &str) -> Result<CommitIdHex, LandError> {
    let target = repo.view().get_local_bookmark(RefName::new(bookmark));
    if target.is_absent() {
        return Err(LandError::DestinationNotFound(bookmark.to_owned()));
    }
    target
        .as_normal()
        .map(CommitIdHex::of)
        .ok_or_else(|| LandError::DestinationConflicted(bookmark.to_owned()))
}

/// The commit `target` names, checked: it is its change's only visible
/// commit and has no conflict, and each parent is an ancestor of
/// `destination`.
async fn checked_commit(
    repo: &ReadonlyRepo,
    target: &LandTarget,
    destination: &CommitIdHex,
) -> Result<Commit, LandError> {
    let amended = || LandError::Amended {
        change: target.change.clone(),
        commit: target.commit.clone(),
    };
    let visible: Vec<CommitId> = repo
        .resolve_change_id(&target.change.to_id())
        .await
        .map_err(|error| LandError::Resolve(error.to_string()))?
        .map(|targets| {
            targets
                .visible_with_offsets()
                .map(|(_, id)| id.clone())
                .collect()
        })
        .unwrap_or_default();
    if visible != [target.commit.to_id()] {
        return Err(amended());
    }
    let commit = repo
        .store()
        .get_commit_async(&target.commit.to_id())
        .await?;
    if commit.has_conflict() {
        return Err(LandError::Conflicted(target.commit.clone()));
    }
    let destination_id = destination.to_id();
    // Rebasing a commit onto itself or a descendant would make a cycle.
    if repo
        .index()
        .is_ancestor(commit.id(), &destination_id)
        .await?
    {
        return Err(LandError::AlreadyOnDestination(target.commit.clone()));
    }
    for parent in commit.parent_ids() {
        if !repo.index().is_ancestor(parent, &destination_id).await? {
            return Err(LandError::UnlandedParents {
                commit: target.commit.clone(),
                parent: CommitIdHex::of(parent),
            });
        }
    }
    Ok(commit)
}

/// The paths that differ from `from` to `to`, at most [`MAX_COMPARED_PATHS`],
/// and whether the walk stopped early.
async fn changed(from: &MergedTree, to: &MergedTree) -> (BTreeSet<String>, bool) {
    let mut stream = from.diff(to, &EverythingMatcher);
    let mut paths = BTreeSet::new();
    while let Some(entry) = stream.next().await {
        if paths.len() >= MAX_COMPARED_PATHS {
            return (paths, true);
        }
        paths.insert(entry.path.as_internal_file_string().to_owned());
    }
    (paths, false)
}

/// The rebase of `commit` onto `destination` in `mut_repo`, or `commit`
/// itself when `destination` is its only parent.
async fn rebased(
    mut_repo: &mut MutableRepo,
    commit: &Commit,
    destination: &CommitIdHex,
) -> Result<(Commit, bool), LandError> {
    let destination_id = destination.to_id();
    if commit.parent_ids() == [destination_id.clone()] {
        return Ok((commit.clone(), false));
    }
    let new = rebase_commit(mut_repo, commit.clone(), vec![destination_id]).await?;
    Ok((new, true))
}

/// What landing `target` on its bookmark's head would do in `repo`; see the
/// module docs. Writes the rebased commit's objects to the store, but no
/// operation.
pub async fn preview(repo: &Arc<ReadonlyRepo>, target: &LandTarget) -> Result<Preview, LandError> {
    let destination = destination_head(repo, &target.bookmark)?;
    let commit = checked_commit(repo, target, &destination).await?;
    preview_onto(repo, &commit, destination).await
}

/// What landing `commit` on `destination` would do, with none of
/// [`preview`]'s checks: what `EvaluatePolicy` shows a policy as the
/// `landing` input. A commit already in `::destination` lands as it is.
pub async fn preview_commit(
    repo: &Arc<ReadonlyRepo>,
    commit: &CommitIdHex,
    destination: &CommitIdHex,
) -> Result<Preview, LandError> {
    let commit = repo.store().get_commit_async(&commit.to_id()).await?;
    if repo
        .index()
        .is_ancestor(commit.id(), &destination.to_id())
        .await?
    {
        return Ok(Preview {
            destination: destination.clone(),
            rebased: false,
            landed_tree: commit.tree_ids().as_resolved().map(ObjectId::hex),
            conflicts: Vec::new(),
            conflicts_total: 0,
            overlap: Vec::new(),
            overlap_total: 0,
        });
    }
    preview_onto(repo, &commit, destination.clone()).await
}

async fn preview_onto(
    repo: &Arc<ReadonlyRepo>,
    commit: &Commit,
    destination: CommitIdHex,
) -> Result<Preview, LandError> {
    let mut tx = repo.start_transaction();
    let (landed, rebased) = rebased(tx.repo_mut(), commit, &destination).await?;
    let mut conflicts = Vec::new();
    let mut conflicts_total = 0usize;
    let landed_tree = landed.tree();
    if landed.has_conflict() {
        let mut stream = landed_tree.conflicts();
        while let Some((path, _)) = stream.next().await {
            conflicts_total += 1;
            if conflicts.len() < MAX_CONFLICT_PATHS {
                conflicts.push(path.as_internal_file_string().to_owned());
            }
        }
    }
    let (overlap, overlap_total) = if rebased {
        let parents: Vec<Commit> = {
            let mut parents = Vec::new();
            for id in commit.parent_ids() {
                parents.push(repo.store().get_commit_async(id).await?);
            }
            parents
        };
        let base = merge_commit_trees(repo.as_ref(), &parents).await?;
        let head = repo.store().get_commit_async(&destination.to_id()).await?;
        let (ours, _) = changed(&base, &commit.tree()).await;
        let (theirs, _) = changed(&base, &head.tree()).await;
        let both: Vec<String> = ours.intersection(&theirs).cloned().collect();
        let total = both.len();
        (both.into_iter().take(MAX_OVERLAP_PATHS).collect(), total)
    } else {
        (Vec::new(), 0)
    };
    Ok(Preview {
        destination,
        rebased,
        landed_tree: landed.tree_ids().as_resolved().map(ObjectId::hex),
        conflicts,
        conflicts_total,
        overlap,
        overlap_total,
    })
}

impl Repo {
    /// Lands `target` on its bookmark, which must still point at
    /// `destination`, in one operation recorded with `meta`, which this
    /// [`Repo`] adopts. Each of `meta.transitions` with no commit names the
    /// landed one. The caller refreshes first and has previewed.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), commit_id = %target.commit))]
    pub async fn land(
        &mut self,
        target: &LandTarget,
        destination: &CommitIdHex,
        meta: &OpMeta,
    ) -> Result<Landed, LandError> {
        let repo = self.readonly();
        let actual = destination_head(&repo, &target.bookmark)?;
        if actual != *destination {
            return Err(LandError::DestinationMoved {
                bookmark: target.bookmark.clone(),
                expected: destination.clone(),
                actual,
            });
        }
        let commit = checked_commit(&repo, target, destination).await?;
        let mut tx = repo.start_transaction();
        let (landed, rebased) = rebased(tx.repo_mut(), &commit, destination).await?;
        if landed.has_conflict() {
            return Err(LandError::RebaseConflicts);
        }
        if rebased {
            tx.repo_mut().rebase_descendants().await?;
        }
        let on_landed: Vec<_> = tx
            .repo()
            .view()
            .wc_commit_ids()
            .iter()
            .filter(|(_, id)| *id == landed.id() || *id == commit.id())
            .map(|(name, _)| name.clone())
            .collect();
        let mut new_working_copies = Vec::with_capacity(on_landed.len());
        for name in on_landed {
            new_working_copies.push(name.as_str().to_owned());
            tx.repo_mut().check_out(name, &landed).await?;
        }
        tx.repo_mut().set_local_bookmark_target(
            RefName::new(&target.bookmark),
            RefTarget::normal(landed.id().clone()),
        );
        let landed_id = CommitIdHex::of(landed.id());
        let mut meta = meta.clone();
        for transition in &mut meta.transitions {
            if transition.commit.is_empty() {
                landed_id.as_str().clone_into(&mut transition.commit);
            }
        }
        self.repo = meta.commit(tx).await?;
        Ok(Landed {
            op_id: OperationIdHex::of(self.repo.op_id()),
            commit: landed_id,
            tree: landed.tree_ids().as_resolved().map(ObjectId::hex),
            rebased,
            new_working_copies,
        })
    }
}
