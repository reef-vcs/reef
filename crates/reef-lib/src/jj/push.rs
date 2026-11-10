//! Publishing a change to a git remote (#258, ADR 0070).
//!
//! Three steps, so the repository thread never waits on the network:
//!
//! 1. [`plan_push`], on the repository thread: the remote and the branch,
//!    the range the remote's branches lack (`::C ~ ::old`), every check on
//!    it, how the branch moves ([`PushUpdate`]) and the lease.
//! 2. [`push`], anywhere: jj-lib's `push_refs` on a transaction started
//!    from the plan's repository and then dropped, so the remote and git's
//!    fetched ref (`refs/jj/remote-bookmarks/…`, which only `push_refs`
//!    writes) change and nothing else does.
//! 3. [`Repo::record_push`], on the repository thread: the tracked remote
//!    bookmark and the local bookmark, in one operation of kind
//!    [`OpKind::Publish`](super::oplog::OpKind::Publish).
//!
//! **Updates.** The lease is always the remote bookmark's target as Reef
//! last saw it ("must not exist" when it has none), so a branch someone
//! else moved is [`PushError::Moved`], never overwritten. A non-fast-forward
//! update is allowed only when the branch's commit belongs to a change being
//! published: its change id (from the commit's `change-id` header, which
//! jj writes and reads, or derived from its hash for a commit made with
//! plain git) is the change id of the published commit or of one in the
//! range. Never "a change in `::C`", which would include all of trunk.
//! Predecessors are never used: they live in operations, which GC bounds.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::sync::Arc;

use futures::StreamExt as _;
use jj_lib::backend::{BackendError, ChangeId, CommitId, TreeId};
use jj_lib::commit::Commit;
use jj_lib::git::{
    GitPushError, GitPushOptions, GitPushRefTargets, GitSidebandLineTerminator,
    GitSubprocessCallback, GitSubprocessOptions,
};
use jj_lib::index::IndexError;
use jj_lib::merge::{Diff, Merge};
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RefNameBuf, RemoteName};
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::revset::{ResolvedRevsetExpression, RevsetEvaluationError};
use jj_lib::transaction::TransactionCommitError;

use super::log::Describer;
use super::oplog::OpMeta;
use super::repo::{ChangeIdHex, CommitIdHex, OperationIdHex, Repo};
use super::revset::{RevisionError, RevsetScope};
use super::rewrite::{self, RewriteError};
use crate::error::{Classify, ErrorKind, classify_by_source};

/// The most commits a publish pushes that the remote's branches lack.
pub const MAX_RANGE: usize = 1_000;
/// The most commits and changes a [`PushPlan`] lists.
pub const MAX_LISTED: usize = 100;
/// The default branch prefix.
pub const DEFAULT_BRANCH_PREFIX: &str = "reef/";
/// How many letters of the change id a default branch name takes.
pub const BRANCH_ID_LETTERS: usize = 12;
/// The most bytes of a branch name.
pub const MAX_BRANCH_BYTES: usize = 200;
/// The most remote message lines kept, and bytes per line.
pub const MAX_REMOTE_LINES: usize = 20;
/// See [`MAX_REMOTE_LINES`].
pub const MAX_REMOTE_LINE_BYTES: usize = 300;
/// The most bytes of git's stderr kept in an error, from its end.
pub const MAX_STDERR_BYTES: usize = 2_048;

/// What to publish, as the daemon asks for it.
#[derive(Clone, Debug, Default)]
pub struct PushRequest {
    /// The revision naming the change.
    pub revision: String,
    /// The commit the caller expects it to resolve to.
    pub expected: Option<CommitIdHex>,
    /// The git remote; empty for the default.
    pub git_remote: String,
    /// The branch; empty for `branch_prefix` and the change id.
    pub branch: String,
    /// The prefix of a default branch name.
    pub branch_prefix: String,
    /// The remotes allowed; empty allows every configured remote.
    pub allowed_remotes: Vec<String>,
    /// Replace a branch holding another change's commit, or move a local
    /// bookmark elsewhere.
    pub force: bool,
    /// The caller's own jj workspace (`reef-<id>`), whose working copy may
    /// be in the range.
    pub own_workspace: Option<String>,
    /// The cap on the range; [`MAX_RANGE`] unless a test lowers it.
    pub max_commits: usize,
    /// The description a dry run's request would set first: the published
    /// commit's undescribed check and the plan's description read it
    /// instead of the commit's own.
    pub description: Option<String>,
}

/// How a publish moves the branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushUpdate {
    /// The branch does not exist on the remote (as Reef last saw it).
    Created,
    /// The branch holds an ancestor of the commit.
    FastForward,
    /// The branch holds a commit of a change being published.
    Rewritten,
    /// The branch holds another change's commit; `force` replaces it.
    Forced,
    /// The branch already holds the commit.
    UpToDate,
}

impl PushUpdate {
    /// The word for text output.
    pub fn as_str(self) -> &'static str {
        match self {
            PushUpdate::Created => "created",
            PushUpdate::FastForward => "fast-forward",
            PushUpdate::Rewritten => "rewritten",
            PushUpdate::Forced => "forced",
            PushUpdate::UpToDate => "up to date",
        }
    }
}

/// One commit of the range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeCommit {
    /// The commit.
    pub commit: CommitIdHex,
    /// Its change.
    pub change: ChangeIdHex,
}

/// What [`plan_push`] decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushPlan {
    /// The git remote.
    pub remote: String,
    /// The branch.
    pub branch: String,
    /// The commit published (`C`).
    pub commit: CommitIdHex,
    /// Its change.
    pub change: ChangeIdHex,
    /// The branch's target on the remote as Reef last saw it: the lease.
    pub previous: Option<CommitIdHex>,
    /// How the branch moves.
    pub update: PushUpdate,
    /// The commits the remote's branches lack, newest first (`M`).
    pub range: Vec<RangeCommit>,
    /// Whether the record sets the local bookmark of the branch's name.
    pub move_local: bool,
    /// The local bookmark's target when planned; `None` when absent or
    /// conflicted. The record leaves a bookmark that moved since alone.
    pub local_before: Option<CommitIdHex>,
    /// Whether `force` moves a local bookmark that held another change's
    /// commit: recorded as forced, as a forced remote update is.
    pub local_forced: bool,
    /// `C`'s description.
    pub description: String,
    /// The branch a pull request would target: trunk's bookmark when the
    /// remote has it, else the remote's HEAD branch.
    pub base: Option<String>,
    /// The remote's push URL, as git configures it after rewriting.
    pub push_url: Option<String>,
}

impl PushPlan {
    /// The range's change ids, newest first, without repeats.
    pub fn change_ids(&self) -> Vec<ChangeIdHex> {
        let mut seen = HashSet::new();
        self.range
            .iter()
            .filter(|commit| seen.insert(commit.change.clone()))
            .map(|commit| commit.change.clone())
            .collect()
    }
}

/// Which rule a branch broke for [`PushError::NotPublishable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unpublishable {
    /// It is trunk's bookmark.
    Trunk,
    /// It is the remote's HEAD branch.
    RemoteHead,
    /// Its remote bookmark points into `immutable()`.
    Immutable,
    /// It is git's checked-out branch in a colocated repository.
    CheckedOut,
}

impl Unpublishable {
    /// The `why` metadata value.
    pub fn as_str(self) -> &'static str {
        match self {
            Unpublishable::Trunk => "trunk",
            Unpublishable::RemoteHead => "remote_head",
            Unpublishable::Immutable => "immutable",
            Unpublishable::CheckedOut => "checked_out",
        }
    }
}

/// A commit a range check refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offending {
    /// The commit.
    pub commit: CommitIdHex,
    /// Its change.
    pub change: ChangeIdHex,
    /// Whether it is the published commit itself.
    pub is_target: bool,
}

/// The commit a branch holds, for [`PushError::Diverged`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignCommit {
    /// The commit.
    pub commit: CommitIdHex,
    /// Its change.
    pub change: ChangeIdHex,
    /// `Name <email>` of its author.
    pub author: String,
    /// Its description's first line, sanitized.
    pub summary: String,
    /// Whether Reef knows it as rewritten (not visible).
    pub hidden: bool,
    /// Whether it is the local bookmark that is elsewhere, not the remote's.
    pub local: bool,
}

/// How git's failure reads, from its stderr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitFailure {
    /// Credentials refused or missing.
    Auth,
    /// The network or the host.
    Unreachable,
    /// Anything else.
    Other,
}

/// Why a publish cannot be planned, pushed or recorded.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PushError {
    /// Resolving or checking the revision failed.
    #[error("resolve the revision")]
    Rewrite(#[from] RewriteError),
    /// The commit changes nothing.
    #[error("change {} is empty", .0.change)]
    Empty(Offending),
    /// A commit's description is empty or its first line blank.
    #[error("commit {} has no description", .0.commit)]
    Undescribed(Offending),
    /// A commit has conflicts.
    #[error("commit {} has conflicts", .0.commit)]
    Conflicted(Offending),
    /// A commit's change has several visible commits.
    #[error("change {} is divergent", .0.change)]
    Divergent(Offending),
    /// A commit's author or committer has no name or email.
    #[error("commit {} has no author or committer name or email", .0.commit)]
    IdentityMissing(Offending),
    /// A commit is authored or committed as the default `reef` identity.
    #[error("commit {} is authored as reef's default identity", .0.commit)]
    AuthorDefault(Offending),
    /// A commit is another active workspace's working copy.
    #[error("commit {} is workspace {workspace}'s working copy", offending.commit)]
    IncludesWorkingCopy {
        /// The commit.
        offending: Offending,
        /// The jj workspace.
        workspace: String,
    },
    /// The range is larger than the cap.
    #[error("more than {max} commits are not on the remote")]
    RangeTooLarge {
        /// The cap.
        max: usize,
    },
    /// The repository has no git backend.
    #[error("the repository has no git backend")]
    NoGitBackend,
    /// No remote was named and none is the default.
    #[error("name a git remote: the repository has {}", if remotes.is_empty() { "none".to_owned() } else { remotes.join(", ") })]
    RemoteRequired {
        /// The configured remotes.
        remotes: Vec<String>,
    },
    /// A remote name Reef does not accept.
    #[error("{0:?} is not a git remote name Reef pushes to")]
    InvalidRemote(String),
    /// The remote is not configured.
    #[error("no git remote {remote:?}")]
    RemoteNotFound {
        /// The name.
        remote: String,
        /// The configured remotes.
        remotes: Vec<String>,
    },
    /// The remote is not in the daemon's allowed list.
    #[error("git remote {0:?} is not allowed by [publish] remotes")]
    RemoteNotAllowed(String),
    /// A branch name Reef does not push to.
    #[error("{0:?} is not a branch name Reef pushes to")]
    InvalidBranch(String),
    /// The branch is trunk's, the remote's HEAD, immutable, or checked out.
    #[error("branch {branch:?} cannot be published to ({})", why.as_str())]
    NotPublishable {
        /// The branch.
        branch: String,
        /// Why.
        why: Unpublishable,
    },
    /// The branch holds another change's commit, or the local bookmark is
    /// elsewhere.
    #[error("branch {branch:?} holds commit {}, which is not part of this change", foreign.commit)]
    Diverged {
        /// The branch.
        branch: String,
        /// What it holds.
        foreign: Box<ForeignCommit>,
    },
    /// The remote bookmark is conflicted.
    #[error("the remote bookmark of {0:?} is conflicted")]
    BranchConflicted(String),
    /// The lease failed.
    #[error("{remote}/{branch} moved since Reef last saw it")]
    Moved {
        /// The remote.
        remote: String,
        /// The branch.
        branch: String,
    },
    /// The remote refused the push.
    #[error("the remote refused the push: {}", reason.as_deref().unwrap_or("no reason given"))]
    RemoteRejected {
        /// Its reason, sanitized.
        reason: Option<String>,
    },
    /// Git failed.
    #[error("git push failed")]
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
    /// The repository's `git.*` settings could not be read.
    #[error("invalid git settings")]
    Settings(#[from] jj_lib::config::ConfigGetError),
    /// The store failed.
    #[error("read the repository")]
    Backend(#[from] BackendError),
    /// The index failed.
    #[error("read the index")]
    Index(#[from] IndexError),
    /// A revset failed.
    #[error("evaluate the range")]
    Evaluation(#[from] RevsetEvaluationError),
    /// A revision failed.
    #[error("read a revision")]
    Revision(#[from] RevisionError),
    /// The operation could not be written.
    #[error("record the publish")]
    Commit(#[from] TransactionCommitError),
    /// The repository could not be loaded again.
    #[error("load the repository")]
    Stores(#[from] jj_lib::repo::StoreLoadError),
    /// The repository could not be loaded at its operation.
    #[error("load the repository at its operation")]
    Load(#[from] jj_lib::repo::RepoLoaderError),
    /// The repository's operation could not be read.
    #[error("read the repository's operation")]
    Operation(#[from] jj_lib::op_store::OpStoreError),
}

impl Classify for PushError {
    fn kind(&self) -> ErrorKind {
        match self {
            PushError::Rewrite(error) => error.kind(),
            PushError::Revision(error) => error.kind(),
            PushError::InvalidRemote(_) | PushError::InvalidBranch(_) => ErrorKind::InvalidArgument,
            PushError::RemoteNotFound { .. } => ErrorKind::NotFound,
            PushError::RemoteNotAllowed(_)
            | PushError::Git {
                failure: GitFailure::Auth,
                ..
            } => ErrorKind::PermissionDenied,
            PushError::Git { .. } | PushError::GitNotFound(_) => ErrorKind::Unavailable,
            PushError::Backend(error) => classify_by_source(error),
            PushError::Index(error) => classify_by_source(error),
            PushError::Commit(error) => classify_by_source(error),
            PushError::Evaluation(_)
            | PushError::Settings(_)
            | PushError::Stores(_)
            | PushError::Load(_)
            | PushError::Operation(_) => ErrorKind::Internal,
            _ => ErrorKind::Conflict,
        }
    }
}

/// Whether `name` and `email` are the daemon's default identity's form,
/// `reef <reef@host>`.
pub fn is_default_identity(name: &str, email: &str) -> bool {
    name == "reef" && email.starts_with("reef@")
}

/// The default branch for `change`: `prefix` and its first
/// [`BRANCH_ID_LETTERS`] letters in jj's reverse hex.
pub fn default_branch(prefix: &str, change: &ChangeIdHex) -> String {
    let letters: String = change
        .reverse_hex()
        .chars()
        .take(BRANCH_ID_LETTERS)
        .collect();
    format!("{prefix}{letters}")
}

/// Checks `name` as a branch Reef pushes to: 1 to [`MAX_BRANCH_BYTES`]
/// bytes of `[A-Za-z0-9._/-]`, not starting with `-`, `.` or `/`, not
/// ending with `/`, `.` or `.lock`, with no `//`, `..` or `@{`, not `HEAD`,
/// and a valid git ref once prefixed with `refs/heads/`.
pub fn check_branch(name: &str) -> Result<(), PushError> {
    let invalid = || PushError::InvalidBranch(name.to_owned());
    if name.is_empty() || name.len() > MAX_BRANCH_BYTES {
        return Err(invalid());
    }
    check_branch_text(name).map_err(|()| invalid())?;
    let full = format!("refs/heads/{name}");
    gix::refs::FullName::try_from(full.as_str()).map_err(|_| invalid())?;
    Ok(())
}

/// Checks a branch prefix with [`check_branch`]'s character rules; empty
/// is allowed. `Err` carries the reason for a config error.
pub fn check_branch_prefix(prefix: &str) -> Result<(), String> {
    if prefix.is_empty() {
        return Ok(());
    }
    if prefix.len() > MAX_BRANCH_BYTES / 2 {
        return Err(format!("longer than {} bytes", MAX_BRANCH_BYTES / 2));
    }
    // A prefix may end with `/` (`reef/`): check it as the start of a name.
    check_branch_text(&format!("{prefix}x"))
        .map_err(|()| "not the start of a branch name Reef pushes to".to_owned())
}

#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "git refuses `.lock` in exactly that case"
)]
fn check_branch_text(name: &str) -> Result<(), ()> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-');
    if !name.chars().all(allowed)
        || name.starts_with(['-', '.', '/'])
        || name.ends_with(['/', '.'])
        || name.ends_with(".lock")
        || name.contains("//")
        || name.contains("..")
        || name.contains("@{")
        || name == "HEAD"
        || name
            .split('/')
            .any(|part| part.starts_with('.') || part.ends_with(".lock"))
    {
        return Err(());
    }
    Ok(())
}

/// Checks `name` as a git remote name: `[A-Za-z0-9][A-Za-z0-9._-]{0,99}`
/// and not `git` (jj's name for the colocated repository). A URL fails.
pub fn check_remote(name: &str) -> Result<(), PushError> {
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !first_ok || !rest_ok || name.len() > 100 || name == "git" {
        return Err(PushError::InvalidRemote(name.to_owned()));
    }
    Ok(())
}

/// The configured remotes of `repo`, sorted; [`PushError::NoGitBackend`]
/// without a git backend.
pub fn remote_names(repo: &ReadonlyRepo) -> Result<Vec<String>, PushError> {
    jj_lib::git::get_all_remote_names(repo.store())
        .map(|names| names.iter().map(|name| name.as_str().to_owned()).collect())
        .map_err(|_| PushError::NoGitBackend)
}

/// `repo` at its operation over stores opened again from `repo_path` (the
/// `.jj/repo` directory), so its git configuration (the remotes, their URLs,
/// `core.sshCommand`) is read now. jj-lib's git backend reads it once, when
/// the store opens, and a daemon keeps its repositories open: without this a
/// remote added after the daemon loaded the repository would not exist for
/// the plan or for `push_refs`.
pub async fn with_current_git_config(
    repo: &ReadonlyRepo,
    repo_path: &std::path::Path,
) -> Result<Arc<ReadonlyRepo>, PushError> {
    let loader = jj_lib::repo::RepoLoader::init_from_file_system(
        repo.settings(),
        repo_path,
        &jj_lib::default_backend_factories::default_backend_factories(),
    )?;
    let operation = loader.load_operation(repo.op_id()).await?;
    Ok(loader.load_at(&operation).await?)
}

/// Picks the remote: `given` if not empty (checked, configured, allowed);
/// else the only configured one; else `origin`; else
/// [`PushError::RemoteRequired`].
pub fn choose_remote(
    names: &[String],
    given: &str,
    allowed: &[String],
) -> Result<String, PushError> {
    let remote = if given.is_empty() {
        match names {
            [only] => only.clone(),
            _ if names.iter().any(|name| name == "origin") => "origin".to_owned(),
            _ => {
                return Err(PushError::RemoteRequired {
                    remotes: names.to_vec(),
                });
            }
        }
    } else {
        check_remote(given)?;
        if !names.iter().any(|name| name == given) {
            return Err(PushError::RemoteNotFound {
                remote: given.to_owned(),
                remotes: names.to_vec(),
            });
        }
        given.to_owned()
    };
    if !allowed.is_empty() && !allowed.contains(&remote) {
        return Err(PushError::RemoteNotAllowed(remote));
    }
    Ok(remote)
}

/// Plans publishing `request.revision` in `repo`, which the caller has
/// imported git refs into. Writes nothing.
#[tracing::instrument(skip_all)]
pub async fn plan_push(
    repo: &ReadonlyRepo,
    request: &PushRequest,
    scope: &RevsetScope<'_>,
) -> Result<PushPlan, PushError> {
    let names = remote_names(repo)?;
    let remote = choose_remote(&names, request.git_remote.trim(), &request.allowed_remotes)?;
    let writable =
        rewrite::writable(repo, &request.revision, request.expected.as_ref(), scope).await?;
    let commit_id = writable.commit_id.to_id();
    let commit = repo.store().get_commit_async(&commit_id).await?;
    let change = writable.change_id.clone();
    let branch = if request.branch.trim().is_empty() {
        default_branch(&request.branch_prefix, &change)
    } else {
        request.branch.trim().to_owned()
    };
    check_branch(&branch)?;

    let describer = Describer::new(repo, scope)?;
    let info = describer.describe(&writable.commit_id).await?;
    let target = Offending {
        commit: writable.commit_id.clone(),
        change: change.clone(),
        is_target: true,
    };
    if info.empty {
        return Err(PushError::Empty(target));
    }

    let range = range(repo, &remote, &commit_id, request.max_commits).await?;
    // A commit another branch of the remote already has is checked too:
    // the range is then empty, but the commit still gets a new branch.
    let target_only;
    let checked = if range.is_empty() {
        target_only = [RangeCommit {
            commit: writable.commit_id.clone(),
            change: change.clone(),
        }];
        &target_only[..]
    } else {
        &range[..]
    };
    check_range(repo, &describer, checked, request).await?;
    let description = request
        .description
        .clone()
        .unwrap_or_else(|| commit.description().to_owned());
    let changes: HashSet<ChangeIdHex> = range
        .iter()
        .map(|commit| commit.change.clone())
        .chain(std::iter::once(change.clone()))
        .collect();

    check_publishable(repo, &remote, &branch).await?;
    let branch_ref = BranchRef {
        remote: &remote,
        branch: &branch,
        commit: &commit_id,
        change: &change,
        force: request.force,
    };
    let (previous, update) = branch_update(repo, &describer, &branch_ref, &changes).await?;
    let local_forced = check_local(repo, &describer, &branch_ref, previous.as_ref()).await?;
    let local_before = repo
        .view()
        .get_local_bookmark(RefName::new(&branch))
        .as_normal()
        .map(CommitIdHex::of);

    let base = base_branch(repo, &remote);
    Ok(PushPlan {
        push_url: push_url(repo, &remote),
        remote,
        branch,
        commit: writable.commit_id,
        change,
        previous: previous.as_ref().map(CommitIdHex::of),
        update,
        range,
        move_local: true,
        local_before,
        local_forced,
        description,
        base,
    })
}

/// The branch a plan publishes to, for [`branch_update`] and
/// [`check_local`].
struct BranchRef<'a> {
    remote: &'a str,
    branch: &'a str,
    commit: &'a CommitId,
    change: &'a ChangeIdHex,
    force: bool,
}

/// The lease (the remote bookmark's target) and how the branch moves:
/// [`PushUpdate::Rewritten`] only when the branch's commit's change is one
/// of `changes` (the range's and the published commit's).
async fn branch_update(
    repo: &ReadonlyRepo,
    describer: &Describer<'_>,
    at: &BranchRef<'_>,
    changes: &HashSet<ChangeIdHex>,
) -> Result<(Option<CommitId>, PushUpdate), PushError> {
    let remote_ref = repo
        .view()
        .get_remote_bookmark(RefName::new(at.branch).to_remote_symbol(RemoteName::new(at.remote)));
    if !remote_ref.target.is_resolved() {
        return Err(PushError::BranchConflicted(at.branch.to_owned()));
    }
    let previous = remote_ref.target.as_normal().cloned();
    let update = match &previous {
        None => PushUpdate::Created,
        Some(id) if id == at.commit => PushUpdate::UpToDate,
        Some(id) if repo.index().is_ancestor(id, at.commit).await? => PushUpdate::FastForward,
        Some(id) => {
            let held = repo.store().get_commit_async(id).await?;
            if changes.contains(&ChangeIdHex::of(held.change_id())) {
                PushUpdate::Rewritten
            } else if at.force {
                PushUpdate::Forced
            } else {
                return Err(PushError::Diverged {
                    branch: at.branch.to_owned(),
                    foreign: Box::new(foreign(describer, &held, false).await?),
                });
            }
        }
    };
    Ok((previous, update))
}

/// Refuses, without `force`, a local bookmark of the branch's name at a
/// commit other than the published one, the lease's, or one of the same
/// change, so `--branch feature` never moves a person's bookmark.
async fn check_local(
    repo: &ReadonlyRepo,
    describer: &Describer<'_>,
    at: &BranchRef<'_>,
    previous: Option<&CommitId>,
) -> Result<bool, PushError> {
    let local = repo.view().get_local_bookmark(RefName::new(at.branch));
    if local.is_absent() {
        return Ok(false);
    }
    let held = match local.as_normal() {
        Some(id) if id == at.commit || previous == Some(id) => return Ok(false),
        Some(id) => id.clone(),
        // A conflicted local bookmark is a person's to resolve.
        None => match local.adds().flatten().next() {
            Some(id) => id.clone(),
            None => return Ok(false),
        },
    };
    let held = repo.store().get_commit_async(&held).await?;
    if local.as_normal().is_some() && ChangeIdHex::of(held.change_id()) == *at.change {
        return Ok(false);
    }
    if at.force {
        return Ok(true);
    }
    Err(PushError::Diverged {
        branch: at.branch.to_owned(),
        foreign: Box::new(foreign(describer, &held, true).await?),
    })
}

/// `::commit ~ ::(every target of `remote`'s bookmarks | root())`, newest
/// first, refusing more than `max` commits.
async fn range(
    repo: &ReadonlyRepo,
    remote: &str,
    commit: &CommitId,
    max: usize,
) -> Result<Vec<RangeCommit>, PushError> {
    let mut old: Vec<CommitId> = repo
        .view()
        .remote_bookmarks(RemoteName::new(remote))
        .flat_map(|(_, remote_ref)| remote_ref.target.adds().flatten().cloned())
        .collect();
    old.push(repo.store().root_commit_id().clone());
    let expression = ResolvedRevsetExpression::commits(old)
        .range(&ResolvedRevsetExpression::commits(vec![commit.clone()]));
    let revset = expression.evaluate(repo)?;
    let mut stream = revset.stream();
    let mut commits = Vec::new();
    while let Some(id) = stream.next().await {
        let id = id?;
        if commits.len() == max {
            return Err(PushError::RangeTooLarge { max });
        }
        let commit = repo.store().get_commit_async(&id).await?;
        commits.push(RangeCommit {
            commit: CommitIdHex::of(&id),
            change: ChangeIdHex::of(commit.change_id()),
        });
    }
    Ok(commits)
}

/// The range checks, newest first, each refusing with the first offending
/// commit.
async fn check_range(
    repo: &ReadonlyRepo,
    describer: &Describer<'_>,
    range: &[RangeCommit],
    request: &PushRequest,
) -> Result<(), PushError> {
    let mut working_copies: BTreeMap<CommitId, Vec<String>> = BTreeMap::new();
    for (name, id) in repo.view().wc_commit_ids() {
        if request.own_workspace.as_deref() == Some(name.as_str()) {
            continue;
        }
        working_copies
            .entry(id.clone())
            .or_default()
            .push(name.as_str().to_owned());
    }
    let target = range.first().map(|commit| commit.commit.clone());
    for commit in range {
        let info = describer.describe(&commit.commit).await?;
        let offending = Offending {
            commit: commit.commit.clone(),
            change: commit.change.clone(),
            is_target: target.as_ref() == Some(&commit.commit),
        };
        let description = match &request.description {
            Some(text) if offending.is_target => text.as_str(),
            _ => info.description.as_str(),
        };
        if description
            .lines()
            .next()
            .is_none_or(|line| line.trim().is_empty())
        {
            return Err(PushError::Undescribed(offending));
        }
        if info.conflict {
            return Err(PushError::Conflicted(offending));
        }
        if info.divergent {
            return Err(PushError::Divergent(offending));
        }
        let signatures = [&info.author, &info.committer];
        if signatures
            .iter()
            .any(|sig| sig.name.trim().is_empty() || sig.email.trim().is_empty())
        {
            return Err(PushError::IdentityMissing(offending));
        }
        if signatures
            .iter()
            .any(|sig| is_default_identity(&sig.name, &sig.email))
        {
            return Err(PushError::AuthorDefault(offending));
        }
        if let Some(names) = working_copies.get(&commit.commit.to_id()) {
            return Err(PushError::IncludesWorkingCopy {
                offending,
                workspace: names[0].clone(),
            });
        }
    }
    Ok(())
}

/// Refuses trunk's branch, the remote's HEAD branch, a branch whose remote
/// bookmark is immutable, and git's checked-out branch.
async fn check_publishable(
    repo: &ReadonlyRepo,
    remote: &str,
    branch: &str,
) -> Result<(), PushError> {
    let refuse = |why| {
        Err(PushError::NotPublishable {
            branch: branch.to_owned(),
            why,
        })
    };
    if let Some(trunk) = super::revset::trunk(repo) {
        let name = trunk.bookmark.split('@').next().unwrap_or_default();
        if name == branch {
            return refuse(Unpublishable::Trunk);
        }
    }
    if super::git::remote_head(repo.store(), remote).as_deref() == Some(branch) {
        return refuse(Unpublishable::RemoteHead);
    }
    let remote_ref = repo
        .view()
        .get_remote_bookmark(RefName::new(branch).to_remote_symbol(RemoteName::new(remote)));
    if let Some(id) = remote_ref.target.as_normal() {
        let scope = RevsetScope {
            workspace: None,
            user_email: "",
            root: std::path::Path::new("/"),
        };
        let info = Describer::new(repo, &scope)?
            .describe(&CommitIdHex::of(id))
            .await?;
        if info.immutable {
            return refuse(Unpublishable::Immutable);
        }
    }
    if let Ok(git) = jj_lib::git::get_git_repo(repo.store())
        && let Ok(Some(head)) = git.head_name()
        && head.as_bstr() == format!("refs/heads/{branch}").as_bytes()
    {
        return refuse(Unpublishable::CheckedOut);
    }
    Ok(())
}

async fn foreign(
    describer: &Describer<'_>,
    commit: &Commit,
    local: bool,
) -> Result<ForeignCommit, PushError> {
    let id = CommitIdHex::of(commit.id());
    let hidden = describer.describe(&id).await.is_ok_and(|info| info.hidden);
    let author = commit.author();
    Ok(ForeignCommit {
        commit: id,
        change: ChangeIdHex::of(commit.change_id()),
        author: sanitize_line(&format!("{} <{}>", author.name, author.email), 200),
        summary: sanitize_line(commit.description().lines().next().unwrap_or_default(), 200),
        hidden,
        local,
    })
}

/// The branch a pull request targets: trunk's bookmark name when `remote`
/// has a bookmark of that name, else the remote's HEAD branch.
fn base_branch(repo: &ReadonlyRepo, remote: &str) -> Option<String> {
    if let Some(trunk) = super::revset::trunk(repo) {
        let name = trunk
            .bookmark
            .split('@')
            .next()
            .unwrap_or_default()
            .to_owned();
        let on_remote = repo
            .view()
            .get_remote_bookmark(RefName::new(&name).to_remote_symbol(RemoteName::new(remote)));
        if on_remote.is_present() {
            return Some(name);
        }
    }
    super::git::remote_head(repo.store(), remote)
}

/// `remote`'s push URL (`pushurl`, else `url`, after `insteadOf`).
fn push_url(repo: &ReadonlyRepo, remote: &str) -> Option<String> {
    let git = jj_lib::git::get_git_repo(repo.store()).ok()?;
    let found = git.find_remote(remote).ok()?;
    let url = found
        .url(gix::remote::Direction::Push)
        .or_else(|| found.url(gix::remote::Direction::Fetch))?;
    Some(url.to_bstring().to_string())
}

/// What [`push`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pushed {
    /// The remote's messages, sanitized and capped.
    pub remote_messages: Vec<String>,
}

/// Pushes `plan` on a transaction started from `repo` and dropped: the
/// remote and git's fetched ref change, the view does not. Runs the git
/// the repository's settings name (`git.executable-path`) with
/// [`push_environment`] over the process environment `env` reads. Blocks
/// on git; call it off the repository thread.
pub fn push(
    repo: &Arc<ReadonlyRepo>,
    plan: &PushPlan,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<Pushed, PushError> {
    let mut options: GitSubprocessOptions =
        jj_lib::git::GitSettings::from_settings(repo.settings())?.to_subprocess_options();
    options.environment = push_environment(env, core_ssh_command(repo).as_deref());
    let mut tx = repo.start_transaction();
    let targets = GitPushRefTargets {
        bookmarks: vec![(
            RefNameBuf::from(plan.branch.clone()),
            Diff::new(
                plan.previous.as_ref().map(CommitIdHex::to_id),
                Some(plan.commit.to_id()),
            ),
        )],
        tags: Vec::new(),
    };
    let mut callback = Messages::default();
    let executable = options.executable_path.display().to_string();
    let stats = jj_lib::git::push_refs(
        tx.repo_mut(),
        options,
        RemoteName::new(&plan.remote),
        &targets,
        &mut callback,
        &GitPushOptions::default(),
    );
    drop(tx);
    let stats = match stats {
        Ok(stats) => stats,
        Err(GitPushError::NoSuchRemote(name)) => {
            return Err(PushError::RemoteNotFound {
                remote: name.as_str().to_owned(),
                remotes: Vec::new(),
            });
        }
        Err(GitPushError::RemoteName(_)) => {
            return Err(PushError::InvalidRemote(plan.remote.clone()));
        }
        Err(GitPushError::UnexpectedBackend(_)) => return Err(PushError::NoGitBackend),
        Err(GitPushError::Subprocess(error)) => {
            return Err(subprocess_error(
                &error.to_string(),
                &callback.local,
                &executable,
            ));
        }
    };
    if !stats.rejected.is_empty() {
        return Err(PushError::Moved {
            remote: plan.remote.clone(),
            branch: plan.branch.clone(),
        });
    }
    if let Some((_, reason)) = stats.remote_rejected.first() {
        return Err(PushError::RemoteRejected {
            reason: reason.as_deref().map(|text| sanitize_line(text, 500)),
        });
    }
    Ok(Pushed {
        remote_messages: callback.lines,
    })
}

/// jj-lib's subprocess error, which it does not export, by its message:
/// "Could not execute …" (spawning failed), "Git does not recognize
/// required option: …" (too old), "Git process failed: `<stderr>`".
fn subprocess_error(message: &str, local: &[String], executable: &str) -> PushError {
    if message.starts_with("Could not execute") {
        PushError::GitNotFound(executable.to_owned())
    } else if message.starts_with("Git does not recognize required option") {
        PushError::GitTooOld(sanitize_line(message, 300))
    } else {
        let errors = message
            .strip_prefix("Git process failed: ")
            .unwrap_or(message);
        let errors = errors
            .strip_prefix("External git program failed:\n")
            .unwrap_or(errors);
        let mut stderr = local.join("\n");
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str(errors);
        PushError::Git {
            failure: classify_stderr(&stderr),
            stderr: redact(&stderr),
        }
    }
}

/// Collects the remote's sideband lines, and git's own lines that jj-lib
/// leaves out of the error: it keeps only `error:` and `fatal:` lines, so
/// ssh's `Permission denied (publickey).` arrives here.
#[derive(Default)]
struct Messages {
    lines: Vec<String>,
    partial: Vec<u8>,
    local: Vec<String>,
}

/// The most of git's local lines kept for an error.
const MAX_LOCAL_LINES: usize = 40;

impl GitSubprocessCallback for Messages {
    fn needs_progress(&self) -> bool {
        false
    }

    fn progress(&mut self, _: &jj_lib::git::GitProgress) -> std::io::Result<()> {
        Ok(())
    }

    fn local_sideband(
        &mut self,
        message: &[u8],
        _: Option<GitSidebandLineTerminator>,
    ) -> std::io::Result<()> {
        let line = String::from_utf8_lossy(message).trim_end().to_owned();
        if !line.is_empty() {
            if self.local.len() == MAX_LOCAL_LINES {
                self.local.remove(0);
            }
            self.local.push(line);
        }
        Ok(())
    }

    fn remote_sideband(
        &mut self,
        message: &[u8],
        term: Option<GitSidebandLineTerminator>,
    ) -> std::io::Result<()> {
        self.partial.extend_from_slice(message);
        match term {
            // A carriage return redraws the same line (progress): drop it.
            Some(GitSidebandLineTerminator::Cr) => self.partial.clear(),
            Some(GitSidebandLineTerminator::Lf) | None => {
                let line = sanitize_line(
                    &String::from_utf8_lossy(&self.partial),
                    MAX_REMOTE_LINE_BYTES,
                );
                self.partial.clear();
                if !line.trim().is_empty() && self.lines.len() < MAX_REMOTE_LINES {
                    self.lines.push(line);
                }
            }
        }
        Ok(())
    }
}

/// `text` on one line: control characters removed (tabs become spaces),
/// trimmed, at most `max` bytes on a character boundary.
pub fn sanitize_line(text: &str, max: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect();
    truncate(cleaned.trim(), max).to_owned()
}

fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// How git's stderr reads, case-insensitively, first match wins.
pub fn classify_stderr(stderr: &str) -> GitFailure {
    const AUTH: [&str; 9] = [
        "permission denied (publickey",
        "authentication failed",
        "could not read username",
        "could not read password",
        "terminal prompts disabled",
        "host key verification failed",
        "invalid username or password",
        "returned error: 403",
        "repository not found",
    ];
    const UNREACHABLE: [&str; 7] = [
        "could not resolve host",
        "connection refused",
        "connection timed out",
        "operation timed out",
        "network is unreachable",
        "no route to host",
        "failed to connect",
    ];
    let lower = stderr.to_lowercase();
    if AUTH.iter().any(|pattern| lower.contains(pattern)) {
        GitFailure::Auth
    } else if UNREACHABLE.iter().any(|pattern| lower.contains(pattern)) {
        GitFailure::Unreachable
    } else {
        GitFailure::Other
    }
}

/// `stderr` with user information removed from URLs (`scheme://x@` becomes
/// `scheme://***@`), control characters but newline and tab removed, and
/// only its last [`MAX_STDERR_BYTES`] kept.
pub fn redact(stderr: &str) -> String {
    let mut out = String::with_capacity(stderr.len());
    let mut rest = stderr;
    while let Some(at) = rest.find("://") {
        let (before, after) = rest.split_at(at + 3);
        out.push_str(before);
        let end = after
            .find(|c: char| c.is_whitespace() || c == '/' || c == '\'' || c == '"')
            .unwrap_or(after.len());
        let authority = &after[..end];
        if let Some(sign) = authority.rfind('@') {
            out.push_str("***");
            out.push_str(&authority[sign..]);
        } else {
            out.push_str(authority);
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    let cleaned: String = out
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    let trimmed = cleaned.trim_end();
    if trimmed.len() <= MAX_STDERR_BYTES {
        return trimmed.to_owned();
    }
    let mut start = trimmed.len() - MAX_STDERR_BYTES;
    while !trimmed.is_char_boundary(start) {
        start += 1;
    }
    trimmed[start..].to_owned()
}

/// The environment every push sets: no prompts from git, Git Credential
/// Manager or ssh, HTTPS low-speed limits, no tracing, and for OpenSSH
/// batch mode and keep-alives. `env` reads the daemon's environment;
/// `ssh_command` is the repository's `core.sshCommand`.
pub fn push_environment(
    env: &dyn Fn(&str) -> Option<OsString>,
    ssh_command: Option<&str>,
) -> HashMap<OsString, OsString> {
    let mut vars: HashMap<OsString, OsString> = [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GCM_INTERACTIVE", "never"),
        ("SSH_ASKPASS_REQUIRE", "never"),
        ("GIT_HTTP_LOW_SPEED_LIMIT", "1000"),
        ("GIT_HTTP_LOW_SPEED_TIME", "60"),
        ("GIT_TRACE", "0"),
        ("GIT_TRACE_CURL", "0"),
        ("GIT_TRACE_PACKET", "0"),
        ("GIT_CURL_VERBOSE", "0"),
    ]
    .into_iter()
    .map(|(key, value)| (OsString::from(key), OsString::from(value)))
    .collect();
    if env("GIT_SSH").is_none() {
        let effective = env("GIT_SSH_COMMAND")
            .map(|value| value.to_string_lossy().into_owned())
            .or_else(|| ssh_command.map(str::to_owned))
            .unwrap_or_else(|| "ssh".to_owned());
        let program = effective.split_whitespace().next().unwrap_or_default();
        let is_ssh = std::path::Path::new(program)
            .file_name()
            .is_some_and(|name| name == "ssh");
        if is_ssh {
            vars.insert(
                OsString::from("GIT_SSH_COMMAND"),
                OsString::from(format!(
                    "{effective} -o BatchMode=yes -o ConnectTimeout=30 -o ServerAliveInterval=15 -o ServerAliveCountMax=4"
                )),
            );
        }
    }
    vars
}

/// The repository's `core.sshCommand`, if it sets one.
pub fn core_ssh_command(repo: &ReadonlyRepo) -> Option<String> {
    let git = jj_lib::git::get_git_repo(repo.store()).ok()?;
    let snapshot = git.config_snapshot();
    snapshot
        .string("core.sshCommand")
        .map(|value| value.to_string())
}

/// What [`Repo::record_push`] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedPush {
    /// The operation; `None` when the view already said so.
    pub op_id: Option<OperationIdHex>,
    /// The commit the local bookmark points at afterwards, if it was set.
    pub local: Option<CommitIdHex>,
    /// Whether the change was rewritten during the push and the local
    /// bookmark could not follow it (no single visible commit).
    pub local_left: bool,
    /// Whether someone moved the local bookmark during the push, so the
    /// record left it where they put it.
    pub local_moved: bool,
}

impl Repo {
    /// Records `plan`, which [`push`] pushed: the remote bookmark at the
    /// pushed commit, tracked, and the local bookmark at it (or at the
    /// change's single visible commit when it was rewritten meanwhile), in
    /// one operation recorded with `meta`, which this [`Repo`] adopts. No
    /// operation when the view already says so.
    pub async fn record_push(
        &mut self,
        plan: &PushPlan,
        meta: &OpMeta,
    ) -> Result<RecordedPush, PushError> {
        let pushed = plan.commit.to_id();
        let targets = self
            .repo
            .resolve_change_id(&plan.change.to_id())
            .await
            .map_err(|error| PushError::Revision(RevisionError::Other(error.into())))?;
        let visible: Vec<CommitId> = targets
            .map(|targets| {
                targets
                    .visible_with_offsets()
                    .map(|(_, id)| id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let local = if visible.contains(&pushed) {
            Some(pushed.clone())
        } else if let [only] = visible.as_slice() {
            Some(only.clone())
        } else {
            None
        };
        // A local bookmark someone moved since the plan (`jj bookmark set`
        // elsewhere; the create lock is not held across the push) is
        // theirs; one that followed a rewrite of the change is not.
        let name = RefName::new(&plan.branch);
        let now = self
            .repo
            .view()
            .get_local_bookmark(name)
            .as_normal()
            .cloned();
        let mut local_moved = false;
        if let Some(now) = &now
            && plan.local_before.as_ref().map(CommitIdHex::to_id).as_ref() != Some(now)
            && Some(now) != local.as_ref()
        {
            let held = self.repo.store().get_commit_async(now).await?;
            local_moved = ChangeIdHex::of(held.change_id()) != plan.change;
        }
        let mut tx = self.repo.start_transaction();
        tx.repo_mut().set_remote_bookmark(
            name.to_remote_symbol(RemoteName::new(&plan.remote)),
            RemoteRef {
                target: RefTarget::normal(pushed),
                state: RemoteRefState::Tracked,
            },
        );
        let set_local = plan.move_local && !local_moved && local.is_some();
        if set_local && let Some(id) = &local {
            tx.repo_mut()
                .set_local_bookmark_target(name, RefTarget::normal(id.clone()));
        }
        let outcome = RecordedPush {
            op_id: None,
            local: local.as_ref().filter(|_| set_local).map(CommitIdHex::of),
            local_left: plan.move_local && local.is_none(),
            local_moved,
        };
        if !tx.repo().has_changes() {
            return Ok(outcome);
        }
        self.repo = meta.commit(tx).await?;
        Ok(RecordedPush {
            op_id: Some(OperationIdHex::of(self.repo.op_id())),
            ..outcome
        })
    }
}

/// The description of a publish operation.
pub fn publish_description(plan: &PushPlan) -> String {
    let id: String = plan
        .change
        .reverse_hex()
        .chars()
        .take(BRANCH_ID_LETTERS)
        .collect();
    let forced = if plan.update == PushUpdate::Forced || plan.local_forced {
        " (forced)"
    } else {
        ""
    };
    format!(
        "{}{id} to {}/{}{forced}",
        super::oplog::PUBLISH_PREFIX,
        plan.remote,
        plan.branch
    )
}

/// The commits of `ids` whose content has left the machine (R1.16's flag,
/// #258): each is reachable from a remote bookmark of `repo`, or has the
/// change id and the tree of a commit that is. The second rule counts a
/// snapshot that a describe or sign-off rewrote before the push (`reef
/// publish -m`): the pushed commit carries its files under another
/// description. A snapshot whose tree differs from every pushed commit of
/// its change stays local.
pub async fn published_commits(
    repo: &ReadonlyRepo,
    ids: &[CommitIdHex],
) -> Result<HashSet<CommitIdHex>, PushError> {
    let heads: Vec<CommitId> = repo
        .view()
        .all_remote_bookmarks()
        .filter(|(symbol, _)| symbol.remote.as_str() != "git")
        .flat_map(|(_, remote_ref)| remote_ref.target.adds().flatten().cloned())
        .collect();
    if heads.is_empty() || ids.is_empty() {
        return Ok(HashSet::new());
    }
    let index = repo.index();
    let mut known = Vec::with_capacity(ids.len());
    for id in ids {
        let commit = id.to_id();
        if index.has_id(&commit).await? {
            known.push(commit);
        }
    }
    let mut found: HashSet<CommitIdHex> = reachable(repo, &heads, known.clone())
        .await?
        .iter()
        .map(CommitIdHex::of)
        .collect();
    // The rest: compare trees with the reachable commits of their change.
    let store = repo.store();
    let mut rest: HashMap<ChangeId, Vec<Commit>> = HashMap::new();
    for id in known
        .iter()
        .filter(|id| !found.contains(&CommitIdHex::of(id)))
    {
        let commit = store.get_commit_async(id).await?;
        rest.entry(commit.change_id().clone())
            .or_default()
            .push(commit);
    }
    if rest.is_empty() {
        return Ok(found);
    }
    let mut candidates = Vec::new();
    for change in rest.keys() {
        if let Some(targets) = repo.resolve_change_id(change).await? {
            candidates.extend(targets.targets.into_iter().map(|(id, _)| id));
        }
    }
    let mut pushed: HashSet<(ChangeId, Merge<TreeId>)> = HashSet::new();
    for id in reachable(repo, &heads, candidates).await? {
        let commit = store.get_commit_async(&id).await?;
        pushed.insert((commit.change_id().clone(), commit.tree_ids().clone()));
    }
    for (change, commits) in rest {
        for commit in commits {
            if pushed.contains(&(change.clone(), commit.tree_ids().clone())) {
                found.insert(CommitIdHex::of(commit.id()));
            }
        }
    }
    Ok(found)
}

/// The commits of `ids` that are ancestors of `heads`: one evaluation of
/// `ids & ::heads`, not a walk per commit and head.
async fn reachable(
    repo: &ReadonlyRepo,
    heads: &[CommitId],
    ids: Vec<CommitId>,
) -> Result<Vec<CommitId>, PushError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let expression = ResolvedRevsetExpression::commits(ids)
        .intersection(&ResolvedRevsetExpression::commits(heads.to_vec()).ancestors());
    let revset = expression.evaluate(repo)?;
    let mut stream = revset.stream();
    let mut found = Vec::new();
    while let Some(id) = stream.next().await {
        found.push(id?);
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names() {
        for good in ["reef/kxyzwqrsmtlo", "feature", "a/b-c_d.e", "x"] {
            assert!(check_branch(good).is_ok(), "{good}");
        }
        let long = "a".repeat(201);
        for bad in [
            "",
            "+main",
            "a:b",
            "-x",
            "--upload-pack=x",
            "HEAD",
            "a..b",
            "@{u}",
            "x.lock",
            "a b",
            "https://h/x",
            ".x",
            "x/",
            "x.",
            "a//b",
            "a/.b",
            long.as_str(),
        ] {
            assert!(check_branch(bad).is_err(), "{bad}");
        }
        assert!(check_branch_prefix("").is_ok());
        assert!(check_branch_prefix("reef/").is_ok());
        assert!(check_branch_prefix("-x").is_err());
    }

    #[test]
    fn remote_names() {
        for good in ["origin", "up-stream", "a.b_c"] {
            assert!(check_remote(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "git",
            "a/b",
            "https://h/x",
            "-x",
            "a b",
            "--upload-pack=x",
        ] {
            assert!(check_remote(bad).is_err(), "{bad}");
        }
        let names = vec!["origin".to_owned(), "fork".to_owned()];
        assert_eq!(choose_remote(&names, "", &[]).unwrap(), "origin");
        assert_eq!(choose_remote(&names[1..], "", &[]).unwrap(), "fork");
        assert!(matches!(
            choose_remote(&["a".to_owned(), "b".to_owned()], "", &[]),
            Err(PushError::RemoteRequired { .. })
        ));
        assert!(matches!(
            choose_remote(&names, "nope", &[]),
            Err(PushError::RemoteNotFound { .. })
        ));
        assert!(matches!(
            choose_remote(&names, "fork", &["origin".to_owned()]),
            Err(PushError::RemoteNotAllowed(_))
        ));
    }

    #[test]
    fn classification() {
        assert_eq!(
            classify_stderr(
                "git@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository."
            ),
            GitFailure::Auth
        );
        assert_eq!(
            classify_stderr(
                "fatal: could not read Username for 'https://github.com': terminal prompts disabled"
            ),
            GitFailure::Auth
        );
        assert_eq!(
            classify_stderr("ssh: Could not resolve hostname x: nodename nor servname provided"),
            GitFailure::Unreachable
        );
        assert_eq!(
            classify_stderr("fatal: unable to access 'https://x/': Could not resolve host: x"),
            GitFailure::Unreachable
        );
        assert_eq!(classify_stderr("error: something else"), GitFailure::Other);
    }

    #[test]
    fn redaction() {
        let text =
            "fatal: unable to access 'https://user:secret@github.com/o/r.git/': 403\n\u{1b}[31mred";
        let redacted = redact(text);
        assert!(!redacted.contains("secret"), "{redacted}");
        assert!(
            redacted.contains("https://***@github.com/o/r.git"),
            "{redacted}"
        );
        assert!(!redacted.contains('\u{1b}'));
        let long = "x".repeat(5_000);
        assert_eq!(redact(&long).len(), MAX_STDERR_BYTES);
    }

    #[test]
    fn sanitized_lines() {
        assert_eq!(sanitize_line("a\u{1b}[0mb\tc\r", 100), "a[0mb c");
        assert_eq!(sanitize_line("ééé", 3), "é");
    }

    #[test]
    fn environment() {
        let none = |_: &str| None;
        let vars = push_environment(&none, None);
        assert_eq!(vars[&OsString::from("GIT_TERMINAL_PROMPT")], "0");
        assert!(
            vars[&OsString::from("GIT_SSH_COMMAND")]
                .to_string_lossy()
                .starts_with("ssh -o BatchMode=yes")
        );
        let vars = push_environment(&none, Some("ssh -i key"));
        assert!(
            vars[&OsString::from("GIT_SSH_COMMAND")]
                .to_string_lossy()
                .starts_with("ssh -i key -o BatchMode=yes")
        );
        let vars = push_environment(&none, Some("plink -batch"));
        assert!(!vars.contains_key(&OsString::from("GIT_SSH_COMMAND")));
        let ssh_set = |key: &str| (key == "GIT_SSH").then(|| OsString::from("/x/ssh"));
        let vars = push_environment(&ssh_set, None);
        assert!(!vars.contains_key(&OsString::from("GIT_SSH_COMMAND")));
    }

    #[test]
    fn default_identity() {
        assert!(is_default_identity("reef", "reef@mac.local"));
        assert!(!is_default_identity("Aaron", "reef@mac.local"));
        assert!(!is_default_identity("reef", "a@b"));
    }
}
