//! Publishing a change to a git remote (#258, ADR 0070,
//! `docs/publishing.md`).
//!
//! [`publish`] runs reef-lib's three steps (`reef_lib::jj::push`): the
//! plan on the repository thread under the create lock shared, the push
//! off the repository thread with only the branch's publish lock held, and
//! the record on the repository thread under the create lock shared; then
//! R1.16's flag is cleared on the snapshots the push sent. The create lock
//! is never held across the push, so a slow remote holds back neither GC
//! nor another agent's writes, and the repository thread never waits on the
//! network.
//!
//! **The branch lock.** One `tokio` mutex per (repository, remote, branch),
//! held from the plan to the record, so two publishes of one branch run
//! one after the other. A push that outlives `[publish] timeout` keeps its
//! lock (jj-lib owns git's process, which cannot be killed from here): the
//! request returns [`PublishError::Timeout`], the push is still recorded
//! if git succeeds, and a publish of that branch meanwhile is
//! [`PublishError::InProgress`] at once.

pub mod hosts;

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt as _;
use reef_api::v1;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use reef_lib::jj::log::{ChangeInfo, Describer};
use reef_lib::jj::push::{
    self, GitFailure, PushError, PushPlan, PushRequest, PushUpdate, Pushed, RecordedPush,
    Unpublishable,
};
use reef_lib::jj::revset::RevsetScope;
use reef_lib::jj::{OpKind, OpMeta};
use tokio::sync::OwnedMutexGuard;

use crate::daemon::AppState;
use crate::error::Advice;
use crate::model::{RepoId, WorkspaceRecord};
use crate::repo::{RepoHandle, RepoThreadError};
use crate::snapshot::index;
use crate::workspace::ManagerError;

/// Why a publish failed, beyond reef-lib's [`PushError`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PublishError {
    /// Planning, pushing or recording failed.
    #[error(transparent)]
    Push(#[from] PushError),
    /// `[publish] enabled` is false.
    #[error("publishing is disabled by the daemon's [publish] enabled")]
    Disabled,
    /// The caller came through the remote listener.
    #[error("publishing is refused through the remote listener")]
    RemoteDenied,
    /// The daemon's `[identity]` is the default.
    #[error("the daemon's identity is the default reef <{email}>")]
    IdentityUnset {
        /// The default email.
        email: String,
    },
    /// The push did not finish in time.
    #[error("the push did not finish within {}", crate::config::format_duration(*.0))]
    Timeout(Duration),
    /// Another publish of the branch is still pushing.
    #[error("another publish of {remote}/{branch} is still pushing")]
    InProgress {
        /// The remote.
        remote: String,
        /// The branch.
        branch: String,
    },
    /// The repository thread failed.
    #[error(transparent)]
    Thread(#[from] RepoThreadError),
    /// The manager failed.
    #[error(transparent)]
    Manager(#[from] ManagerError),
    /// The database failed.
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
    /// The push's thread panicked.
    #[error("the push panicked")]
    Panicked,
}

impl Classify for PublishError {
    fn kind(&self) -> ErrorKind {
        match self {
            PublishError::Push(error) => error.kind(),
            PublishError::Disabled | PublishError::RemoteDenied => ErrorKind::PermissionDenied,
            PublishError::IdentityUnset { .. } => ErrorKind::Conflict,
            PublishError::Timeout(_) | PublishError::InProgress { .. } => ErrorKind::Unavailable,
            PublishError::Thread(error) => error.kind(),
            PublishError::Manager(error) => error.kind(),
            PublishError::Db(error) => error.kind(),
            PublishError::Panicked => ErrorKind::Internal,
        }
    }
}

/// What a request's errors need to read well: where the daemon runs and
/// what it was asked.
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// The daemon's pid.
    pub pid: u32,
    /// Whether the daemon's environment has `SSH_AUTH_SOCK`.
    pub ssh_auth_sock: bool,
    /// The repository's root.
    pub root: PathBuf,
    /// The revision the request named.
    pub revision: String,
    /// The daemon's config file.
    pub config_path: PathBuf,
    /// The configured remotes, when known.
    pub remotes: Vec<String>,
}

impl Context {
    /// The context of a request in `state` naming `revision` in `root`.
    pub fn new(state: &AppState, root: PathBuf, revision: &str) -> Context {
        Context {
            pid: std::process::id(),
            ssh_auth_sock: std::env::var_os("SSH_AUTH_SOCK").is_some_and(|value| !value.is_empty()),
            root,
            revision: revision.to_owned(),
            config_path: state.config.paths.config_file.clone(),
            remotes: Vec::new(),
        }
    }
}

/// `error` as a status with its reason, metadata, hint and `next`.
pub fn status(error: &PublishError, context: &Context) -> tonic::Status {
    let kind = error.kind();
    let Some((reason, metadata, advice)) = describe(error, context) else {
        return crate::error::status_from(error, kind);
    };
    let message = ErrorChain(error).to_string();
    if !kind.is_caller_error() {
        tracing::error!(error = %message, ?kind, "publish failed");
    }
    let detail = v1::ErrorDetail {
        kind: crate::error::proto_kind(kind).into(),
        reason: reason.to_owned(),
        chain: crate::error::chain(error),
        hint: advice.hint,
        next: advice.next,
        metadata,
    };
    reef_api::error::status(crate::error::code_of(kind), message, &detail)
}

type Described = (&'static str, BTreeMap<String, String>, Advice);

fn meta(pairs: &[(&str, String)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

/// The reason, metadata and advice of a publish error; `None` for one the
/// daemon's general mapping reads (revisions, rewrites, the store).
fn describe(error: &PublishError, context: &Context) -> Option<Described> {
    use reef_api::error as reason;
    let hint = |text: String| Advice {
        hint: text,
        next: Vec::new(),
    };
    Some(match error {
        PublishError::Disabled => (
            reason::PUBLISH_DISABLED,
            BTreeMap::new(),
            hint(format!(
                "The daemon's [publish] enabled is false in {}; set it to true and restart the daemon to publish.",
                context.config_path.display()
            )),
        ),
        PublishError::RemoteDenied => (
            reason::REMOTE_PUBLISH_DENIED,
            BTreeMap::new(),
            hint("Publish from the daemon's machine: a remote token never reaches the user's git credentials.".to_owned()),
        ),
        PublishError::IdentityUnset { email } => (
            reason::IDENTITY_UNSET,
            BTreeMap::new(),
            hint(format!(
                "Publishing and sign-offs use the daemon's identity, which is the default reef <{email}>. Set [identity] name and email in {}, then restart the daemon (`reef daemon stop`; the next reef command starts it).",
                context.config_path.display()
            )),
        ),
        PublishError::Timeout(after) => (
            reason::PUBLISH_TIMEOUT,
            BTreeMap::new(),
            hint(format!(
                "The push is still running after {}; it is recorded if git succeeds. Check `reef op log --kind publish` later, or raise [publish] timeout.",
                crate::config::format_duration(*after)
            )),
        ),
        PublishError::InProgress { remote, branch } => (
            reason::PUBLISH_IN_PROGRESS,
            meta(&[("git_remote", remote.clone()), ("branch", branch.clone())]),
            hint("An earlier publish of this branch is still pushing; publish again when it ends.".to_owned()),
        ),
        PublishError::Push(error) => return describe_push(error, context),
        _ => return None,
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "one arm per reason, each with its hint"
)]
fn describe_push(error: &PushError, context: &Context) -> Option<Described> {
    use reef_api::error as reason;
    let hint = |text: String| Advice {
        hint: text,
        next: Vec::new(),
    };
    let with = |text: String, next: Vec<String>| Advice { hint: text, next };
    let commit = |offending: &push::Offending| {
        meta(&[
            ("change_id", offending.change.to_string()),
            ("commit_id", offending.commit.to_string()),
        ])
    };
    let short = |change: &reef_lib::jj::ChangeIdHex| {
        change.reverse_hex().chars().take(12).collect::<String>()
    };
    Some(match error {
        PushError::Empty(offending) => {
            let next = if context.revision.trim() == "@" {
                argv(&["reef", "publish", "@-"])
            } else {
                Vec::new()
            };
            (
                reason::CHANGE_EMPTY,
                meta(&[("change_id", offending.change.to_string())]),
                with(
                    "The change changes nothing. Edit files first, or publish its parent.".to_owned(),
                    next,
                ),
            )
        }
        PushError::Undescribed(offending) => (
            reason::CHANGE_UNDESCRIBED,
            commit(offending),
            hint(if offending.is_target {
                format!(
                    "A published commit needs a description: `reef publish {} -m \"<summary>\"`.",
                    context.revision.trim()
                )
            } else {
                format!(
                    "An ancestor the remote lacks has no description: `reef describe {} -m \"<summary>\"`, then publish again.",
                    short(&offending.change)
                )
            }),
        ),
        PushError::Conflicted(offending) => (
            reason::CHANGE_CONFLICTED,
            commit(offending),
            hint(format!(
                "Commit {} has conflicts; resolve them (`reef conflict show {}`) before publishing.",
                offending.commit,
                short(&offending.change)
            )),
        ),
        PushError::Divergent(offending) => (
            reason::CHANGE_DIVERGENT,
            commit(offending),
            with(
                "The change has several visible commits; abandon all but one before publishing."
                    .to_owned(),
                vec![
                    "reef".to_owned(),
                    "log".to_owned(),
                    "-r".to_owned(),
                    format!("change_id({})", offending.change),
                ],
            ),
        ),
        PushError::IdentityMissing(offending) => (
            reason::COMMIT_IDENTITY_MISSING,
            commit(offending),
            hint("A commit in the range has no author or committer name or email; git hosts refuse it.".to_owned()),
        ),
        PushError::AuthorDefault(offending) => (
            reason::COMMIT_AUTHOR_DEFAULT,
            commit(offending),
            with(
                "This commit was written before the daemon's [identity] was set and is authored as reef <reef@host>; reset its author first.".to_owned(),
                vec![
                    "reef".to_owned(),
                    "describe".to_owned(),
                    short(&offending.change),
                    "--reset-author".to_owned(),
                ],
            ),
        ),
        PushError::IncludesWorkingCopy {
            offending,
            workspace,
        } => {
            let id = workspace
                .strip_prefix(reef_lib::jj::workspace::REEF_WORKSPACE_PREFIX)
                .map(str::to_owned);
            let mut metadata = commit(offending);
            metadata.insert(
                "workspace".to_owned(),
                id.clone().unwrap_or_else(|| workspace.clone()),
            );
            let next = id
                .as_ref()
                .map(|id| {
                    vec![
                        "reef".to_owned(),
                        "publish".to_owned(),
                        "--workspace".to_owned(),
                        id.clone(),
                        context.revision.trim().to_owned(),
                    ]
                })
                .unwrap_or_default();
            (
                reason::INCLUDES_WORKING_COPY,
                metadata,
                with(
                    format!(
                        "Commit {} is the working copy of workspace {}, which may still be writing it; publish from that workspace, or wait until it moves on.",
                        offending.commit,
                        id.as_deref().unwrap_or(workspace)
                    ),
                    next,
                ),
            )
        }
        PushError::RangeTooLarge { max } => (
            reason::RANGE_TOO_LARGE,
            meta(&[("count", format!("more than {max}"))]),
            hint("The remote has almost none of this history; push the repository's trunk to it once with git, then publish.".to_owned()),
        ),
        PushError::NoGitBackend => (
            reason::NO_GIT_BACKEND,
            BTreeMap::new(),
            hint("This repository has no git backend, so it has no git remotes to publish to.".to_owned()),
        ),
        PushError::RemoteRequired { remotes } => {
            let next = remotes
                .first()
                .map(|first| {
                    vec![
                        "reef".to_owned(),
                        "publish".to_owned(),
                        context.revision.trim().to_owned(),
                        "--git-remote".to_owned(),
                        first.clone(),
                    ]
                })
                .unwrap_or_default();
            let text = if remotes.is_empty() {
                format!(
                    "The repository has no git remote; add one (`git -C {} remote add origin <url>`), then publish.",
                    context.root.display()
                )
            } else {
                format!(
                    "Name the git remote with --git-remote: the repository has {}.",
                    remotes.join(", ")
                )
            };
            (
                reason::GIT_REMOTE_REQUIRED,
                meta(&[("remotes", remotes.join(","))]),
                with(text, next),
            )
        }
        PushError::InvalidRemote(name) => (
            reason::INVALID_GIT_REMOTE,
            meta(&[("git_remote", name.clone())]),
            hint("Name a git remote the repository configures (`git remote`); a URL is never accepted.".to_owned()),
        ),
        PushError::RemoteNotFound { remote, remotes } => {
            let remotes = if remotes.is_empty() {
                &context.remotes
            } else {
                remotes
            };
            (
                reason::GIT_REMOTE_NOT_FOUND,
                meta(&[("git_remote", remote.clone()), ("remotes", remotes.join(","))]),
                hint(if remotes.is_empty() {
                    "The repository configures no git remote.".to_owned()
                } else {
                    format!("The repository's git remotes are {}.", remotes.join(", "))
                }),
            )
        }
        PushError::RemoteNotAllowed(remote) => (
            reason::GIT_REMOTE_NOT_ALLOWED,
            meta(&[("git_remote", remote.clone())]),
            hint(format!(
                "The daemon's [publish] remotes in {} does not list {remote}.",
                context.config_path.display()
            )),
        ),
        PushError::InvalidBranch(branch) => (
            reason::INVALID_BRANCH,
            meta(&[("branch", branch.clone())]),
            hint("A branch is 1 to 200 bytes of letters, digits, `.`, `_`, `/` and `-`, not starting with `-`, `.` or `/`.".to_owned()),
        ),
        PushError::NotPublishable { branch, why } => (
            reason::BRANCH_NOT_PUBLISHABLE,
            meta(&[("branch", branch.clone()), ("why", why.as_str().to_owned())]),
            hint(match why {
                Unpublishable::Trunk | Unpublishable::RemoteHead => format!(
                    "{branch} is the repository's trunk; publish to a review branch (omit --branch, or name another), and land with `reef land`."
                ),
                Unpublishable::Immutable => format!(
                    "{branch} on the remote points at an immutable commit; publish to another branch with --branch."
                ),
                Unpublishable::CheckedOut => format!(
                    "{branch} is checked out in the repository's git working tree; publish to another branch with --branch."
                ),
            }),
        ),
        PushError::Diverged { branch, foreign } => {
            let text = if foreign.local {
                format!(
                    "The local bookmark {branch} is at {} ({}: {}), which is not part of this change. Publish to another branch with --branch, or pass --force to move it.",
                    foreign.commit, foreign.author, foreign.summary
                )
            } else {
                format!(
                    "{branch} on the remote is at {} ({}: {}), which is not part of this change. Rebase onto it, publish to another branch with --branch, or pass --force to replace it.",
                    foreign.commit, foreign.author, foreign.summary
                )
            };
            (
                reason::BRANCH_DIVERGED,
                meta(&[
                    ("branch", branch.clone()),
                    ("remote_commit_id", foreign.commit.to_string()),
                    ("remote_change_id", foreign.change.to_string()),
                    ("remote_author", foreign.author.clone()),
                    ("remote_summary", foreign.summary.clone()),
                    ("remote_hidden", foreign.hidden.to_string()),
                ]),
                with(
                    text,
                    vec![
                        "reef".to_owned(),
                        "show".to_owned(),
                        foreign.commit.to_string(),
                    ],
                ),
            )
        }
        PushError::BranchConflicted(branch) => (
            reason::BRANCH_CONFLICTED,
            meta(&[("branch", branch.clone())]),
            hint(format!(
                "The remote-tracking bookmark of {branch} is conflicted; fetch it again (`jj -R {} git fetch`) or set it with `jj bookmark set`.",
                context.root.display()
            )),
        ),
        PushError::Moved { remote, branch } => (
            reason::BRANCH_MOVED,
            meta(&[("git_remote", remote.clone()), ("branch", branch.clone())]),
            hint(format!(
                "{remote}/{branch} moved, or was deleted, since Reef last saw it. Fetch it (`jj -R {root} git fetch --remote {remote} --branch {branch}`, which also forgets a branch deleted on the host; or `git -C {root} fetch {remote} {branch}`), then publish again; if the branch holds someone else's work you will be asked for --force.",
                root = context.root.display()
            )),
        ),
        PushError::RemoteRejected { reason: why } => (
            reason::REMOTE_REJECTED,
            meta(&[("remote_reason", why.clone().unwrap_or_default())]),
            hint("The remote refused the push (a protected branch, a declined hook, or no permission to the branch); see remote_reason.".to_owned()),
        ),
        PushError::Git { failure, stderr } => match failure {
            GitFailure::Auth => (
                reason::GIT_AUTH_FAILED,
                meta(&[
                    ("git_stderr", stderr.clone()),
                    (
                        "ssh_auth_sock",
                        if context.ssh_auth_sock { "set" } else { "unset" }.to_owned(),
                    ),
                ]),
                hint(format!(
                    "The push runs in the daemon (pid {}) with its environment: SSH_AUTH_SOCK is {}. Load your key into an agent the daemon can reach, or configure a credential helper, then publish again; a daemon started from another shell has that shell's environment (`reef daemon stop`, then any reef command, restarts it from this one).",
                    context.pid,
                    if context.ssh_auth_sock { "set" } else { "unset" }
                )),
            ),
            GitFailure::Unreachable => (
                reason::GIT_UNREACHABLE,
                meta(&[("git_stderr", stderr.clone())]),
                hint("The git remote could not be reached; check the network and the remote's URL (`git remote -v`).".to_owned()),
            ),
            GitFailure::Other => (
                reason::GIT_PUSH_FAILED,
                meta(&[("git_stderr", stderr.clone())]),
                hint("git push failed; git_stderr has its message.".to_owned()),
            ),
        },
        PushError::GitNotFound(path) => (
            reason::GIT_NOT_FOUND,
            BTreeMap::new(),
            hint(format!(
                "The daemon cannot run git ({path}); install git, or set [gc] git_executable in {}.",
                context.config_path.display()
            )),
        ),
        PushError::GitTooOld(_) => (
            reason::GIT_TOO_OLD,
            BTreeMap::new(),
            hint("Publishing needs git 2.42 or newer; upgrade git, or set [gc] git_executable to a newer one.".to_owned()),
        ),
        _ => return None,
    })
}

/// One branch's publish lock, and whether a push past its timeout holds it.
#[derive(Default)]
struct BranchLock {
    mutex: Arc<tokio::sync::Mutex<()>>,
    stuck: AtomicBool,
}

type LockKey = (RepoId, String, String);

/// The publish locks of the process: per (repository, remote, branch).
fn locks() -> &'static Mutex<HashMap<LockKey, Arc<BranchLock>>> {
    static LOCKS: std::sync::OnceLock<Mutex<HashMap<LockKey, Arc<BranchLock>>>> =
        std::sync::OnceLock::new();
    LOCKS.get_or_init(Mutex::default)
}

fn lock_for(key: LockKey) -> Arc<BranchLock> {
    let mut locks = locks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Drop the locks nobody holds, so the map never grows past the branches
    // being published.
    locks.retain(|_, lock| Arc::strong_count(lock) > 1 || Arc::strong_count(&lock.mutex) > 1);
    Arc::clone(locks.entry(key).or_default())
}

/// What [`publish`] needs.
#[derive(Clone, Debug)]
pub struct PublishInput {
    /// The repository.
    pub repo: RepoId,
    /// Its thread.
    pub handle: RepoHandle,
    /// The workspace the request names.
    pub workspace: Option<WorkspaceRecord>,
    /// The email revsets' `mine()` reads.
    pub user_email: String,
    /// The repository's root.
    pub root: PathBuf,
    /// The plan's request.
    pub request: PushRequest,
    /// Who asked.
    pub agent: Option<String>,
    /// Plan only.
    pub dry_run: bool,
    /// `[publish] timeout`.
    pub timeout: Duration,
}

/// What [`publish`] did.
#[derive(Clone, Debug)]
pub struct PublishOutcome {
    /// The plan.
    pub plan: PushPlan,
    /// The published commit, as described by the plan job.
    pub info: Option<ChangeInfo>,
    /// The push; `None` for a dry run.
    pub pushed: Option<Pushed>,
    /// The record; `None` for a dry run.
    pub recorded: Option<RecordedPush>,
    /// How many snapshot rows the publish marked as having left the machine.
    pub published_rows: u64,
}

/// Plans, pushes and records `input`'s publish.
///
/// The push and the record run in a task that owns the branch's lock, so a
/// request that is cancelled (its client went away) or times out never
/// leaves a push unrecorded or its branch unlocked while git runs.
pub async fn publish(
    state: &Arc<AppState>,
    input: PublishInput,
) -> Result<PublishOutcome, PublishError> {
    let (first, info, _) = plan_job(state, &input).await?;
    if input.dry_run {
        return Ok(PublishOutcome {
            plan: first,
            info,
            pushed: None,
            recorded: None,
            published_rows: 0,
        });
    }
    let deadline = tokio::time::Instant::now() + input.timeout;
    let mut key = (first.remote, first.branch);
    // Planned again under the lock: a publish of the branch that ran
    // meanwhile moved what this one leases on. If that plan names another
    // branch (the revision moved to another change), its lock is taken
    // instead.
    for _ in 0..3 {
        let lock = lock_for((input.repo, key.0.clone(), key.1.clone()));
        let guard = acquire(&lock, &key, deadline).await?;
        let (plan, info, readonly) = plan_job(state, &input).await?;
        if plan.remote != key.0 || plan.branch != key.1 {
            key = (plan.remote, plan.branch);
            continue;
        }
        return push_and_record(state, &input, plan, info, readonly, (lock, guard), deadline).await;
    }
    Err(PublishError::InProgress {
        remote: key.0,
        branch: key.1,
    })
}

/// The branch's lock, waiting at most until `deadline`; at once
/// [`PublishError::InProgress`] while a push past its timeout holds it.
async fn acquire(
    lock: &Arc<BranchLock>,
    key: &(String, String),
    deadline: tokio::time::Instant,
) -> Result<OwnedMutexGuard<()>, PublishError> {
    let busy = || PublishError::InProgress {
        remote: key.0.clone(),
        branch: key.1.clone(),
    };
    loop {
        if lock.stuck.load(Ordering::Acquire) {
            return Err(busy());
        }
        if let Ok(guard) = Arc::clone(&lock.mutex).try_lock_owned() {
            return Ok(guard);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(busy());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Pushes and records `plan` in a task that holds `held`'s guard until it
/// ends; waits for it until `deadline`.
async fn push_and_record(
    state: &Arc<AppState>,
    input: &PublishInput,
    plan: PushPlan,
    info: Option<ChangeInfo>,
    readonly: Arc<reef_lib::jj::ReadonlyRepo>,
    held: (Arc<BranchLock>, OwnedMutexGuard<()>),
    deadline: tokio::time::Instant,
) -> Result<PublishOutcome, PublishError> {
    let (lock, guard) = held;
    let mut work = {
        let state = Arc::clone(state);
        let input = input.clone();
        let plan = plan.clone();
        let lock = Arc::clone(&lock);
        tokio::spawn(async move {
            let _guard = guard;
            let pushing = {
                let plan = plan.clone();
                tokio::task::spawn_blocking(move || push::push(&readonly, &plan, &env))
            };
            let result = match pushing.await {
                Ok(Ok(pushed)) => record(&state, &input, &plan)
                    .await
                    .map(|(recorded, rows)| (pushed, recorded, rows)),
                Ok(Err(error)) => Err(PublishError::from(error)),
                Err(_) => Err(PublishError::Panicked),
            };
            if lock.stuck.swap(false, Ordering::AcqRel) {
                match &result {
                    Ok(_) => tracing::info!(
                        git_remote = %plan.remote,
                        branch = %plan.branch,
                        "a push that outlived its request finished and was recorded"
                    ),
                    Err(error) => tracing::warn!(
                        error = %ErrorChain(error),
                        "a push that outlived its request did not finish"
                    ),
                }
            }
            result
        })
    };
    match tokio::time::timeout_at(deadline, &mut work).await {
        Ok(Ok(Ok((pushed, recorded, published_rows)))) => Ok(PublishOutcome {
            plan,
            info,
            pushed: Some(pushed),
            recorded: Some(recorded),
            published_rows,
        }),
        Ok(Ok(Err(error))) => Err(error),
        Ok(Err(_)) => Err(PublishError::Panicked),
        Err(_) => {
            lock.stuck.store(true, Ordering::Release);
            // The task may have ended between the timeout and the store;
            // it clears the flag only while it runs.
            if work.is_finished() {
                lock.stuck.store(false, Ordering::Release);
            }
            tracing::warn!(
                git_remote = %plan.remote,
                branch = %plan.branch,
                "a push outlived [publish] timeout; it is recorded if git succeeds"
            );
            Err(PublishError::Timeout(input.timeout))
        }
    }
}

/// The plan job on the repository thread, under the create lock shared:
/// imports git refs, then plans.
async fn plan_job(
    state: &AppState,
    input: &PublishInput,
) -> Result<
    (
        PushPlan,
        Option<ChangeInfo>,
        Arc<reef_lib::jj::ReadonlyRepo>,
    ),
    PublishError,
> {
    let create_lock = state.manager.repo_lock(input.repo);
    let _shared = create_lock.read().await;
    let workspace = input.workspace.as_ref().map(|record| record.id);
    let agent = input
        .agent
        .clone()
        .or_else(|| input.workspace.as_ref().map(|record| record.owner.clone()));
    let request = input.request.clone();
    let user_email = input.user_email.clone();
    let root = input.root.clone();
    let planned = input
        .handle
        .call_async(move |head| {
            async move {
                crate::workspace::refresh_for_resolve(
                    head,
                    &crate::workspace::import_meta(workspace, agent.as_deref()),
                )
                .await
                .map_err(PublishError::from)?;
                // Read the git configuration as it is now: a remote added
                // since the daemon loaded the repository counts.
                let repo =
                    push::with_current_git_config(&head.repo.readonly(), head.repo.repo_path())
                        .await?;
                let jj_name = workspace.map(|id| id.jj_workspace_name());
                let scope = RevsetScope {
                    workspace: jj_name.as_deref(),
                    user_email: &user_email,
                    root: &root,
                };
                let plan = push::plan_push(&repo, &request, &scope).await?;
                let info = match Describer::new(&repo, &scope) {
                    Ok(describer) => describer.describe(&plan.commit).await.ok(),
                    Err(_) => None,
                };
                Ok::<_, PublishError>((plan, info, repo))
            }
            .boxed_local()
        })
        .await??;
    Ok(planned)
}

/// The record job, then R1.16's flag.
async fn record(
    state: &AppState,
    input: &PublishInput,
    plan: &PushPlan,
) -> Result<(RecordedPush, u64), PublishError> {
    let mut meta = OpMeta::new(OpKind::Publish, push::publish_description(plan));
    meta.workspace = input.workspace.as_ref().map(|record| record.id.as_hex());
    meta.agent = input
        .agent
        .clone()
        .or_else(|| input.workspace.as_ref().map(|record| record.owner.clone()));
    // The local rows of the published changes, read before the record so
    // the repository thread can tell which of them the push carried.
    let candidates = {
        let db = Arc::clone(&state.db);
        let repo = input.repo;
        let changes: Vec<_> = plan
            .range
            .iter()
            .map(|commit| commit.change.clone())
            .collect();
        tokio::task::spawn_blocking(move || index::local_commits_of(&db, repo, &changes))
            .await
            .map_err(|_| PublishError::Panicked)??
    };
    let (recorded, commits) = {
        let create_lock = state.manager.repo_lock(input.repo);
        let _shared = create_lock.read().await;
        let plan = plan.clone();
        input
            .handle
            .call_async(move |head| {
                async move {
                    head.refresh().await.map_err(ManagerError::from)?;
                    let recorded = head.repo.record_push(&plan, &meta).await?;
                    let pushed = plan.range.iter().map(|commit| commit.commit.clone());
                    // Snapshots a describe rewrote before the push carry the
                    // pushed tree: their content left too.
                    let commits: Vec<_> =
                        match push::published_commits(&head.repo.readonly(), &candidates).await {
                            Ok(found) => pushed.chain(found).collect(),
                            Err(error) => {
                                tracing::warn!(
                                    error = %ErrorChain(&error),
                                    "cannot tell which earlier snapshots the publish carried"
                                );
                                pushed.collect()
                            }
                        };
                    Ok::<_, PublishError>((recorded, commits))
                }
                .boxed_local()
            })
            .await??
    };
    let db = Arc::clone(&state.db);
    let repo = input.repo;
    let published = tokio::task::spawn_blocking(move || index::mark_published(&db, repo, &commits))
        .await
        .map_err(|_| PublishError::Panicked)??;
    tracing::info!(
        git_remote = %plan.remote,
        branch = %plan.branch,
        update = plan.update.as_str(),
        commit_count = plan.range.len(),
        published_count = published,
        "change published"
    );
    Ok((recorded, published))
}

/// The process environment variable `name`.
pub fn env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// Whether `update` writes nothing new on the remote.
pub fn is_noop(update: PushUpdate) -> bool {
    update == PushUpdate::UpToDate
}
