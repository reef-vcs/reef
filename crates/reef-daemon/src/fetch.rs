//! Fetching from git remotes (#259, ADR 0071): `GitService.FetchGitRemote`
//! and `RebaseChange`'s first step, over `reef_lib::jj::fetch`.
//!
//! The plan and the record are repository-thread jobs under the create
//! lock shared; git's network call holds only this module's lock for the
//! (repository, remote), never the repository thread or the create lock
//! (S-182). The fetch and its record run in a task that owns that lock, so
//! a request that times out or is cancelled never leaves a fetch
//! unrecorded or its remote locked while git runs.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt as _;
use reef_api::v1;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use reef_lib::jj::fetch::{self, FetchError, FetchPlan, FetchedRefs, RecordedFetch};
use reef_lib::jj::push::{self, GitFailure, PushError};
use reef_lib::jj::{OpKind, OpMeta};
use tokio::sync::OwnedMutexGuard;

use crate::daemon::AppState;
use crate::error::{Advice, Advise};
use crate::model::{RepoId, WorkspaceRecord};
use crate::repo::{RepoHandle, RepoThreadError};
use crate::workspace::ManagerError;

/// Why a fetch failed, beyond reef-lib's [`FetchError`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FetchFailure {
    /// Planning, fetching or recording failed.
    #[error(transparent)]
    Fetch(#[from] FetchError),
    /// `[fetch] enabled` is false.
    #[error("fetching is disabled by the daemon's [fetch] enabled")]
    Disabled {
        /// The daemon's config file.
        config: std::path::PathBuf,
    },
    /// The caller came through the remote listener.
    #[error("fetching is refused through the remote listener")]
    RemoteDenied,
    /// The fetch did not finish in time.
    #[error("the fetch did not finish within {}", crate::config::format_duration(*.0))]
    Timeout(Duration),
    /// Another fetch from the remote is still running.
    #[error("another fetch from {0} is still running")]
    InProgress(String),
    /// The repository thread failed.
    #[error(transparent)]
    Thread(#[from] RepoThreadError),
    /// The manager failed.
    #[error(transparent)]
    Manager(#[from] ManagerError),
    /// The fetch's thread panicked.
    #[error("the fetch panicked")]
    Panicked,
}

impl From<PushError> for FetchFailure {
    fn from(error: PushError) -> Self {
        FetchFailure::Fetch(FetchError::Plan(error))
    }
}

impl Classify for FetchFailure {
    fn kind(&self) -> ErrorKind {
        match self {
            FetchFailure::Fetch(error) => error.kind(),
            FetchFailure::Disabled { .. } | FetchFailure::RemoteDenied => {
                ErrorKind::PermissionDenied
            }
            FetchFailure::Timeout(_) | FetchFailure::InProgress(_) => ErrorKind::Unavailable,
            FetchFailure::Thread(error) => error.kind(),
            FetchFailure::Manager(error) => error.kind(),
            FetchFailure::Panicked => ErrorKind::Internal,
        }
    }
}

impl FetchFailure {
    /// Its reason and metadata, as `crate::error` reports them; `None` for
    /// a failure the source chain names.
    pub(crate) fn reason(&self) -> Option<(&'static str, Vec<(&'static str, String)>)> {
        use reef_api::error as reason;
        Some(match self {
            FetchFailure::Disabled { .. } => (reason::FETCH_DISABLED, Vec::new()),
            FetchFailure::RemoteDenied => (reason::REMOTE_FETCH_DENIED, Vec::new()),
            FetchFailure::Timeout(_) => (reason::FETCH_TIMEOUT, Vec::new()),
            FetchFailure::InProgress(remote) => (
                reason::FETCH_IN_PROGRESS,
                vec![("git_remote", remote.clone())],
            ),
            FetchFailure::Fetch(error) => return fetch_reason(error),
            _ => return None,
        })
    }
}

fn fetch_reason(error: &FetchError) -> Option<(&'static str, Vec<(&'static str, String)>)> {
    use reef_api::error as reason;
    Some(match error {
        FetchError::Plan(PushError::NoGitBackend) => (reason::NO_GIT_BACKEND, Vec::new()),
        FetchError::Plan(PushError::RemoteRequired { remotes }) => (
            reason::GIT_REMOTE_REQUIRED,
            vec![("remotes", remotes.join(","))],
        ),
        FetchError::Plan(PushError::InvalidRemote(name)) => (
            reason::INVALID_GIT_REMOTE,
            vec![("git_remote", name.clone())],
        ),
        FetchError::Plan(PushError::RemoteNotFound { remote, remotes }) => (
            reason::GIT_REMOTE_NOT_FOUND,
            vec![
                ("git_remote", remote.clone()),
                ("remotes", remotes.join(",")),
            ],
        ),
        FetchError::Plan(PushError::InvalidBranch(branch)) => {
            (reason::INVALID_BRANCH, vec![("branch", branch.clone())])
        }
        FetchError::TooManyBranches => (reason::INVALID_BRANCH, Vec::new()),
        FetchError::NoBranch { remote } => {
            (reason::INVALID_BRANCH, vec![("git_remote", remote.clone())])
        }
        FetchError::Git { failure, stderr } => (
            match failure {
                GitFailure::Auth => reason::GIT_AUTH_FAILED,
                GitFailure::Unreachable => reason::GIT_UNREACHABLE,
                GitFailure::Other => reason::GIT_FETCH_FAILED,
            },
            vec![("git_stderr", stderr.clone())],
        ),
        FetchError::Rejected(refs) => {
            (reason::GIT_FETCH_FAILED, vec![("git_stderr", refs.clone())])
        }
        FetchError::GitNotFound(path) => (reason::GIT_NOT_FOUND, vec![("git", path.clone())]),
        FetchError::GitTooOld(_) => (reason::GIT_TOO_OLD, Vec::new()),
        _ => return None,
    })
}

impl Advise for FetchFailure {
    fn advice(&self) -> Option<Advice> {
        match self {
            FetchFailure::Disabled { config } => Some(Advice::hint(format!(
                "The daemon's [fetch] enabled is false in {}; `reef rebase --no-fetch` rebases onto the trunk Reef last saw.",
                config.display()
            ))),
            FetchFailure::RemoteDenied => Some(Advice::hint(
                "Fetch from the daemon's machine: a remote token never reaches the user's git credentials.",
            )),
            FetchFailure::Timeout(after) => Some(Advice::hint(format!(
                "Git is still fetching after {}; it is recorded if it succeeds. Fetch again later, or raise [fetch] timeout.",
                crate::config::format_duration(*after)
            ))),
            FetchFailure::InProgress(_) => Some(Advice::hint(
                "An earlier fetch from this remote is still running; fetch again when it ends.",
            )),
            FetchFailure::Fetch(error) => fetch_advice(error),
            _ => None,
        }
    }
}

fn fetch_advice(error: &FetchError) -> Option<Advice> {
    match error {
        FetchError::Plan(PushError::NoGitBackend) => Some(Advice::hint(
            "The repository has no git backend, so there is nothing to fetch from; rebase with `--no-fetch`.",
        )),
        FetchError::Plan(PushError::RemoteRequired { remotes }) => {
            Some(Advice::hint(if remotes.is_empty() {
                "The repository has no git remote; add one with `git remote add origin <url>`, or rebase with `--no-fetch`.".to_owned()
            } else {
                format!(
                    "Several remotes and none is `origin` ({}): fetch one with `reef fetch --git-remote <name>`, or rebase onto `<branch>@<name>` with `--onto`.",
                    remotes.join(", ")
                )
            }))
        }
        FetchError::Plan(PushError::RemoteNotFound { remotes, .. }) => Some(Advice::hint(format!(
            "The repository's git remotes are: {}.",
            if remotes.is_empty() {
                "none".to_owned()
            } else {
                remotes.join(", ")
            }
        ))),
        FetchError::NoBranch { .. } => Some(Advice::hint(
            "Name the branch with `--branch`, or create a `main` bookmark for trunk().",
        )),
        FetchError::Git {
            failure: GitFailure::Auth,
            ..
        } => Some(Advice::hint(
            "Git could not authenticate as the daemon: it runs with the daemon's environment and never prompts. Check that `git fetch` works from a terminal, that the daemon's environment has your SSH agent (SSH_AUTH_SOCK) or a credential helper, then restart the daemon; or rebase with `--no-fetch`.",
        )),
        FetchError::Git {
            failure: GitFailure::Unreachable,
            ..
        } => Some(Advice::hint(
            "The remote could not be reached. Rebase onto the trunk Reef last saw with `--no-fetch`, or retry when online.",
        )),
        FetchError::GitNotFound(_) => Some(Advice::hint(
            "Install git, or point jj's `git.executable-path` at it.",
        )),
        FetchError::GitTooOld(_) => Some(Advice::hint(
            "Upgrade git: jj-lib's fetch needs a newer one.",
        )),
        _ => None,
    }
}

struct RemoteLock {
    mutex: Arc<tokio::sync::Mutex<()>>,
    stuck: AtomicBool,
}

type LockKey = (RepoId, String);

/// The fetch locks of the process: per (repository, remote).
fn locks() -> &'static Mutex<HashMap<LockKey, Arc<RemoteLock>>> {
    static LOCKS: std::sync::OnceLock<Mutex<HashMap<LockKey, Arc<RemoteLock>>>> =
        std::sync::OnceLock::new();
    LOCKS.get_or_init(Mutex::default)
}

fn lock_for(key: LockKey) -> Arc<RemoteLock> {
    let mut locks = locks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|_, lock| Arc::strong_count(lock) > 1 || Arc::strong_count(&lock.mutex) > 1);
    Arc::clone(locks.entry(key).or_insert_with(|| {
        Arc::new(RemoteLock {
            mutex: Arc::default(),
            stuck: AtomicBool::new(false),
        })
    }))
}

/// What [`fetch()`] needs.
#[derive(Clone, Debug)]
pub struct FetchInput {
    /// The repository.
    pub repo: RepoId,
    /// Its thread.
    pub handle: RepoHandle,
    /// The workspace the request names.
    pub workspace: Option<WorkspaceRecord>,
    /// Who asked.
    pub agent: Option<String>,
    /// The remote; empty to choose.
    pub remote: String,
    /// The branches; empty for trunk's.
    pub branches: Vec<String>,
}

/// Plans, fetches and records `input`'s fetch. Refused when `[fetch]
/// enabled` is false.
pub async fn fetch(
    state: &Arc<AppState>,
    input: FetchInput,
) -> Result<RecordedFetch, FetchFailure> {
    if !state.config.file.fetch.enabled {
        return Err(FetchFailure::Disabled {
            config: state.config.paths.config_file.clone(),
        });
    }
    let timeout = state.config.file.fetch.timeout;
    let deadline = tokio::time::Instant::now() + timeout;
    let (plan, readonly) = plan_job(state, &input).await?;
    let lock = lock_for((input.repo, plan.remote.clone()));
    let guard = acquire(&lock, &plan.remote, deadline).await?;
    fetch_and_record(
        state,
        &input,
        plan,
        readonly,
        (lock, guard),
        (deadline, timeout),
    )
    .await
}

async fn acquire(
    lock: &Arc<RemoteLock>,
    remote: &str,
    deadline: tokio::time::Instant,
) -> Result<OwnedMutexGuard<()>, FetchFailure> {
    loop {
        if lock.stuck.load(Ordering::Acquire) {
            return Err(FetchFailure::InProgress(remote.to_owned()));
        }
        if let Ok(guard) = Arc::clone(&lock.mutex).try_lock_owned() {
            return Ok(guard);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(FetchFailure::InProgress(remote.to_owned()));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn fetch_and_record(
    state: &Arc<AppState>,
    input: &FetchInput,
    plan: FetchPlan,
    readonly: Arc<reef_lib::jj::ReadonlyRepo>,
    held: (Arc<RemoteLock>, OwnedMutexGuard<()>),
    (deadline, timeout): (tokio::time::Instant, Duration),
) -> Result<RecordedFetch, FetchFailure> {
    let (lock, guard) = held;
    let mut work = {
        let state = Arc::clone(state);
        let input = input.clone();
        let lock = Arc::clone(&lock);
        let plan = plan.clone();
        tokio::spawn(async move {
            let _guard = guard;
            let fetching = tokio::task::spawn_blocking(move || {
                fetch::fetch(&readonly, &plan, &crate::publish::env)
            });
            let result = match fetching.await {
                Ok(Ok(fetched)) => record(&state, &input, &fetched).await,
                Ok(Err(error)) => Err(FetchFailure::from(error)),
                Err(_) => Err(FetchFailure::Panicked),
            };
            if lock.stuck.swap(false, Ordering::AcqRel) {
                match &result {
                    Ok(_) => tracing::info!(
                        "a fetch that outlived its request finished and was recorded"
                    ),
                    Err(error) => {
                        tracing::warn!(error = %ErrorChain(error), "a fetch that outlived its request did not finish");
                    }
                }
            }
            result
        })
    };
    match tokio::time::timeout_at(deadline, &mut work).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(FetchFailure::Panicked),
        Err(_) => {
            lock.stuck.store(true, Ordering::Release);
            if work.is_finished() {
                lock.stuck.store(false, Ordering::Release);
            }
            tracing::warn!(git_remote = %plan.remote, "a fetch outlived [fetch] timeout; it is recorded if git succeeds");
            Err(FetchFailure::Timeout(timeout))
        }
    }
}

/// The plan job on the repository thread, under the create lock shared:
/// imports git refs, reads the git configuration as it is now, then plans.
async fn plan_job(
    state: &AppState,
    input: &FetchInput,
) -> Result<(FetchPlan, Arc<reef_lib::jj::ReadonlyRepo>), FetchFailure> {
    let create_lock = state.manager.repo_lock(input.repo);
    let _shared = create_lock.read().await;
    let workspace = input.workspace.as_ref().map(|record| record.id);
    let agent = input
        .agent
        .clone()
        .or_else(|| input.workspace.as_ref().map(|record| record.owner.clone()));
    let (remote, branches) = (input.remote.clone(), input.branches.clone());
    input
        .handle
        .call_async(move |head| {
            async move {
                crate::workspace::refresh_for_resolve(
                    head,
                    &crate::workspace::import_meta(workspace, agent.as_deref()),
                )
                .await
                .map_err(FetchFailure::from)?;
                let repo =
                    push::with_current_git_config(&head.repo.readonly(), head.repo.repo_path())
                        .await?;
                let plan = fetch::plan_fetch(&repo, &remote, &branches)?;
                Ok::<_, FetchFailure>((plan, repo))
            }
            .boxed_local()
        })
        .await?
}

/// The record job on the repository thread, under the create lock shared.
async fn record(
    state: &AppState,
    input: &FetchInput,
    fetched: &FetchedRefs,
) -> Result<RecordedFetch, FetchFailure> {
    let mut meta = OpMeta::new(OpKind::Fetch, fetch::fetch_description(fetched));
    meta.workspace = input.workspace.as_ref().map(|record| record.id.as_hex());
    meta.agent = input
        .agent
        .clone()
        .or_else(|| input.workspace.as_ref().map(|record| record.owner.clone()));
    let create_lock = state.manager.repo_lock(input.repo);
    let _shared = create_lock.read().await;
    let fetched = fetched.clone();
    let recorded = input
        .handle
        .call_async(move |head| {
            async move {
                head.refresh().await.map_err(ManagerError::from)?;
                Ok::<_, FetchFailure>(head.repo.record_fetch(&fetched, &meta).await?)
            }
            .boxed_local()
        })
        .await??;
    tracing::info!(
        git_remote = %recorded.remote,
        branches_count = recorded.branches.len(),
        op_id = recorded.op_id.as_ref().map(ToString::to_string).unwrap_or_default(),
        "fetched"
    );
    Ok(recorded)
}

/// `recorded` as the API reports it.
pub fn message(recorded: &RecordedFetch) -> v1::Fetched {
    let mut notes = Vec::new();
    let branches = recorded
        .branches
        .iter()
        .map(|branch| {
            if branch.missing {
                notes.push(format!(
                    "{} has no branch {}; nothing was changed for it",
                    recorded.remote, branch.branch
                ));
            }
            if branch.checked_out_left {
                notes.push(format!(
                    "the local bookmark {} is git's checked-out branch, so it was left as it is; {}@{} moved",
                    branch.branch, branch.branch, recorded.remote
                ));
            }
            v1::FetchedBranch {
                branch: branch.branch.clone(),
                commit_id: branch
                    .commit
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                previous_commit_id: branch
                    .previous
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                local_moved: branch.local_moved,
                missing: branch.missing,
            }
        })
        .collect();
    v1::Fetched {
        git_remote: recorded.remote.clone(),
        branches,
        notes,
    }
}
