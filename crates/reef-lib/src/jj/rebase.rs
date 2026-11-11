//! Rebasing changes onto a destination (issue #259, ADR 0071): the set a
//! request moves, the checks it must pass, the destination it resolves to,
//! and the write, in one operation.
//!
//! [`plan`] resolves a request against a view and refuses what must not
//! move; [`Repo::rebase`] writes a plan with jj-lib's `move_commits`, which
//! keeps change ids and moves descendants, bookmarks and working copies
//! with the moved commits. The daemon plans twice, as every write does:
//! once before it locks the workspaces the rebase moves, and again on the
//! repository thread just before the write.
//!
//! A rebase records no lifecycle transition; checks that need the
//! daemon's lifecycle rows (a queued change) are the daemon's.

use std::collections::{BTreeSet, HashMap};

use futures::StreamExt as _;
use jj_lib::backend::{BackendError, CommitId};
use jj_lib::commit::Commit;
use jj_lib::index::IndexError;
use jj_lib::ref_name::{RefName, RemoteName, WorkspaceName, WorkspaceNameBuf};
use jj_lib::repo::{CheckOutCommitError, ReadonlyRepo, Repo as _};
use jj_lib::revset::{ResolvedRevsetExpression, RevsetEvaluationError};
use jj_lib::rewrite::{
    MoveCommitsLocation, MoveCommitsTarget, RebaseOptions, RebasedCommit, move_commits,
};
use jj_lib::transaction::TransactionCommitError;

use super::oplog::OpMeta;
use super::repo::{ChangeIdHex, CommitIdHex, OperationIdHex, Repo};
use super::revset::{self, RevisionError, RevsetScope, Trunk};
use crate::error::{Classify, ErrorKind, classify_by_source};

/// The most commits one rebase moves: the set and its descendants.
pub const MAX_MOVED: usize = 1_000;

/// The most commits [`behind_trunk`] counts.
pub const MAX_BEHIND: u32 = 1_000;

/// Which commits a rebase moves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RebaseMode {
    /// Every commit of the revision's ancestry the destination lacks, from
    /// its roots, with their descendants: `jj rebase -b`, `git rebase main`
    /// on a branch.
    #[default]
    Branch,
    /// The revision's commit and its descendants: `jj rebase -s`.
    Source,
}

/// A rebase request, as [`plan`] reads it.
#[derive(Clone, Debug)]
pub struct RebaseRequest<'a> {
    /// The revision: one commit.
    pub revision: &'a str,
    /// The destination revision; `None` for `trunk()`, or its remote
    /// bookmark when that is ahead (see [`destination`]).
    pub onto: Option<&'a str>,
    /// Which commits move.
    pub mode: RebaseMode,
    /// The commit `revision` must name, if given.
    pub expected: Option<&'a CommitIdHex>,
    /// The remote whose bookmark may stand for a local `trunk()` that is
    /// behind it; `None` for `origin`.
    pub remote: Option<&'a str>,
}

/// Where a rebase goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Destination {
    /// The commit.
    pub commit_id: CommitIdHex,
    /// What it resolved from: a bookmark (`main`, `main@origin`), or the
    /// revision given.
    pub name: String,
    /// A sentence for a person when the destination is not what `trunk()`
    /// names: the local bookmark left behind.
    pub note: Option<String>,
}

/// What [`plan`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebasePlan {
    /// The revision's commit.
    pub commit_id: CommitIdHex,
    /// Its change.
    pub change_id: ChangeIdHex,
    /// The destination.
    pub destination: Destination,
    /// What the rebase does.
    pub action: Action,
}

/// What a planned rebase does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing: every root already has the destination as its only parent,
    /// or the revision is already in the destination's ancestry.
    UpToDate,
    /// Move `roots` and their descendants onto the destination.
    Move {
        /// The roots of the set, in the index's order (newest first).
        roots: Vec<CommitIdHex>,
        /// Every commit that moves: the roots and their descendants.
        moved: Vec<CommitIdHex>,
        /// The jj workspaces whose working-copy commit moves, sorted.
        workspaces: Vec<String>,
    },
    /// The revision is the named workspace's working-copy commit, which has
    /// landed (is immutable): the workspace starts a new empty change on
    /// the destination instead.
    StartOn {
        /// The jj workspace.
        workspace: String,
    },
}

/// One change a rebase moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebasedChange {
    /// The change.
    pub change_id: ChangeIdHex,
    /// Its commit before.
    pub old_commit_id: CommitIdHex,
    /// Its commit after.
    pub new_commit_id: CommitIdHex,
    /// The new commit has a conflict.
    pub conflicted: bool,
    /// The rebase introduced it: a conflicted path no new parent carries,
    /// on a change that had none before.
    pub introduces_conflict: bool,
    /// The change was not empty and is now: its edits are already in the
    /// destination (a squash merge, typically).
    pub became_empty: bool,
}

/// What [`Repo::rebase`] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rebased {
    /// The operation; `None` when the plan was [`Action::UpToDate`].
    pub op_id: Option<OperationIdHex>,
    /// The revision's commit afterwards.
    pub commit_id: CommitIdHex,
    /// Every change moved, newest first.
    pub changes: Vec<RebasedChange>,
    /// For [`Action::StartOn`], the new working-copy commit.
    pub started: Option<CommitIdHex>,
}

/// Why a rebase is refused or failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RebaseError {
    /// The revision, or the destination, did not resolve to one commit.
    #[error("resolve the revision")]
    Revision(#[from] RevisionError),
    /// The revision names another commit than the caller expected.
    #[error("the revision names commit {actual}, not the expected {expected}")]
    Mismatch {
        /// What the caller expected.
        expected: CommitIdHex,
        /// What the revision names now.
        actual: CommitIdHex,
    },
    /// No destination given and no `trunk()`.
    #[error("the repository has no trunk() bookmark; name a destination with --onto")]
    NoDestination,
    /// The destination is in the set, or descends from it.
    #[error(
        "cannot rebase onto {destination}: it is one of the moved commits or descends from one"
    )]
    OntoDescendant {
        /// The destination commit.
        destination: CommitIdHex,
    },
    /// A commit of the set has landed (is immutable).
    #[error("commit {commit_id} is immutable (on {head})")]
    Immutable {
        /// The commit.
        commit_id: CommitIdHex,
        /// Its change.
        change_id: ChangeIdHex,
        /// What makes it immutable.
        head: String,
    },
    /// The revision is hidden.
    #[error("commit {0} is hidden; rebasing it would make it visible again")]
    Hidden(CommitIdHex),
    /// A change of the set has more than one visible commit.
    #[error("change {change_id} is divergent")]
    Divergent {
        /// The change.
        change_id: ChangeIdHex,
        /// One of its commits in the set.
        commit_id: CommitIdHex,
    },
    /// A commit of the set below the revision is another Reef workspace's
    /// working-copy commit (S-177).
    #[error("commit {commit_id} is workspace {workspace}'s working copy, below the change")]
    IncludesWorkingCopy {
        /// The commit.
        commit_id: CommitIdHex,
        /// Its change.
        change_id: ChangeIdHex,
        /// The jj workspace.
        workspace: String,
    },
    /// The set and its descendants are more than [`MAX_MOVED`] commits.
    #[error("the rebase would move {count} commits, more than {MAX_MOVED}")]
    TooLarge {
        /// How many.
        count: usize,
    },
    /// Reading or writing a commit failed.
    #[error("failed to read or write a commit")]
    Backend(#[from] BackendError),
    /// The index could not answer.
    #[error("failed to query the index")]
    Index(#[from] IndexError),
    /// Evaluating a revset failed.
    #[error("failed to evaluate the set")]
    Evaluation(#[from] RevsetEvaluationError),
    /// Starting a new change failed.
    #[error("failed to check out a new working-copy commit")]
    CheckOut(#[from] CheckOutCommitError),
    /// Committing the operation failed.
    #[error("failed to commit the operation")]
    Commit(#[from] TransactionCommitError),
}

impl Classify for RebaseError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Revision(error) => error.kind(),
            Self::OntoDescendant { .. } => ErrorKind::InvalidArgument,
            Self::Mismatch { .. }
            | Self::NoDestination
            | Self::Immutable { .. }
            | Self::Hidden(_)
            | Self::Divergent { .. }
            | Self::IncludesWorkingCopy { .. }
            | Self::TooLarge { .. } => ErrorKind::Conflict,
            Self::Backend(error) => classify_by_source(error),
            Self::Index(error) => classify_by_source(error),
            Self::Evaluation(error) => classify_by_source(error),
            Self::CheckOut(error) => classify_by_source(error),
            Self::Commit(error) => classify_by_source(error),
        }
    }
}

/// The destination of `request` in `repo`: `onto` when given; else
/// `trunk()`, or, when `trunk()` is a local bookmark whose remote bookmark
/// on `request.remote` (or `origin`) is a strict descendant of it, that
/// remote bookmark, with a note: a git user's local `main` is rarely
/// fast-forwarded, and Reef never moves it to get there.
pub async fn destination(
    repo: &ReadonlyRepo,
    request: &RebaseRequest<'_>,
    scope: &RevsetScope<'_>,
) -> Result<Destination, RebaseError> {
    if let Some(onto) = request.onto {
        let resolved = revset::resolve_revision(repo, onto, scope).await?;
        // Rebasing onto a hidden commit would make it visible again.
        let named = repo
            .store()
            .get_commit_async(&resolved.commit_id.to_id())
            .await?;
        if !visible(repo, &named).await? {
            return Err(RebaseError::Hidden(resolved.commit_id));
        }
        // A local bookmark its remote one is ahead of: say so, since a git
        // user's local `main` is rarely fast-forwarded.
        let remote = request.remote.unwrap_or(revset::DEFAULT_REMOTE);
        let symbol = RefName::new(onto).to_remote_symbol(RemoteName::new(remote));
        let mut note = None;
        if repo
            .view()
            .get_local_bookmark(RefName::new(onto))
            .is_present()
            && let Some(ahead) = repo.view().get_remote_bookmark(symbol).target.as_normal()
            && *ahead != resolved.commit_id.to_id()
            && repo
                .index()
                .is_ancestor(&resolved.commit_id.to_id(), ahead)
                .await?
        {
            note = Some(format!(
                "{onto}@{remote} is ahead of the local bookmark {onto}; `--onto {onto}@{remote}` rebases onto it"
            ));
        }
        return Ok(Destination {
            commit_id: resolved.commit_id,
            name: onto.to_owned(),
            note,
        });
    }
    default_destination(repo, request.remote)
        .await?
        .ok_or(RebaseError::NoDestination)
}

/// The destination a rebase with no `onto` goes to: `trunk()`, or its
/// remote bookmark on `remote` (or `origin`) when that is strictly ahead
/// of a local `trunk()`; `None` without a `trunk()`.
pub async fn default_destination(
    repo: &ReadonlyRepo,
    remote: Option<&str>,
) -> Result<Option<Destination>, RebaseError> {
    let Some(Trunk {
        commit_id,
        bookmark,
    }) = revset::trunk(repo)
    else {
        return Ok(None);
    };
    if !bookmark.contains('@') {
        let remote = remote.unwrap_or(revset::DEFAULT_REMOTE);
        let symbol = RefName::new(&bookmark).to_remote_symbol(RemoteName::new(remote));
        if let Some(ahead) = repo.view().get_remote_bookmark(symbol).target.as_normal()
            && *ahead != commit_id.to_id()
            && repo.index().is_ancestor(&commit_id.to_id(), ahead).await?
        {
            let name = format!("{bookmark}@{remote}");
            return Ok(Some(Destination {
                commit_id: CommitIdHex::of(ahead),
                note: Some(format!(
                    "rebased onto {name}; the local bookmark {bookmark} is behind it and was left as it is"
                )),
                name,
            }));
        }
    }
    Ok(Some(Destination {
        commit_id,
        name: bookmark,
        note: None,
    }))
}

/// Plans `request` in `repo`, for the jj workspace `workspace` (the
/// request's, which `@` names): resolves the revision and the destination,
/// computes the set, and refuses what must not move. A commit of the set
/// that is another Reef workspace's working copy (a `reef-` jj workspace
/// other than `workspace`) below the revision is refused in branch mode
/// (S-177); descendants always move with their workspaces.
pub async fn plan(
    repo: &ReadonlyRepo,
    request: &RebaseRequest<'_>,
    scope: &RevsetScope<'_>,
) -> Result<RebasePlan, RebaseError> {
    let resolved = revset::resolve_revision(repo, request.revision, scope).await?;
    if let Some(expected) = request.expected
        && *expected != resolved.commit_id
    {
        return Err(RebaseError::Mismatch {
            expected: expected.clone(),
            actual: resolved.commit_id,
        });
    }
    let commit = repo
        .store()
        .get_commit_async(&resolved.commit_id.to_id())
        .await?;
    if !visible(repo, &commit).await? {
        return Err(RebaseError::Hidden(resolved.commit_id));
    }
    let destination = destination(repo, request, scope).await?;
    let destination_id = destination.commit_id.to_id();
    let plan = |action| RebasePlan {
        commit_id: resolved.commit_id.clone(),
        change_id: resolved.change_id.clone(),
        destination: destination.clone(),
        action,
    };

    // A landed `@` (immutable, or already in trunk's ancestry): a new
    // change on the destination. Below an explicit `onto`, `@` is not
    // landed: the destination is above it (REBASE_ONTO_DESCENDANT).
    let own = scope.workspace;
    if let Some(own) = own
        && repo.view().get_wc_commit_id(WorkspaceName::new(own)) == Some(commit.id())
        && ((request.onto.is_none()
            && repo
                .index()
                .is_ancestor(commit.id(), &destination_id)
                .await?)
            || revset::immutable_head(repo, &resolved.commit_id)
                .await?
                .is_some())
    {
        return Ok(plan(Action::StartOn {
            workspace: own.to_owned(),
        }));
    }

    let roots: Vec<CommitId> = match request.mode {
        RebaseMode::Branch => {
            let expression = ResolvedRevsetExpression::commits(vec![destination_id.clone()])
                .range(&ResolvedRevsetExpression::commits(vec![
                    commit.id().clone(),
                ]))
                .roots();
            collect(repo, &expression).await?
        }
        RebaseMode::Source => vec![commit.id().clone()],
    };
    if roots.is_empty() {
        // The revision is in the destination's ancestry.
        return Ok(plan(Action::UpToDate));
    }
    for root in &roots {
        if repo.index().is_ancestor(root, &destination_id).await? {
            return Err(RebaseError::OntoDescendant {
                destination: destination.commit_id.clone(),
            });
        }
    }
    let mut up_to_date = true;
    for root in &roots {
        let root_commit = repo.store().get_commit_async(root).await?;
        if root_commit.parent_ids() != std::slice::from_ref(&destination_id) {
            up_to_date = false;
        }
        if let Some(head) = revset::immutable_head(repo, &CommitIdHex::of(root)).await? {
            return Err(RebaseError::Immutable {
                commit_id: CommitIdHex::of(root),
                change_id: ChangeIdHex::of(root_commit.change_id()),
                head: head.to_string(),
            });
        }
    }
    if up_to_date {
        return Ok(plan(Action::UpToDate));
    }
    let moved_expression = ResolvedRevsetExpression::commits(roots.clone()).descendants();
    let moved = collect_capped(repo, &moved_expression, MAX_MOVED).await?;
    let below = if request.mode == RebaseMode::Branch {
        below_revision(repo, &roots, commit.id()).await?
    } else {
        BTreeSet::new()
    };
    refuse_divergent(repo, &moved).await?;
    let workspaces = moved_workspaces(repo, &moved, &below, own).await?;
    Ok(plan(Action::Move {
        roots: roots.iter().map(CommitIdHex::of).collect(),
        moved: moved.iter().map(CommitIdHex::of).collect(),
        workspaces,
    }))
}

/// The commits of `roots::` that are proper ancestors of `commit`: those a
/// branch-mode rebase moves from under the revision.
async fn below_revision(
    repo: &ReadonlyRepo,
    roots: &[CommitId],
    commit: &CommitId,
) -> Result<BTreeSet<CommitId>, RebaseError> {
    let expression = ResolvedRevsetExpression::commits(roots.to_vec())
        .descendants()
        .intersection(&ResolvedRevsetExpression::commits(vec![commit.clone()]).ancestors());
    Ok(collect(repo, &expression)
        .await?
        .into_iter()
        .filter(|id| id != commit)
        .collect())
}

/// Refuses a set with a divergent change.
async fn refuse_divergent(repo: &ReadonlyRepo, moved: &[CommitId]) -> Result<(), RebaseError> {
    for id in moved {
        let moved_commit = repo.store().get_commit_async(id).await?;
        let divergent = repo
            .resolve_change_id(moved_commit.change_id())
            .await
            .map_err(|error| RebaseError::Revision(RevisionError::Other(error.into())))?
            .is_some_and(|targets| targets.is_divergent());
        if divergent {
            return Err(RebaseError::Divergent {
                change_id: ChangeIdHex::of(moved_commit.change_id()),
                commit_id: CommitIdHex::of(id),
            });
        }
    }
    Ok(())
}

/// The jj workspaces on `moved`, sorted; another Reef workspace on a commit
/// of `below` (not `own`) is [`RebaseError::IncludesWorkingCopy`] (S-177).
async fn moved_workspaces(
    repo: &ReadonlyRepo,
    moved: &[CommitId],
    below: &BTreeSet<CommitId>,
    own: Option<&str>,
) -> Result<Vec<String>, RebaseError> {
    let moved_set: BTreeSet<&CommitId> = moved.iter().collect();
    let mut workspaces = Vec::new();
    for (name, wc) in repo.view().wc_commit_ids() {
        if !moved_set.contains(wc) {
            continue;
        }
        let name = name.as_str();
        if below.contains(wc) && name.starts_with("reef-") && Some(name) != own {
            let below_commit = repo.store().get_commit_async(wc).await?;
            return Err(RebaseError::IncludesWorkingCopy {
                commit_id: CommitIdHex::of(wc),
                change_id: ChangeIdHex::of(below_commit.change_id()),
                workspace: name.to_owned(),
            });
        }
        workspaces.push(name.to_owned());
    }
    workspaces.sort();
    Ok(workspaces)
}

/// Whether `commit` is its change's visible commit, or a visible commit of
/// a divergent change.
async fn visible(repo: &ReadonlyRepo, commit: &Commit) -> Result<bool, RebaseError> {
    Ok(repo
        .resolve_change_id(commit.change_id())
        .await
        .map_err(|error| RebaseError::Revision(RevisionError::Other(error.into())))?
        .is_some_and(|targets| {
            targets
                .visible_with_offsets()
                .any(|(_, id)| id == commit.id())
        }))
}

/// Every commit of `expression` in `repo`, newest first.
async fn collect(
    repo: &ReadonlyRepo,
    expression: &std::sync::Arc<ResolvedRevsetExpression>,
) -> Result<Vec<CommitId>, RebaseError> {
    let revset = std::sync::Arc::clone(expression).evaluate(repo)?;
    let mut stream = revset.stream();
    let mut ids = Vec::new();
    while let Some(id) = stream.next().await {
        ids.push(id?);
    }
    Ok(ids)
}

/// [`collect`], refused with [`RebaseError::TooLarge`] past `cap`.
async fn collect_capped(
    repo: &ReadonlyRepo,
    expression: &std::sync::Arc<ResolvedRevsetExpression>,
    cap: usize,
) -> Result<Vec<CommitId>, RebaseError> {
    let revset = std::sync::Arc::clone(expression).evaluate(repo)?;
    let mut stream = revset.stream();
    let mut ids = Vec::new();
    while let Some(id) = stream.next().await {
        ids.push(id?);
        if ids.len() > cap {
            let (low, _) = revset.count_estimate().await?;
            return Err(RebaseError::TooLarge {
                count: low.max(ids.len()),
            });
        }
    }
    Ok(ids)
}

/// How many commits the default destination ([`default_destination`]:
/// `trunk()`, or its remote bookmark when ahead) has that `commit` lacks,
/// at most [`MAX_BEHIND`], with the destination's name; `None` without a
/// `trunk()`. The answer reads "behind main@origin by N" (issue #259).
pub async fn behind_trunk(
    repo: &ReadonlyRepo,
    commit: &CommitIdHex,
) -> Result<Option<(u32, String)>, RebaseError> {
    let Some(destination) = default_destination(repo, None).await? else {
        return Ok(None);
    };
    let expression = ResolvedRevsetExpression::commits(vec![commit.to_id()]).range(
        &ResolvedRevsetExpression::commits(vec![destination.commit_id.to_id()]),
    );
    let revset = expression.evaluate(repo)?;
    let mut stream = revset.stream();
    let mut count = 0u32;
    while let Some(id) = stream.next().await {
        id?;
        count += 1;
        if count >= MAX_BEHIND {
            break;
        }
    }
    Ok(Some((count, destination.name)))
}

impl Repo {
    /// Writes `plan` in one operation recorded with `meta`, which this
    /// [`Repo`] adopts: `move_commits` of the roots onto the destination
    /// (change ids kept, descendants, bookmarks and working copies moved
    /// with them, empty commits kept), or a new change on the destination
    /// for [`Action::StartOn`]. Nothing for [`Action::UpToDate`]. The
    /// caller plans against the head this [`Repo`] is at.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), commit_id = %plan.commit_id))]
    pub async fn rebase(
        &mut self,
        plan: &RebasePlan,
        meta: &OpMeta,
    ) -> Result<Rebased, RebaseError> {
        let repo = std::sync::Arc::clone(&self.repo);
        let destination = repo
            .store()
            .get_commit_async(&plan.destination.commit_id.to_id())
            .await?;
        match &plan.action {
            Action::UpToDate => Ok(Rebased {
                op_id: None,
                commit_id: plan.commit_id.clone(),
                changes: Vec::new(),
                started: None,
            }),
            Action::StartOn { workspace } => {
                let mut tx = repo.start_transaction();
                let started = tx
                    .repo_mut()
                    .check_out(WorkspaceNameBuf::from(workspace.as_str()), &destination)
                    .await?;
                self.repo = meta.commit(tx).await?;
                Ok(Rebased {
                    op_id: Some(OperationIdHex::of(self.repo.op_id())),
                    commit_id: plan.commit_id.clone(),
                    changes: Vec::new(),
                    started: Some(CommitIdHex::of(started.id())),
                })
            }
            Action::Move { roots, moved, .. } => {
                let mut tx = repo.start_transaction();
                let location = MoveCommitsLocation {
                    new_parent_ids: vec![destination.id().clone()],
                    new_child_ids: Vec::new(),
                    target: MoveCommitsTarget::Roots(
                        roots.iter().map(CommitIdHex::to_id).collect(),
                    ),
                };
                let stats =
                    move_commits(tx.repo_mut(), &location, &RebaseOptions::default()).await?;
                // Working copies and bookmarks follow the rewritten commits.
                tx.repo_mut().rebase_descendants().await?;
                let mut changes = Vec::new();
                let mut new_of: HashMap<CommitId, CommitId> = HashMap::new();
                for (old, rebased) in &stats.rebased_commits {
                    if let RebasedCommit::Rewritten(new) = rebased {
                        new_of.insert(old.clone(), new.id().clone());
                    }
                }
                for (old, new) in &new_of {
                    let old_commit = repo.store().get_commit_async(old).await?;
                    let new_commit = tx.repo().store().get_commit_async(new).await?;
                    changes.push(describe_move(tx.repo(), &old_commit, &new_commit).await?);
                }
                // Newest first, in the plan's (the index's) order.
                let position = |id: &CommitIdHex| moved.iter().position(|m| m == id);
                changes.sort_by_key(|change| position(&change.old_commit_id));
                let commit_id = new_of
                    .get(&plan.commit_id.to_id())
                    .map_or_else(|| plan.commit_id.clone(), CommitIdHex::of);
                self.repo = meta.commit(tx).await?;
                Ok(Rebased {
                    op_id: Some(OperationIdHex::of(self.repo.op_id())),
                    commit_id,
                    changes,
                    started: None,
                })
            }
        }
    }
}

/// `old` moved to `new`: conflicts, and whether it became empty.
async fn describe_move(
    repo: &jj_lib::repo::MutableRepo,
    old: &Commit,
    new: &Commit,
) -> Result<RebasedChange, RebaseError> {
    let conflicted = new.has_conflict();
    // Introduced by this rebase: the new commit has a conflicted path no
    // parent carries (R3.25's rule), and the change was not conflicted
    // before it.
    let introduces_conflict = if conflicted && !old.has_conflict() {
        let mut parent_trees = Vec::with_capacity(new.parent_ids().len());
        for parent in new.parent_ids() {
            parent_trees.push(repo.store().get_commit_async(parent).await?.tree());
        }
        let options = super::conflict::ReadOptions {
            limit: 0,
            ..super::conflict::ReadOptions::default()
        };
        match super::conflict::read_tree(
            &new.tree(),
            &parent_trees,
            &ChangeIdHex::of(new.change_id()),
            &options,
        )
        .await
        {
            Ok(object) => object.introduced > 0,
            // Unreadable: a conflict with no conflicted parent is its own.
            Err(_) => parent_trees.iter().all(|tree| !tree.has_conflict()),
        }
    } else {
        false
    };
    let became_empty = !old.is_empty(repo.index()).await? && new.is_empty(repo.index()).await?;
    Ok(RebasedChange {
        change_id: ChangeIdHex::of(new.change_id()),
        old_commit_id: CommitIdHex::of(old.id()),
        new_commit_id: CommitIdHex::of(new.id()),
        conflicted,
        introduces_conflict,
        became_empty,
    })
}
