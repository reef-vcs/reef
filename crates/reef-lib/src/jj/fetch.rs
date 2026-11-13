//! Fetching branches from a git remote (issue #259, ADR 0071).
//!
//! A fetch has #258's three-step shape, so the repository thread never
//! waits on the network:
//!
//! 1. [`plan_fetch`] chooses the remote and the branches. Nothing is
//!    written.
//! 2. [`fetch`] runs jj-lib's `GitFetch` on a transaction started from the
//!    repository and dropped: git writes the objects and jj's fetched refs
//!    (`refs/jj/remote-bookmarks/<remote>/<branch>`), and the view does not
//!    change. It blocks on git; run it off the repository thread.
//! 3. [`Repo::record_fetch`] replays the import of those refs on the
//!    repository thread with jj-lib's public calls (`import_head_commits`,
//!    `add_heads`, `merge_local_bookmark`, `set_remote_bookmark`), in one
//!    operation, as `import_refs_inner` does for a fetch. A tracked local
//!    bookmark is fast-forwarded with it, except git's checked-out branch
//!    in a colocated repository (S-183). A branch the remote lacks is
//!    reported missing and left as it is in the view: it is never deleted
//!    by a fetch Reef runs, which may have named the wrong remote. Tags are
//!    not fetched, and unreachable commits are not abandoned.
//!
//! **Followed bookmarks** (R3.17, ADR 0077 decision 7). A landing
//! destination that pushes to the remote is never merged by a fetch, tracked
//! or not: it is fast-forwarded to the fetched commit when it is an ancestor
//! of it and the policy files are unchanged, and otherwise left exactly as it
//! is ([`Left`]). A merge would leave it conflicted, which stops every
//! landing onto it.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;

use jj_lib::backend::{BackendError, CommitId};
use jj_lib::commit::Commit;
use jj_lib::git::{GitFetch, GitFetchError, GitFetchRefExpression, GitImportOptions};
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RemoteName};
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::str_util::{StringExpression, StringPattern};
use jj_lib::transaction::TransactionCommitError;

use super::oplog::OpMeta;
use super::push::{self, GitFailure, PushError};
use super::repo::{CommitIdHex, OperationIdHex, Repo};
use super::revset;
use crate::error::{Classify, ErrorKind, classify_by_source};

/// The most branches one fetch names.
pub const MAX_BRANCHES: usize = 20;

/// The git ref namespace jj-lib fetches remote branches into.
const FETCHED_NAMESPACE: &str = "refs/jj/remote-bookmarks/";

/// What [`plan_fetch`] chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchPlan {
    /// The git remote.
    pub remote: String,
    /// The branches, by exact name.
    pub branches: Vec<String>,
}

/// One branch as [`fetch`] left it in git's fetched refs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedRef {
    /// The branch.
    pub branch: String,
    /// The commit the remote has; `None` when it has no such branch.
    pub commit: Option<CommitIdHex>,
}

/// What [`fetch`] fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedRefs {
    /// The remote.
    pub remote: String,
    /// Each branch planned.
    pub refs: Vec<FetchedRef>,
}

/// Why a followed bookmark was left where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Left {
    /// Neither it nor the fetched commit contains the other.
    Diverged,
    /// The fetched commit is ahead, but changes a policy file.
    Policy,
}

impl Left {
    /// The word for output and records.
    pub fn as_str(self) -> &'static str {
        match self {
            Left::Diverged => "diverged",
            Left::Policy => "policy",
        }
    }
}

/// One branch as [`Repo::record_fetch`] recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedBranch {
    /// The branch.
    pub branch: String,
    /// The remote-tracking bookmark now; `None` when missing.
    pub commit: Option<CommitIdHex>,
    /// It before; `None` when it was absent.
    pub previous: Option<CommitIdHex>,
    /// The tracked local bookmark was fast-forwarded with it.
    pub local_moved: bool,
    /// The remote has no such branch.
    pub missing: bool,
    /// A tracked local bookmark that is git's checked-out branch, left as
    /// it is.
    pub checked_out_left: bool,
    /// A followed bookmark left where it is, and why.
    pub left: Option<Left>,
}

/// What [`Repo::record_fetch`] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedFetch {
    /// The operation; `None` when nothing changed.
    pub op_id: Option<OperationIdHex>,
    /// The remote.
    pub remote: String,
    /// Each branch.
    pub branches: Vec<RecordedBranch>,
}

/// Why a fetch cannot be planned, run or recorded.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FetchError {
    /// The remote or a branch name was refused, or the repository has no
    /// git backend, as for a publish.
    #[error(transparent)]
    Plan(#[from] PushError),
    /// No branch given, no `trunk()`, and the remote has no known HEAD.
    #[error(
        "name a branch to fetch: the repository has no trunk() and {remote} has no known HEAD branch"
    )]
    NoBranch {
        /// The remote.
        remote: String,
    },
    /// More branches than [`MAX_BRANCHES`].
    #[error("at most {MAX_BRANCHES} branches per fetch")]
    TooManyBranches,
    /// Git failed.
    #[error("git fetch failed")]
    Git {
        /// How it reads.
        failure: GitFailure,
        /// Its stderr, redacted and capped.
        stderr: String,
    },
    /// Git could not be run.
    #[error("cannot run git at {0}")]
    GitNotFound(String),
    /// Git is older than jj-lib needs.
    #[error("the daemon's git is too old: {0}")]
    GitTooOld(String),
    /// Git refused to update a fetched ref.
    #[error("git refused to update {0}")]
    Rejected(String),
    /// The fetched commits could not be imported.
    #[error("import the fetched commits")]
    Import(#[source] BackendError),
    /// The store failed.
    #[error("read the repository")]
    Backend(#[from] BackendError),
    /// The index failed.
    #[error("read the index")]
    Index(#[from] jj_lib::index::IndexError),
    /// The repository's `git.*` settings could not be read.
    #[error("invalid git settings")]
    Settings(#[from] jj_lib::config::ConfigGetError),
    /// Committing the operation failed.
    #[error("record the fetch")]
    Commit(#[from] TransactionCommitError),
}

impl Classify for FetchError {
    fn kind(&self) -> ErrorKind {
        match self {
            FetchError::Plan(error) => error.kind(),
            FetchError::NoBranch { .. } | FetchError::GitTooOld(_) => ErrorKind::Conflict,
            FetchError::TooManyBranches => ErrorKind::InvalidArgument,
            FetchError::Git {
                failure: GitFailure::Auth,
                ..
            } => ErrorKind::PermissionDenied,
            FetchError::Git { .. } | FetchError::GitNotFound(_) | FetchError::Rejected(_) => {
                ErrorKind::Unavailable
            }
            FetchError::Import(error) | FetchError::Backend(error) => classify_by_source(error),
            FetchError::Index(error) => classify_by_source(error),
            FetchError::Settings(_) => ErrorKind::Internal,
            FetchError::Commit(error) => classify_by_source(error),
        }
    }
}

/// Plans a fetch from `remote` (empty: `trunk()`'s remote when it is a
/// remote bookmark, else the only configured remote, else `origin`) of
/// `branches` (empty: `trunk()`'s bookmark name, else the remote's HEAD
/// branch), in `repo`, which the caller loaded with the current git
/// configuration ([`push::with_current_git_config`]). Writes nothing.
pub fn plan_fetch(
    repo: &ReadonlyRepo,
    remote: &str,
    branches: &[String],
) -> Result<FetchPlan, FetchError> {
    if branches.len() > MAX_BRANCHES {
        return Err(FetchError::TooManyBranches);
    }
    let names = push::remote_names(repo)?;
    let trunk = revset::trunk(repo);
    let trunk_remote = trunk
        .as_ref()
        .and_then(|trunk| trunk.bookmark.split_once('@'))
        .map(|(_, remote)| remote.to_owned())
        .filter(|remote| names.contains(remote));
    let remote = match (remote.is_empty(), trunk_remote) {
        (true, Some(trunk_remote)) => trunk_remote,
        _ => push::choose_remote(&names, remote, &[])?,
    };
    let mut chosen = Vec::with_capacity(branches.len().max(1));
    for branch in branches {
        push::check_branch(branch)?;
        if !chosen.contains(branch) {
            chosen.push(branch.clone());
        }
    }
    if chosen.is_empty() {
        let branch = trunk
            .map(|trunk| {
                trunk
                    .bookmark
                    .split('@')
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .or_else(|| super::git::remote_head(repo.store(), &remote))
            .ok_or_else(|| FetchError::NoBranch {
                remote: remote.clone(),
            })?;
        push::check_branch(&branch)?;
        chosen.push(branch);
    }
    Ok(FetchPlan {
        remote,
        branches: chosen,
    })
}

/// Fetches `plan` on a transaction started from `repo` and dropped: git's
/// objects and jj's fetched refs change, the view does not. Runs the git
/// the repository's settings name with #258's environment over the
/// process environment `env` reads. Blocks on git; call it off the
/// repository thread.
pub fn fetch(
    repo: &Arc<ReadonlyRepo>,
    plan: &FetchPlan,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<FetchedRefs, FetchError> {
    let mut options =
        jj_lib::git::GitSettings::from_settings(repo.settings())?.to_subprocess_options();
    options.environment = push::push_environment(env, push::core_ssh_command(repo).as_deref());
    let executable = options.executable_path.display().to_string();
    let import_options = GitImportOptions {
        abandon_unreachable_commits: false,
        record_synthetic_predecessors: false,
        remote_auto_track_bookmarks: HashMap::new(),
    };
    let mut tx = repo.start_transaction();
    let expression = GitFetchRefExpression {
        bookmark: StringExpression::union_all(
            plan.branches
                .iter()
                .map(|branch| StringExpression::pattern(StringPattern::exact(branch.clone())))
                .collect(),
        ),
        tag: StringExpression::none(),
    };
    let mut callback = push::Messages::default();
    let fetched = {
        let mut git = GitFetch::new(tx.repo_mut(), options, &import_options)
            .map_err(|_| PushError::NoGitBackend)?;
        git.fetch(
            RemoteName::new(&plan.remote),
            &expression,
            &mut callback,
            None,
        )
    };
    drop(tx);
    match fetched {
        Ok(()) => {}
        Err(GitFetchError::NoSuchRemote(name)) => {
            return Err(PushError::RemoteNotFound {
                remote: name.as_str().to_owned(),
                remotes: Vec::new(),
            }
            .into());
        }
        Err(GitFetchError::RemoteName(_)) => {
            return Err(PushError::InvalidRemote(plan.remote.clone()).into());
        }
        Err(GitFetchError::RefExpansion(_)) => {
            return Err(PushError::InvalidBranch(plan.branches.join(",")).into());
        }
        Err(GitFetchError::RejectedUpdates(names)) => {
            let names = names
                .iter()
                .map(|name| name.as_str().to_owned())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(FetchError::Rejected(names));
        }
        Err(GitFetchError::FailedToPrune(error)) => {
            return Err(FetchError::Rejected(error.to_string()));
        }
        Err(GitFetchError::Subprocess(error)) => {
            return Err(
                match push::subprocess_error(&error.to_string(), &callback.local, &executable) {
                    PushError::GitNotFound(path) => FetchError::GitNotFound(path),
                    PushError::GitTooOld(text) => FetchError::GitTooOld(text),
                    PushError::Git { failure, stderr } => FetchError::Git { failure, stderr },
                    other => FetchError::Plan(other),
                },
            );
        }
    }
    let git = jj_lib::git::get_git_repo(repo.store()).map_err(|_| PushError::NoGitBackend)?;
    let refs = plan
        .branches
        .iter()
        .map(|branch| {
            let name = format!("{FETCHED_NAMESPACE}{}/{branch}", plan.remote);
            let commit = git
                .try_find_reference(name.as_str())
                .ok()
                .flatten()
                .and_then(|mut reference| reference.peel_to_id().ok())
                .and_then(|id| CommitIdHex::try_of_bytes(id.as_bytes()));
            FetchedRef {
                branch: branch.clone(),
                commit,
            }
        })
        .collect();
    Ok(FetchedRefs {
        remote: plan.remote.clone(),
        refs,
    })
}

impl Repo {
    /// Records `fetched` in one operation recorded with `meta`, which this
    /// [`Repo`] adopts (see the module docs); no operation when the view
    /// already says so.
    pub async fn record_fetch(
        &mut self,
        fetched: &FetchedRefs,
        meta: &OpMeta,
    ) -> Result<RecordedFetch, FetchError> {
        self.record_fetch_following(fetched, &[], meta).await
    }

    /// [`Repo::record_fetch`] with `follow`, the local bookmarks that are
    /// only ever fast-forwarded (see the module docs): a followed bookmark
    /// behind its remote bookmark is fast-forwarded even when the fetch
    /// found the remote unchanged.
    #[expect(
        clippy::too_many_lines,
        reason = "jj's import replayed in order, with the followed bookmarks"
    )]
    pub async fn record_fetch_following(
        &mut self,
        fetched: &FetchedRefs,
        follow: &[String],
        meta: &OpMeta,
    ) -> Result<RecordedFetch, FetchError> {
        let repo = Arc::clone(&self.repo);
        let followed = |branch: &str| follow.iter().any(|name| name == branch);
        let mut catch_up: Vec<(usize, CommitId)> = Vec::new();
        let remote = RemoteName::new(&fetched.remote);
        let checked_out = checked_out_branch(&repo);
        let mut branches = Vec::with_capacity(fetched.refs.len());
        let mut updates: Vec<(&FetchedRef, RemoteRef, CommitId)> = Vec::new();
        for fetched_ref in &fetched.refs {
            let symbol = RefName::new(&fetched_ref.branch).to_remote_symbol(remote);
            let old = repo.view().get_remote_bookmark(symbol).clone();
            let previous = old.target.as_normal().map(CommitIdHex::of);
            let Some(commit) = &fetched_ref.commit else {
                branches.push(RecordedBranch {
                    branch: fetched_ref.branch.clone(),
                    commit: previous.clone(),
                    previous,
                    local_moved: false,
                    missing: true,
                    checked_out_left: false,
                    left: None,
                });
                continue;
            };
            let new = commit.to_id();
            if old.target.as_normal() == Some(&new) {
                if followed(&fetched_ref.branch) {
                    catch_up.push((branches.len(), new.clone()));
                }
                branches.push(RecordedBranch {
                    branch: fetched_ref.branch.clone(),
                    commit: Some(commit.clone()),
                    previous,
                    local_moved: false,
                    missing: false,
                    checked_out_left: false,
                    left: None,
                });
                continue;
            }
            updates.push((fetched_ref, old, new));
        }
        // A followed bookmark the remote is already ahead of, from an
        // earlier fetch or someone else's: fast-forward it in this one.
        let mut catch_up_moves: Vec<(usize, CommitId)> = Vec::new();
        for (position, new) in catch_up {
            let name = RefName::new(&branches[position].branch);
            let local = repo.view().get_local_bookmark(name).clone();
            match follow_target(repo.as_ref(), &local, &new).await? {
                Follow::Move => catch_up_moves.push((position, new)),
                Follow::Stay => {}
                Follow::Leave(why) => branches[position].left = Some(why),
            }
        }
        if updates.is_empty() && catch_up_moves.is_empty() {
            return Ok(RecordedFetch {
                op_id: None,
                remote: fetched.remote.clone(),
                branches,
            });
        }
        // As jj-lib's import of fetched refs: import and index the new
        // heads, add them, then move the refs.
        let backend = jj_lib::git::get_git_backend(repo.store())
            .map_err(|_| FetchError::Plan(PushError::NoGitBackend))?;
        let heads: Vec<CommitId> = updates.iter().map(|(_, _, new)| new.clone()).collect();
        backend
            .import_head_commits(heads.iter())
            .map_err(FetchError::Import)?;
        let mut commits: Vec<Commit> = Vec::with_capacity(heads.len());
        for id in &heads {
            commits.push(repo.store().get_commit_async(id).await?);
        }
        let mut tx = repo.start_transaction();
        tx.repo_mut().index_commits(&commits).await?;
        tx.repo_mut().add_heads(&commits).await?;
        for (position, new) in catch_up_moves {
            let name = RefName::new(&branches[position].branch);
            tx.repo_mut()
                .set_local_bookmark_target(name, RefTarget::normal(new));
            branches[position].local_moved = true;
        }
        for (fetched_ref, old, new) in updates {
            let name = RefName::new(&fetched_ref.branch);
            let symbol = name.to_remote_symbol(remote);
            let target = RefTarget::normal(new.clone());
            let state = if old.target.is_present() || old.state == RemoteRefState::Tracked {
                old.state
            } else {
                RemoteRefState::New
            };
            let new_ref = RemoteRef {
                target: target.clone(),
                state,
            };
            let is_checked_out = checked_out.as_deref() == Some(fetched_ref.branch.as_str());
            let before = tx.repo().view().get_local_bookmark(name).clone();
            let mut left = None;
            if followed(&fetched_ref.branch) {
                // Fast-forward only, tracked or not, checked out or not: a
                // landing moves a checked-out destination too (R3.13). The
                // transaction's index has the fetched commits.
                match follow_target(tx.repo(), &before, &new).await? {
                    Follow::Move => tx
                        .repo_mut()
                        .set_local_bookmark_target(name, RefTarget::normal(new.clone())),
                    Follow::Stay => {}
                    Follow::Leave(why) => left = Some(why),
                }
            } else if new_ref.is_tracked() && !is_checked_out {
                let base = merge_base(&repo, &before, &old).await?;
                tx.repo_mut()
                    .merge_local_bookmark(name, &base, &target)
                    .await?;
            }
            let local_moved = *tx.repo().view().get_local_bookmark(name) != before;
            tx.repo_mut().set_remote_bookmark(symbol, new_ref);
            branches.push(RecordedBranch {
                branch: fetched_ref.branch.clone(),
                commit: Some(CommitIdHex::of(&new)),
                previous: old.target.as_normal().map(CommitIdHex::of),
                local_moved,
                missing: false,
                checked_out_left: !followed(&fetched_ref.branch)
                    && is_checked_out
                    && old.state == RemoteRefState::Tracked,
                left,
            });
        }
        branches.sort_by(|a, b| {
            let position = |branch: &str| fetched.refs.iter().position(|r| r.branch == branch);
            position(&a.branch).cmp(&position(&b.branch))
        });
        self.repo = meta.commit(tx).await?;
        Ok(RecordedFetch {
            op_id: Some(OperationIdHex::of(self.repo.op_id())),
            remote: fetched.remote.clone(),
            branches,
        })
    }
}

/// The base a fetched remote bookmark merges into its tracked local one
/// over: the old remote target, as jj's import, unless the local bookmark
/// is strictly behind it (a fetch skipped git's checked-out branch), when it
/// fast-forwards from where it is; merged over the old remote target it
/// would conflict.
async fn merge_base(
    repo: &ReadonlyRepo,
    local: &RefTarget,
    old: &RemoteRef,
) -> Result<RefTarget, FetchError> {
    Ok(match (local.as_normal(), old.target.as_normal()) {
        (Some(local), Some(remote))
            if local != remote && repo.index().is_ancestor(local, remote).await? =>
        {
            RefTarget::normal(local.clone())
        }
        _ => old.tracked_target().clone(),
    })
}

/// What a fetch does with a followed bookmark.
enum Follow {
    /// Fast-forward it to the fetched commit.
    Move,
    /// Nothing: it is absent, at the commit, or ahead of it.
    Stay,
    /// Leave it, for this reason.
    Leave(Left),
}

/// Whether followed bookmark `local` moves to `new`: only forward, and only
/// when the policy files are the same at both (S-201). An absent bookmark
/// stays absent: following never creates a destination.
async fn follow_target(
    repo: &(impl jj_lib::repo::Repo + ?Sized),
    local: &RefTarget,
    new: &CommitId,
) -> Result<Follow, FetchError> {
    if local.is_absent() {
        return Ok(Follow::Stay);
    }
    let Some(at) = local.as_normal() else {
        return Ok(Follow::Leave(Left::Diverged));
    };
    if at == new || repo.index().is_ancestor(new, at).await? {
        return Ok(Follow::Stay);
    }
    if !repo.index().is_ancestor(at, new).await? {
        return Ok(Follow::Leave(Left::Diverged));
    }
    if push::policy_files_differ(repo, at, new).await? {
        return Ok(Follow::Leave(Left::Policy));
    }
    Ok(Follow::Move)
}

/// The branch git's `HEAD` names in a colocated repository, if any.
fn checked_out_branch(repo: &ReadonlyRepo) -> Option<String> {
    let git = jj_lib::git::get_git_repo(repo.store()).ok()?;
    let head = git.head_name().ok()??;
    head.as_bstr()
        .to_string()
        .strip_prefix("refs/heads/")
        .map(str::to_owned)
}

/// The description of a fetch operation: `reef: fetch <branches> from
/// <remote>`.
pub fn fetch_description(fetched: &FetchedRefs) -> String {
    let branches: Vec<&str> = fetched.refs.iter().map(|r| r.branch.as_str()).collect();
    format!(
        "{}{} from {}",
        super::oplog::FETCH_PREFIX,
        branches.join(", "),
        fetched.remote
    )
}
