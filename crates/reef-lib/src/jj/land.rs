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
use jj_lib::backend::{BackendError, CommitId, TreeId};
use jj_lib::commit::Commit;
use jj_lib::index::IndexError;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefName;
use jj_lib::repo::{CheckOutCommitError, MutableRepo};
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::revset::RevsetExpression;
use jj_lib::rewrite::{RebaseOptions, RewriteRefsOptions, merge_commit_trees, rebase_commit};
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
    /// A landed tree is not the one the batch planned and tested
    /// (R3.14): nothing was written.
    #[error("change {change} would land as tree {actual}, not the planned {planned}")]
    PlanMismatch {
        /// The change.
        change: ChangeIdHex,
        /// The tree the batch planned and tested.
        planned: String,
        /// The tree the landing would write.
        actual: String,
    },
    /// A tree id is not one of this repository's.
    #[error("no tree {0} in the store")]
    NoTree(String),
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
            LandError::Resolve(_) | LandError::PlanMismatch { .. } | LandError::NoTree(_) => {
                ErrorKind::Internal
            }
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

/// One change of a chain, as [`preview_chain`] found it (R3.14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainMember {
    /// Whether it is in the chain: false when rebasing it onto the earlier
    /// members conflicts, though it merges onto the destination alone.
    pub included: bool,
    /// Whether it is rebased; false only for a first member whose only
    /// parent is the destination's head.
    pub rebased: bool,
    /// The chain's tree with it applied (the destination and every
    /// included member up to it); `None` when excluded.
    pub tree: Option<String>,
    /// The conflicted paths against the chain, when excluded.
    pub conflicts: Vec<String>,
    /// How many there are.
    pub conflicts_total: usize,
    /// Paths it and the destination since its parents, or an earlier
    /// member, both changed; sorted, at most [`MAX_OVERLAP_PATHS`].
    pub overlap: Vec<String>,
    /// How many there are.
    pub overlap_total: usize,
}

/// A chain of changes rebased one on another onto a bookmark's head, as
/// [`preview_chain`] found it (R3.14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainPreview {
    /// The destination's head.
    pub destination: CommitIdHex,
    /// Each target, in order.
    pub members: Vec<ChainMember>,
}

/// What landing `targets` together on their bookmark (all the same) would
/// do: each rebased onto the previous included one, the first onto the
/// bookmark's head. A target that conflicts with the chain is left out and
/// the chain goes on without it. Each target is checked as [`preview`]
/// checks it. Writes objects to the store, but no operation.
pub async fn preview_chain(
    repo: &Arc<ReadonlyRepo>,
    bookmark: &str,
    targets: &[LandTarget],
) -> Result<ChainPreview, LandError> {
    let destination = destination_head(repo, bookmark)?;
    let mut tx = repo.start_transaction();
    let mut tip = repo.store().get_commit_async(&destination.to_id()).await?;
    let mut members = Vec::with_capacity(targets.len());
    for target in targets {
        let commit = checked_commit(repo, target, &destination).await?;
        let rebased = commit.parent_ids() != [tip.id().clone()];
        let landed = if rebased {
            rebase_commit(tx.repo_mut(), commit.clone(), vec![tip.id().clone()]).await?
        } else {
            commit.clone()
        };
        if landed.has_conflict() {
            let (conflicts, conflicts_total) = conflicted_paths(&landed).await;
            members.push(ChainMember {
                included: false,
                rebased,
                tree: None,
                conflicts,
                conflicts_total,
                overlap: Vec::new(),
                overlap_total: 0,
            });
            continue;
        }
        let (overlap, overlap_total) = if rebased {
            overlap_with(repo, &commit, &tip).await?
        } else {
            (Vec::new(), 0)
        };
        members.push(ChainMember {
            included: true,
            rebased,
            tree: landed.tree_ids().as_resolved().map(ObjectId::hex),
            conflicts: Vec::new(),
            conflicts_total: 0,
            overlap,
            overlap_total,
        });
        tip = landed;
    }
    Ok(ChainPreview {
        destination,
        members,
    })
}

/// The conflicted paths of `commit`, at most [`MAX_CONFLICT_PATHS`], and
/// how many there are.
async fn conflicted_paths(commit: &Commit) -> (Vec<String>, usize) {
    let mut conflicts = Vec::new();
    let mut total = 0usize;
    let mut stream = commit.tree().conflicts();
    while let Some((path, _)) = stream.next().await {
        total += 1;
        if conflicts.len() < MAX_CONFLICT_PATHS {
            conflicts.push(path.as_internal_file_string().to_owned());
        }
    }
    (conflicts, total)
}

/// The paths both `commit` (against its parents) and `onto` (against the
/// same parents) changed.
async fn overlap_with(
    repo: &Arc<ReadonlyRepo>,
    commit: &Commit,
    onto: &Commit,
) -> Result<(Vec<String>, usize), LandError> {
    let mut parents = Vec::new();
    for id in commit.parent_ids() {
        parents.push(repo.store().get_commit_async(id).await?);
    }
    let base = merge_commit_trees(repo.as_ref(), &parents).await?;
    let (ours, _) = changed(&base, &commit.tree()).await;
    let (theirs, _) = changed(&base, &onto.tree()).await;
    let both: Vec<String> = ours.intersection(&theirs).cloned().collect();
    let total = both.len();
    Ok((both.into_iter().take(MAX_OVERLAP_PATHS).collect(), total))
}

/// A trial commit [`Repo::write_trial`] wrote (R3.14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trial {
    /// The operation.
    pub op_id: OperationIdHex,
    /// The commit.
    pub commit: CommitIdHex,
    /// Its change, a new one.
    pub change: ChangeIdHex,
}

/// One change of a [`LandPlan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedMember {
    /// The change at its queued commit.
    pub target: LandTarget,
    /// The tree it must land as, when a batch planned and tested one.
    pub tree: Option<String>,
}

/// What [`Repo::land_chain`] lands: changes in order, each rebased onto
/// the previous, the first onto `destination`, which `bookmark` must still
/// point at, and the trial commits to abandon in the same operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandPlan {
    /// The destination bookmark.
    pub bookmark: String,
    /// Its head when the plan was judged.
    pub destination: CommitIdHex,
    /// The changes, in order.
    pub members: Vec<PlannedMember>,
    /// Trial commits to abandon, if still visible.
    pub trials: Vec<CommitIdHex>,
}

/// One landed change of [`LandedChain`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandedMember {
    /// The commit it landed as.
    pub commit: CommitIdHex,
    /// Its git tree id.
    pub tree: Option<String>,
    /// Whether it was rebased.
    pub rebased: bool,
}

/// What [`Repo::land_chain`] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandedChain {
    /// The operation.
    pub op_id: OperationIdHex,
    /// Each member, in order.
    pub members: Vec<LandedMember>,
    /// The jj workspaces given a new working-copy commit on a landed one.
    pub new_working_copies: Vec<String>,
}

/// Whether `commit` is its change's visible commit in `repo`.
async fn visible(repo: &ReadonlyRepo, commit: &Commit) -> Result<bool, LandError> {
    Ok(repo
        .resolve_change_id(commit.change_id())
        .await
        .map_err(|error| LandError::Resolve(error.to_string()))?
        .is_some_and(|targets| {
            targets
                .visible_with_offsets()
                .any(|(_, id)| id == commit.id())
        }))
}

/// `commit`'s change when the commit is that change's visible commit in
/// `repo` (R3.14); `None` otherwise, or for one the store does not have.
pub async fn visible_change(
    repo: &ReadonlyRepo,
    commit: &CommitIdHex,
) -> Result<Option<ChangeIdHex>, LandError> {
    match repo.store().get_commit_async(&commit.to_id()).await {
        Ok(commit) => Ok(visible(repo, &commit)
            .await?
            .then(|| ChangeIdHex::of(commit.change_id()))),
        Err(BackendError::ObjectNotFound { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

impl Repo {
    /// Writes a trial commit for a batch (R3.14): a new change on
    /// `destination` whose tree is `tree`, described `description`, in one
    /// operation recorded with `meta`.
    pub async fn write_trial(
        &mut self,
        destination: &CommitIdHex,
        tree: &str,
        description: &str,
        meta: &OpMeta,
    ) -> Result<Trial, LandError> {
        let repo = self.readonly();
        let parent = repo.store().get_commit_async(&destination.to_id()).await?;
        let tree_id =
            TreeId::try_from_hex(tree).ok_or_else(|| LandError::NoTree(tree.to_owned()))?;
        match repo
            .store()
            .get_tree(jj_lib::repo_path::RepoPathBuf::root(), &tree_id)
            .await
        {
            Ok(_) => {}
            Err(BackendError::ObjectNotFound { .. }) => {
                return Err(LandError::NoTree(tree.to_owned()));
            }
            Err(error) => return Err(error.into()),
        }
        let mut tx = repo.start_transaction();
        let commit = tx
            .repo_mut()
            .new_commit(
                vec![parent.id().clone()],
                MergedTree::resolved(repo.store().clone(), tree_id),
            )
            .set_description(description)
            .write()
            .await?;
        self.repo = meta.commit(tx).await?;
        Ok(Trial {
            op_id: OperationIdHex::of(self.repo.op_id()),
            commit: CommitIdHex::of(commit.id()),
            change: ChangeIdHex::of(commit.change_id()),
        })
    }

    /// Abandons those of `trials` still visible (R3.14), in one operation
    /// recorded with `meta`; `None` when none was.
    pub async fn end_trials(
        &mut self,
        trials: &[CommitIdHex],
        meta: &OpMeta,
    ) -> Result<Option<OperationIdHex>, LandError> {
        let repo = self.readonly();
        let mut tx = repo.start_transaction();
        if !abandon_visible(&repo, tx.repo_mut(), trials).await? {
            return Ok(None);
        }
        self.repo = meta.commit(tx).await?;
        Ok(Some(OperationIdHex::of(self.repo.op_id())))
    }

    /// Lands `target` on its bookmark, which must still point at
    /// `destination`, in one operation recorded with `meta`, which this
    /// [`Repo`] adopts. Each of `meta.transitions` with no commit names the
    /// landed one. The caller refreshes first and has previewed.
    pub async fn land(
        &mut self,
        target: &LandTarget,
        destination: &CommitIdHex,
        meta: &OpMeta,
    ) -> Result<Landed, LandError> {
        let plan = LandPlan {
            bookmark: target.bookmark.clone(),
            destination: destination.clone(),
            members: vec![PlannedMember {
                target: target.clone(),
                tree: None,
            }],
            trials: Vec::new(),
        };
        let landed = self.land_chain(&plan, meta).await?;
        let member = landed
            .members
            .into_iter()
            .next()
            .ok_or_else(|| LandError::Resolve("nothing landed".to_owned()))?;
        Ok(Landed {
            op_id: landed.op_id,
            commit: member.commit,
            tree: member.tree,
            rebased: member.rebased,
            new_working_copies: landed.new_working_copies,
        })
    }

    /// Lands `plan` in one operation recorded with `meta`, which this
    /// [`Repo`] adopts (R3.13, R3.14): each change rebased onto the
    /// previous (the first onto the destination's head; none for a
    /// fast-forward), refused unless each lands as its planned tree, every
    /// jj workspace on a landed commit given a new empty working-copy commit
    /// on it, the plan's trial commits abandoned, and the bookmark set to
    /// the last landed commit. Each of `meta.transitions` with no commit
    /// names its change's landed commit. The caller refreshes first.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), members_count = plan.members.len()))]
    pub async fn land_chain(
        &mut self,
        plan: &LandPlan,
        meta: &OpMeta,
    ) -> Result<LandedChain, LandError> {
        let repo = self.readonly();
        let actual = destination_head(&repo, &plan.bookmark)?;
        if actual != plan.destination {
            return Err(LandError::DestinationMoved {
                bookmark: plan.bookmark.clone(),
                expected: plan.destination.clone(),
                actual,
            });
        }
        let mut tx = repo.start_transaction();
        let mut tip = plan.destination.to_id();
        let mut pairs: Vec<(Commit, Commit, bool)> = Vec::with_capacity(plan.members.len());
        for member in &plan.members {
            let commit = checked_commit(&repo, &member.target, &plan.destination).await?;
            let rebased = commit.parent_ids() != [tip.clone()];
            let landed = if rebased {
                rebase_commit(tx.repo_mut(), commit.clone(), vec![tip.clone()]).await?
            } else {
                commit.clone()
            };
            if landed.has_conflict() {
                return Err(LandError::RebaseConflicts);
            }
            let tree = landed.tree_ids().as_resolved().map(ObjectId::hex);
            if let Some(planned) = &member.tree
                && tree.as_ref() != Some(planned)
            {
                return Err(LandError::PlanMismatch {
                    change: member.target.change.clone(),
                    planned: planned.clone(),
                    actual: tree.unwrap_or_default(),
                });
            }
            tip = landed.id().clone();
            pairs.push((commit, landed, rebased));
        }
        let Some((_, last, _)) = pairs.last().cloned() else {
            return Err(LandError::Resolve("nothing to land".to_owned()));
        };
        let abandoned = abandon_visible(&repo, tx.repo_mut(), &plan.trials).await?;
        if !abandoned && pairs.iter().any(|(_, _, rebased)| *rebased) {
            tx.repo_mut().rebase_descendants().await?;
        }
        let mut new_working_copies = Vec::new();
        for (commit, landed, _) in &pairs {
            let on_landed: Vec<_> = tx
                .repo()
                .view()
                .wc_commit_ids()
                .iter()
                .filter(|(_, id)| *id == landed.id() || *id == commit.id())
                .map(|(name, _)| name.clone())
                .collect();
            for name in on_landed {
                new_working_copies.push(name.as_str().to_owned());
                tx.repo_mut().check_out(name, landed).await?;
            }
        }
        tx.repo_mut().set_local_bookmark_target(
            RefName::new(&plan.bookmark),
            RefTarget::normal(last.id().clone()),
        );
        let mut meta = meta.clone();
        for transition in &mut meta.transitions {
            if !transition.commit.is_empty() {
                continue;
            }
            let landed = plan
                .members
                .iter()
                .zip(&pairs)
                .find(|(member, _)| member.target.change.as_str() == transition.change)
                .or_else(|| (plan.members.len() == 1).then(|| (&plan.members[0], &pairs[0])));
            if let Some((_, (_, landed, _))) = landed {
                CommitIdHex::of(landed.id())
                    .as_str()
                    .clone_into(&mut transition.commit);
            }
        }
        self.repo = meta.commit(tx).await?;
        Ok(LandedChain {
            op_id: OperationIdHex::of(self.repo.op_id()),
            members: pairs
                .iter()
                .map(|(_, landed, rebased)| LandedMember {
                    commit: CommitIdHex::of(landed.id()),
                    tree: landed.tree_ids().as_resolved().map(ObjectId::hex),
                    rebased: *rebased,
                })
                .collect(),
            new_working_copies,
        })
    }
}

/// Records each of `trials` that is still its change's visible commit as
/// abandoned in `mut_repo`, then rebases descendants (deleting bookmarks
/// on them); whether any was.
async fn abandon_visible(
    repo: &ReadonlyRepo,
    mut_repo: &mut MutableRepo,
    trials: &[CommitIdHex],
) -> Result<bool, LandError> {
    let mut any = false;
    for trial in trials {
        let commit = match repo.store().get_commit_async(&trial.to_id()).await {
            Ok(commit) => commit,
            Err(BackendError::ObjectNotFound { .. }) => continue,
            Err(error) => return Err(error.into()),
        };
        if visible(repo, &commit).await? {
            mut_repo.record_abandoned_commit(&commit);
            any = true;
        }
    }
    if any {
        let options = RebaseOptions {
            rewrite_refs: RewriteRefsOptions {
                delete_abandoned_bookmarks: true,
            },
            ..RebaseOptions::default()
        };
        mut_repo
            .rebase_descendants_with_options(&RevsetExpression::none(), &options, |_, _| {})
            .await?;
    }
    Ok(any)
}
