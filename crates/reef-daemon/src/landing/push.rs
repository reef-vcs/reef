//! Pushing landed history to a git remote (R3.17, ADR 0077,
//! `docs/landing.md` "Pushing landed history").
//!
//! A destination whose policy says `[landing] push = "<remote>"` is pushed
//! to the branch of its own name on that remote after each landing, fast
//! forward only, through #258's one push path ([`crate::publish`]: the
//! branch lock, the timeout, the record, R1.16's marking). The push never
//! runs inside a landing: a landing wakes this module's worker for its
//! (repository, destination) and goes on, so an offline laptop keeps
//! landing.
//!
//! **The worker** pushes the destination's head, coalescing: whatever lands
//! while a push runs goes in the next one. A rejected lease means someone
//! else wrote the branch: the worker fetches it (following the destination
//! fast forward, never merging it) and plans again: up to date, pushed
//! again, followed, or `diverged`, which pauses pushing until a person acts
//! or the next landing finds it cleared. Failures that may pass retry with
//! backoff (30 s doubling to 15 minutes; 15 minutes for authentication and
//! a missing git); nothing polls. The state is one `destination_pushes` row
//! per destination, so a restart resumes it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use futures::FutureExt as _;
use reef_lib::error::{Classify as _, ErrorChain, ErrorKind};
use reef_lib::jj::push::{self, GitFailure, LandedPlan, LandedUpdate, PushError};
use reef_lib::jj::{CommitIdHex, OpKind, OpMeta, ReadonlyRepo};

use crate::daemon::AppState;
use crate::db::DestinationPushRow;
use crate::error::Advice;
use crate::model::{RepoId, Timestamp};
use crate::publish::{self, PublishError};

use super::LandingError;

/// The first retry after a failure that may pass.
const FIRST_RETRY: Duration = Duration::from_secs(30);
/// The longest wait between retries, and the wait after an authentication
/// failure or a missing git.
const MAX_RETRY: Duration = Duration::from_mins(15);
/// How recent a fetch or push of a destination makes a batch skip its
/// fetch, and how old a failed attempt must be for a landing to try again
/// before its retry is due.
const RECENT: Duration = Duration::from_secs(30);
/// The bound on a batch's fetch.
const BATCH_FETCH_WAIT: Duration = Duration::from_secs(30);
/// How long a destination retries before `reef daemon status` counts it.
pub(crate) const ATTENTION_AFTER: Duration = Duration::from_hours(1);

/// What woke a push.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// A landing onto the destination: tries at once unless a failed
    /// attempt is less than [`RECENT`] old.
    Landing,
    /// A retry's timer, or the daemon's start: tries when the retry is due.
    Due,
    /// `PushLanded` or `reef landing push`: tries at once.
    Request,
}

type Key = (RepoId, String);

/// One destination's worker, while it runs.
#[derive(Debug, Default)]
struct Running {
    /// Woken while it ran: look again before stopping.
    dirty: bool,
    /// The wake was a request (`PushLanded`): the next attempt ignores
    /// the retry schedule.
    requested: bool,
}

/// The push workers of the daemon (one in `AppState`).
#[derive(Debug)]
pub struct Pushes {
    /// Destinations whose worker runs.
    running: Mutex<HashMap<Key, Running>>,
    /// Destinations with a retry timer.
    timers: Mutex<HashSet<Key>>,
    /// When each destination was last fetched by this module.
    fetched: Mutex<HashMap<Key, tokio::time::Instant>>,
    /// When the last finished attempt of each destination started: an
    /// attempt that started after a landing finished has seen it.
    attempted_from: Mutex<HashMap<Key, Timestamp>>,
    /// Woken after every attempt.
    attempted: tokio::sync::Notify,
    /// The window after a fetch or push in which a batch does not fetch,
    /// in milliseconds; [`RECENT`] unless a test sets it.
    window_ms: std::sync::atomic::AtomicU64,
}

impl Default for Pushes {
    fn default() -> Self {
        Pushes {
            running: Mutex::default(),
            timers: Mutex::default(),
            fetched: Mutex::default(),
            attempted_from: Mutex::default(),
            attempted: tokio::sync::Notify::new(),
            window_ms: std::sync::atomic::AtomicU64::new(u64::MAX),
        }
    }
}

impl Pushes {
    /// Marks the destination's worker running; `false` when it already was
    /// (it then looks again, as a request when `requested`).
    fn claim(&self, key: &Key, requested: bool) -> bool {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = running.get_mut(key) {
            entry.dirty = true;
            entry.requested |= requested;
            return false;
        }
        running.insert(key.clone(), Running::default());
        true
    }

    /// Whether the worker stops: no wake since it last looked. The next
    /// attempt's trigger otherwise.
    fn release(&self, key: &Key) -> Option<Trigger> {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        match running.get_mut(key) {
            Some(entry) if entry.dirty => {
                entry.dirty = false;
                let requested = std::mem::take(&mut entry.requested);
                Some(if requested {
                    Trigger::Request
                } else {
                    Trigger::Landing
                })
            }
            _ => {
                running.remove(key);
                None
            }
        }
    }

    fn forget(&self, key: &Key) {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
    }

    fn window(&self) -> Duration {
        match self.window_ms.load(std::sync::atomic::Ordering::Relaxed) {
            u64::MAX => RECENT,
            ms => Duration::from_millis(ms),
        }
    }

    /// Sets the window after a fetch or push in which a batch does not
    /// fetch again (tests).
    #[doc(hidden)]
    pub fn set_fetch_window(&self, window: Duration) {
        self.window_ms.store(
            u64::try_from(window.as_millis()).unwrap_or(u64::MAX - 1),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    fn fetched_recently(&self, key: &Key) -> bool {
        let window = self.window();
        self.fetched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .is_some_and(|at| at.elapsed() < window)
    }

    fn is_running(&self, key: &Key) -> bool {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(key)
    }

    fn note_fetch(&self, key: &Key) {
        self.fetched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.clone(), tokio::time::Instant::now());
    }

    fn note_attempt(&self, key: &Key, started: Timestamp) {
        self.attempted_from
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.clone(), started);
        self.attempted.notify_waiters();
    }

    /// When the last finished attempt of the destination started.
    fn attempted_from(&self, key: &Key) -> Option<Timestamp> {
        self.attempted_from
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .copied()
    }

    /// Resolves after the next attempt of any destination.
    pub(crate) fn attempted(&self) -> tokio::sync::futures::Notified<'_> {
        self.attempted.notified()
    }
}

/// Removes a worker's key when its task ends, however it ends.
struct RunningGuard {
    state: Weak<AppState>,
    key: Key,
    done: bool,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        if !self.done
            && let Some(state) = self.state.upgrade()
        {
            state.pushes.forget(&self.key);
        }
    }
}

/// A landing's claim on its destination's push worker, taken before the
/// landing's rows are written, so a `reef land` waiting for the push never
/// sees a destination with no worker yet. [`Claim::start`] after the rows.
#[must_use]
pub(crate) struct Claim {
    key: Option<Key>,
}

impl Claim {
    /// Starts the worker this claim took; wakes the running one otherwise.
    pub(crate) fn start(self, state: &Arc<AppState>) {
        if let Some(key) = self.key {
            spawn(state, key, Trigger::Landing);
        }
    }
}

/// Claims `destination`'s push worker for a landing about to be recorded;
/// a running worker looks again when it ends.
pub(crate) fn claim(state: &Arc<AppState>, repo: RepoId, destination: &str) -> Claim {
    let key = (repo, destination.to_owned());
    Claim {
        key: state.pushes.claim(&key, false).then_some(key),
    }
}

/// Wakes the push worker of `destination` in `repo`; a running one looks
/// again when it ends.
pub(crate) fn wake(state: &Arc<AppState>, repo: RepoId, destination: &str, trigger: Trigger) {
    let key = (repo, destination.to_owned());
    if state.pushes.claim(&key, trigger == Trigger::Request) {
        spawn(state, key, trigger);
    }
}

fn spawn(state: &Arc<AppState>, key: Key, trigger: Trigger) {
    let weak = Arc::downgrade(state);
    tokio::spawn(run(weak, key, trigger));
}

async fn run(weak: Weak<AppState>, key: Key, trigger: Trigger) {
    let mut guard = RunningGuard {
        state: weak.clone(),
        key: key.clone(),
        done: false,
    };
    let mut trigger = trigger;
    loop {
        let Some(state) = weak.upgrade() else {
            return;
        };
        if state.shutdown.is_cancelled() {
            return;
        }
        let started = Timestamp::now();
        if let Err(error) = attempt(&state, key.0, &key.1, trigger).await {
            tracing::warn!(repo = %key.0, destination = %key.1, error = %ErrorChain(&error), "cannot push landed history");
            if let Err(error) = record_error(&state, key.0, &key.1, &error) {
                tracing::warn!(error = %ErrorChain(&error), "cannot record a landing push's failure");
            }
        }
        state.pushes.note_attempt(&key, started);
        match state.pushes.release(&key) {
            None => {
                guard.done = true;
                return;
            }
            Some(next) => trigger = next,
        }
    }
}

/// `PushLanded` with `wait`: an attempt through the destination's worker
/// (one at a time per destination, and finished even if the caller goes
/// away), then its row; waits at most `deadline`.
pub(crate) async fn push_now(
    state: &Arc<AppState>,
    repo: RepoId,
    destination: &str,
    deadline: tokio::time::Instant,
) -> Result<Option<DestinationPushRow>, LandingError> {
    let key = (repo, destination.to_owned());
    let asked = Timestamp::now();
    wake(state, repo, destination, Trigger::Request);
    loop {
        let attempted = state.pushes.attempted();
        let seen = state
            .pushes
            .attempted_from(&key)
            .is_some_and(|from| from.0 >= asked.0);
        if seen || tokio::time::Instant::now() >= deadline {
            return current(state, repo, destination).await;
        }
        let _ = tokio::time::timeout_at(deadline, attempted).await;
    }
}

/// An attempt that failed outside the push itself (the database, the
/// repository thread): `retrying` with backoff, so it is tried again.
fn record_error(
    state: &Arc<AppState>,
    repo: RepoId,
    destination: &str,
    error: &LandingError,
) -> Result<(), LandingError> {
    let Some(mut row) = state.db.destination_push(repo, destination)? else {
        return Ok(());
    };
    let message = ErrorChain(error).to_string();
    retry_later(
        state,
        &mut row,
        reef_api::error::GIT_PUSH_FAILED,
        &message,
        Advice {
            hint: "The daemon's log has more; it tries again, and `reef landing push` tries now."
                .to_owned(),
            next: Vec::new(),
        },
        None,
    )
}

/// At start: wakes every destination with a push row, and every one with a
/// landing no push carried yet whose policy may push, so a crash between a
/// landing and its push, or between a push and its record, is pushed (or
/// recorded) now. A destination that does not push costs one policy read.
pub(crate) fn sweep(state: &Arc<AppState>) {
    let mut keys: HashSet<Key> = HashSet::new();
    match state.db.destination_pushes(None) {
        Ok(rows) => keys.extend(rows.into_iter().map(|row| (row.repo_id, row.destination))),
        Err(error) => {
            tracing::warn!(error = %ErrorChain(&error), "cannot read the landing pushes");
        }
    }
    match state.db.destinations_with_unpushed() {
        Ok(found) => keys.extend(found),
        Err(error) => {
            tracing::warn!(error = %ErrorChain(&error), "cannot read the unpushed landings");
        }
    }
    for (repo, destination) in keys {
        wake(state, repo, &destination, Trigger::Due);
    }
}

/// Fetches a pushing destination's branch before a batch forms (decision
/// 6), so a merge on the host since the last landing is followed instead
/// of diverging. Skipped while pushing or fetching is off, while the remote
/// is known to be failing, and within [`RECENT`] of the last fetch or push.
/// A failure is only logged. Returns whether the destination moved.
pub(crate) async fn fetch_before_batch(
    state: &Arc<AppState>,
    repo: RepoId,
    destination: &str,
    remote: &str,
) -> bool {
    let publish = &state.config.file.publish;
    if !state.config.file.fetch.enabled || !publish.enabled || !publish.landings {
        return false;
    }
    let key = (repo, destination.to_owned());
    if state.pushes.fetched_recently(&key) {
        return false;
    }
    let now = Timestamp::now();
    if let Ok(Some(row)) = state.db.destination_push(repo, destination) {
        let failing = row.state == State::Retrying.as_str()
            && row.next_attempt_at.is_some_and(|at| at.0 > now.0);
        let pushed_recently = row
            .pushed_at
            .is_some_and(|at| now.0.saturating_sub(at.0) < millis(state.pushes.window()));
        if failing || pushed_recently {
            return false;
        }
    }
    fetch_destination(state, repo, destination, remote, Some(BATCH_FETCH_WAIT))
        .await
        .unwrap_or(false)
}

/// Fetches `destination`'s branch from `remote`, following it: whether
/// the destination moved, or why the fetch failed.
async fn fetch_destination(
    state: &Arc<AppState>,
    repo: RepoId,
    destination: &str,
    remote: &str,
    wait: Option<Duration>,
) -> Result<bool, crate::fetch::FetchFailure> {
    let handle = state
        .manager
        .handle(repo)
        .map_err(crate::fetch::FetchFailure::from)?;
    state.pushes.note_fetch(&(repo, destination.to_owned()));
    let input = crate::fetch::FetchInput {
        repo,
        handle,
        workspace: None,
        agent: Some(super::BATCH_AGENT.to_owned()),
        remote: remote.to_owned(),
        branches: vec![destination.to_owned()],
        follow: vec![destination.to_owned()],
        timeout: wait,
    };
    match crate::fetch::fetch(state, input).await {
        Ok(recorded) => Ok(recorded.branches.iter().any(|branch| branch.local_moved)),
        Err(error) => {
            tracing::info!(repo = %repo, destination, error = %ErrorChain(&error), "could not fetch a pushing destination");
            Err(error)
        }
    }
}

/// A destination's push state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// The remote branch is at the destination's head.
    Pushed,
    /// A push is due or running.
    Pending,
    /// The last attempt failed for a reason that may pass.
    Retrying,
    /// The remote and the destination each have commits the other lacks.
    Diverged,
    /// A person must act.
    Refused,
    /// The daemon's `[publish]` configuration turns it off.
    Disabled,
}

impl State {
    /// Its text in the row.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            State::Pushed => "pushed",
            State::Pending => "pending",
            State::Retrying => "retrying",
            State::Diverged => "diverged",
            State::Refused => "refused",
            State::Disabled => "disabled",
        }
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// The wait before retry number `attempts` (1 for the first failure).
fn backoff(attempts: u32) -> Duration {
    let doublings = attempts.saturating_sub(1).min(10);
    FIRST_RETRY.saturating_mul(1 << doublings).min(MAX_RETRY)
}

/// Whether a retrying destination tries now: a request always; a landing
/// when the retry is due or the last attempt is [`RECENT`] old (but not
/// for failed authentication or a missing git, whose fix is outside
/// Reef); a timer or the start when it is due.
fn due(row: &DestinationPushRow, trigger: Trigger, now: Timestamp) -> bool {
    let Some(next) = row
        .next_attempt_at
        .filter(|_| row.state == State::Retrying.as_str())
    else {
        return true;
    };
    let due = next.0 <= now.0;
    let outside = matches!(
        row.reason.as_deref(),
        Some(reef_api::error::GIT_AUTH_FAILED | reef_api::error::GIT_NOT_FOUND)
    );
    let old_attempt = now.0.saturating_sub(row.updated_at.0) >= millis(RECENT);
    match trigger {
        Trigger::Request => true,
        Trigger::Landing => due || (old_attempt && !outside),
        Trigger::Due => due,
    }
}

/// One attempt at pushing `destination`: reads its policy, and pushes,
/// records, follows or reports. `None` when the destination does not push
/// (its row is then gone).
pub(crate) async fn attempt(
    state: &Arc<AppState>,
    repo: RepoId,
    destination: &str,
    trigger: Trigger,
) -> Result<Option<DestinationPushRow>, LandingError> {
    let now = Timestamp::now();
    let existing = state.db.destination_push(repo, destination)?;
    // A retry that is not due costs nothing.
    if let Some(row) = &existing
        && !due(row, trigger, now)
    {
        if let Some(next) = row.next_attempt_at {
            schedule(state, repo, destination, next);
        }
        return Ok(existing);
    }
    // A request may come before any landing, when the destination is a git
    // branch jj has not imported yet; a landing moved the bookmark itself.
    if trigger == Trigger::Request {
        import_refs(state, repo).await?;
    }
    let settings = super::batch::settings(state, repo, destination).await?;
    if settings.policy_invalid() {
        // The policy does not load: nothing lands either. Keep the state.
        return Ok(existing);
    }
    let Some(remote) = settings.push.clone() else {
        state.db.delete_destination_push(repo, destination)?;
        return Ok(None);
    };
    let mut row = existing
        .filter(|row| row.git_remote == remote)
        .unwrap_or_else(|| DestinationPushRow::new(repo, destination, &remote, now));
    let publish = &state.config.file.publish;
    if !publish.enabled || !publish.landings {
        let key = if publish.enabled {
            "landings"
        } else {
            "enabled"
        };
        let advice = Advice {
            hint: format!(
                "The daemon's [publish] {key} is false in {}, so landings onto {destination} are not pushed to {remote}; set it to true and restart the daemon.",
                state.config.paths.config_file.display()
            ),
            next: Vec::new(),
        };
        settle(
            state,
            &mut row,
            State::Disabled,
            Some((
                reef_api::error::PUBLISH_DISABLED,
                "pushing landed history is off",
                advice,
            )),
        )?;
        return Ok(Some(row));
    }
    if state.config.identity_is_default() {
        let error = PublishError::IdentityUnset {
            email: state.config.identity.email.clone(),
        };
        failed(state, &mut row, &error)?;
        return Ok(Some(row));
    }
    State::Pending.as_str().clone_into(&mut row.state);
    row.updated_at = now;
    state.db.put_destination_push(&row)?;
    let policy_head = settings.commit.clone();
    push_and_follow(state, repo, destination, &remote, policy_head, &mut row).await?;
    Ok(Some(row))
}

/// Imports git's refs on the repository thread.
async fn import_refs(state: &AppState, repo: RepoId) -> Result<(), LandingError> {
    let handle = state.manager.handle(repo)?;
    handle
        .call_async(|head| {
            async move {
                crate::workspace::refresh_for_resolve(
                    head,
                    &crate::workspace::import_meta(None, Some(super::BATCH_AGENT)),
                )
                .await
            }
            .boxed_local()
        })
        .await??;
    Ok(())
}

/// Plans and pushes, fetching once when the remote moved or is ahead.
async fn push_and_follow(
    state: &Arc<AppState>,
    repo: RepoId,
    destination: &str,
    remote: &str,
    policy_head: Option<String>,
    row: &mut DestinationPushRow,
) -> Result<(), LandingError> {
    let timeout = state.config.file.publish.timeout;
    let mut fetched = false;
    // A plan, at most one fetch, a plan again; and a plan under the lock.
    for _ in 0..4 {
        let planned = match plan(state, repo, destination, remote).await {
            Ok(planned) => planned,
            Err(error) => return failed(state, row, &error),
        };
        // The policy read was at another head (a landing ran meanwhile, and
        // may have changed `[landing] push`): read it again.
        if policy_head.as_deref() != Some(planned.plan.head.as_str()) {
            State::Pending.as_str().clone_into(&mut row.state);
            wake(state, repo, destination, Trigger::Landing);
            return Ok(());
        }
        match planned.plan.update {
            LandedUpdate::UpToDate => return pushed(state, row, &planned.plan, None).await,
            LandedUpdate::Created | LandedUpdate::FastForward => {}
            LandedUpdate::RemoteAhead if !fetched => {
                fetched = true;
                if let Err(error) = fetch_destination(state, repo, destination, remote, None).await
                {
                    return fetch_failed(state, row, &error);
                }
                continue;
            }
            LandedUpdate::RemoteAhead => return remote_ahead(state, row, &planned),
            LandedUpdate::Diverged if !fetched => {
                // The remote may have been reset since Reef last saw it.
                fetched = true;
                if let Err(error) = fetch_destination(state, repo, destination, remote, None).await
                {
                    return fetch_failed(state, row, &error);
                }
                continue;
            }
            LandedUpdate::Diverged => return diverged(state, row, &planned.plan),
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let lock = publish::lock_for((repo, remote.to_owned(), destination.to_owned()));
        let guard = match publish::acquire(
            &lock,
            &(remote.to_owned(), destination.to_owned()),
            deadline,
        )
        .await
        {
            Ok(guard) => guard,
            Err(error) => return failed(state, row, &error),
        };
        // Planned again under the lock: another push of the branch may have
        // run meanwhile.
        let planned = match plan(state, repo, destination, remote).await {
            Ok(planned) => planned,
            Err(error) => return failed(state, row, &error),
        };
        let Some(push_plan) = planned.plan.plan.clone() else {
            drop(guard);
            continue;
        };
        let mut meta = OpMeta::new(OpKind::Publish, push::push_landed_description(&push_plan));
        meta.agent = Some(super::BATCH_AGENT.to_owned());
        let target = publish::PushTarget {
            repo,
            handle: state.manager.handle(repo)?,
            meta,
            timeout,
        };
        match publish::push_and_record(
            state,
            target,
            push_plan,
            planned.readonly,
            (lock, guard),
            deadline,
        )
        .await
        {
            Ok((_, recorded, _)) => {
                let op = recorded.op_id.map(|id| id.to_string());
                return pushed(state, row, &planned.plan, op).await;
            }
            Err(PublishError::Push(PushError::Moved { .. })) if !fetched => {
                fetched = true;
                if let Err(error) = fetch_destination(state, repo, destination, remote, None).await
                {
                    return fetch_failed(state, row, &error);
                }
            }
            Err(error) => return failed(state, row, &error),
        }
    }
    let error = PublishError::InProgress {
        remote: remote.to_owned(),
        branch: destination.to_owned(),
    };
    failed(state, row, &error)
}

/// A landed plan and the repository it was planned in.
struct Planned {
    plan: LandedPlan,
    readonly: Arc<ReadonlyRepo>,
}

/// The plan job on the repository thread, under the create lock shared:
/// reads the repository and the git configuration as they are now, then
/// plans. It does not import git's refs: a `git fetch` at the root that
/// moved a tracked remote bookmark would otherwise merge into the
/// destination here.
async fn plan(
    state: &AppState,
    repo: RepoId,
    destination: &str,
    remote: &str,
) -> Result<Planned, PublishError> {
    let create_lock = state.manager.repo_lock(repo);
    let _shared = create_lock.read().await;
    let handle = state.manager.handle(repo)?;
    let destination = destination.to_owned();
    let remote = remote.to_owned();
    let allowed = state.config.file.publish.remotes.clone();
    handle
        .call_async(move |head| {
            async move {
                head.refresh().await.map_err(|error| {
                    PublishError::from(crate::workspace::ManagerError::from(error))
                })?;
                let readonly =
                    push::with_current_git_config(&head.repo.readonly(), head.repo.repo_path())
                        .await?;
                let plan = push::plan_landed_push(
                    &readonly,
                    &destination,
                    &remote,
                    &allowed,
                    push::MAX_RANGE,
                )
                .await?;
                Ok::<_, PublishError>(Planned { plan, readonly })
            }
            .boxed_local()
        })
        .await?
}

/// The landings onto the destination no push carried yet, split into those
/// whose head is in `::head` and the rest.
async fn carried(
    state: &AppState,
    repo: RepoId,
    destination: &str,
    head: &CommitIdHex,
) -> Result<(Vec<String>, usize), LandingError> {
    let unpushed = state.db.unpushed_landings(repo, destination)?;
    if unpushed.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let handle = state.manager.handle(repo)?;
    let head = head.clone();
    let total = unpushed.len();
    let found = handle
        .call_async(move |repo_head| {
            async move {
                let commits: Vec<CommitIdHex> = unpushed
                    .iter()
                    .filter_map(|(_, commit)| super::commit_hex(commit))
                    .collect();
                let reached =
                    push::reachable_from(&repo_head.repo.readonly(), &head, &commits).await;
                let reached = match reached {
                    Ok(reached) => reached,
                    Err(error) => {
                        tracing::warn!(error = %ErrorChain(&error), "cannot tell which landings a push carried");
                        return Vec::new();
                    }
                };
                unpushed
                    .into_iter()
                    .filter(|(_, commit)| {
                        super::commit_hex(commit).is_some_and(|commit| reached.contains(&commit))
                    })
                    .map(|(id, _)| id)
                    .collect::<Vec<String>>()
            }
            .boxed_local()
        })
        .await?;
    let rest = total - found.len();
    Ok((found, rest))
}

/// Records a push (or the remote already at the head, with no operation).
async fn pushed(
    state: &Arc<AppState>,
    row: &mut DestinationPushRow,
    plan: &LandedPlan,
    operation: Option<String>,
) -> Result<(), LandingError> {
    let now = Timestamp::now();
    let (ids, rest) = carried(state, row.repo_id, &row.destination, &plan.head).await?;
    state
        .db
        .mark_landings_pushed(row.repo_id, &ids, now, operation.as_deref())?;
    row.local_commit = Some(plan.head.to_string());
    row.remote_commit = Some(plan.head.to_string());
    row.unpushed_count = u32::try_from(rest).unwrap_or(u32::MAX);
    if let Some(operation) = operation {
        row.pushed_at = Some(now);
        row.operation_id = Some(operation);
        row.operation_collected = false;
        tracing::info!(repo = %row.repo_id, destination = %row.destination, git_remote = %row.git_remote, landings_count = ids.len(), "landed history pushed");
    } else if row.pushed_at.is_none() {
        row.pushed_at = Some(now);
    }
    settle(state, row, State::Pushed, None)
}

/// The remote is ahead and the fetch did not follow it.
fn remote_ahead(
    state: &AppState,
    row: &mut DestinationPushRow,
    planned: &Planned,
) -> Result<(), LandingError> {
    let plan = &planned.plan;
    row.local_commit = Some(plan.head.to_string());
    row.remote_commit = plan.remote_commit.as_ref().map(ToString::to_string);
    row.remote_head = plan.remote_head.as_ref().map(foreign_json);
    let (reason, message, hint) = if plan.policy_changed {
        (
            reef_api::error::REMOTE_POLICY_CHANGED,
            format!(
                "{}/{} is ahead of {} with a change to reef-policy.toml or reef-checks.toml, so it is not followed",
                row.git_remote, row.branch, row.destination
            ),
            format!(
                "A policy change on the host would govern later landings without the owners' sign-off here. Review it, then move {d} to it by hand (`jj bookmark set {d} -r {d}@{r}` at the repository's root) and `reef landing push`.",
                d = row.destination,
                r = row.git_remote
            ),
        )
    } else {
        (
            reef_api::error::BRANCH_MOVED,
            format!(
                "{}/{} is ahead of {} and the fetch did not follow it",
                row.git_remote, row.branch, row.destination
            ),
            "Fetch it with `reef fetch`, then push again with `reef landing push`.".to_owned(),
        )
    };
    let advice = Advice {
        hint,
        next: vec!["reef".into(), "landing".into(), "pushes".into()],
    };
    settle(state, row, State::Refused, Some((reason, &message, advice)))
}

/// The remote and the destination diverged: pushing pauses.
fn diverged(
    state: &AppState,
    row: &mut DestinationPushRow,
    plan: &LandedPlan,
) -> Result<(), LandingError> {
    row.local_commit = Some(plan.head.to_string());
    row.remote_commit = plan.remote_commit.as_ref().map(ToString::to_string);
    row.remote_head = plan.remote_head.as_ref().map(foreign_json);
    row.unpushed_count = u32::try_from(
        state
            .db
            .unpushed_landings(row.repo_id, &row.destination)?
            .len(),
    )
    .unwrap_or(u32::MAX);
    let message = format!(
        "{}/{} and {} have diverged: each has commits the other lacks",
        row.git_remote, row.branch, row.destination
    );
    let advice = Advice {
        hint: format!(
            "Landings onto {d} continue here but are not pushed. Either make the remote include them (at the repository's root: `jj rebase -b {d} -d {d}@{r} --ignore-immutable`, then `reef landing push`), or make {d} the remote's (`jj bookmark set {d} -r {d}@{r}`, then land the unpushed changes again).",
            d = row.destination,
            r = row.git_remote
        ),
        next: vec!["reef".into(), "landing".into(), "pushes".into()],
    };
    settle(
        state,
        row,
        State::Diverged,
        Some((reef_api::error::BRANCH_DIVERGED, &message, advice)),
    )
}

fn foreign_json(foreign: &push::ForeignCommit) -> String {
    serde_json::json!({
        "commit_id": foreign.commit.to_string(),
        "author": foreign.author,
        "summary": foreign.summary,
    })
    .to_string()
}

/// Records a failed attempt: `retrying` with its backoff, or `refused`.
fn failed(
    state: &Arc<AppState>,
    row: &mut DestinationPushRow,
    error: &PublishError,
) -> Result<(), LandingError> {
    let root = state
        .db
        .repository(row.repo_id)
        .map(|record| record.root)
        .unwrap_or_default();
    let context = publish::Context::new(state, root, &row.destination);
    let message = ErrorChain(error).to_string();
    let (reason, advice) = match error {
        PublishError::Push(PushError::DestinationNotFound(_)) => (
            reef_api::error::DESTINATION_NOT_FOUND,
            Advice {
                hint: format!("There is no bookmark {} to push.", row.destination),
                next: Vec::new(),
            },
        ),
        // Someone else keeps writing the branch: Reef fetched and planned
        // again, and the remote moved once more (publish's hint about
        // `--force` does not apply here).
        PublishError::Push(PushError::Moved { .. }) => (
            reef_api::error::BRANCH_MOVED,
            Advice {
                hint: format!(
                    "{}/{} moved again while Reef pushed; it tries again (`reef landing push` tries now).",
                    row.git_remote, row.branch
                ),
                next: Vec::new(),
            },
        ),
        PublishError::Push(PushError::DestinationConflicted(_)) => (
            reef_api::error::DESTINATION_CONFLICTED,
            Advice {
                hint: format!(
                    "Bookmark {d} is conflicted; resolve it at the repository's root (`jj bookmark set {d} -r <commit>`), then `reef landing push`.",
                    d = row.destination
                ),
                next: Vec::new(),
            },
        ),
        _ => publish::reason_and_advice(error, &context).unwrap_or_else(|| {
            (
                reef_api::error::GIT_PUSH_FAILED,
                Advice {
                    hint: "The daemon's log has more; `reef landing push` tries again.".to_owned(),
                    next: Vec::new(),
                },
            )
        }),
    };
    if let PublishError::Push(PushError::Git { stderr, .. }) = error {
        row.git_stderr = Some(stderr.clone());
    } else {
        row.git_stderr = None;
    }
    let wait = match error {
        PublishError::Push(
            PushError::Git {
                failure: GitFailure::Auth,
                ..
            }
            | PushError::GitNotFound(_),
        ) => Some(MAX_RETRY),
        PublishError::Push(PushError::Git { .. } | PushError::Moved { .. })
        | PublishError::Timeout(_)
        | PublishError::InProgress { .. } => Some(backoff(row.attempts + 1)),
        other if matches!(other.kind(), ErrorKind::Unavailable | ErrorKind::Internal) => {
            Some(backoff(row.attempts + 1))
        }
        _ => None,
    };
    let now = Timestamp::now();
    row.attempts = row.attempts.saturating_add(1);
    row.failing_since.get_or_insert(now);
    row.unpushed_count = u32::try_from(
        state
            .db
            .unpushed_landings(row.repo_id, &row.destination)?
            .len(),
    )
    .unwrap_or(u32::MAX);
    let failure = Some((reason, message.as_str(), advice));
    if let Some(wait) = wait {
        let at = Timestamp(now.0.saturating_add(millis(wait)));
        row.next_attempt_at = Some(at);
        settle_keeping_failure(state, row, State::Retrying, failure)?;
        schedule(state, row.repo_id, &row.destination, at);
    } else {
        row.next_attempt_at = None;
        settle_keeping_failure(state, row, State::Refused, failure)?;
    }
    tracing::info!(repo = %row.repo_id, destination = %row.destination, reason, state = %row.state, error = %message, "landed history not pushed");
    Ok(())
}

/// A fetch the push needed failed: `retrying` with backoff, never a push
/// with the old lease; `refused` when fetching is off.
fn fetch_failed(
    state: &Arc<AppState>,
    row: &mut DestinationPushRow,
    error: &crate::fetch::FetchFailure,
) -> Result<(), LandingError> {
    let message = ErrorChain(error).to_string();
    if let crate::fetch::FetchFailure::Disabled { config } = error {
        let advice = Advice {
            hint: format!(
                "{}/{} moved, and following it needs a fetch, which [fetch] enabled = false in {} turns off; turn it on, or move {} by hand.",
                row.git_remote,
                row.branch,
                config.display(),
                row.destination
            ),
            next: Vec::new(),
        };
        row.next_attempt_at = None;
        row.attempts = row.attempts.saturating_add(1);
        row.failing_since.get_or_insert(Timestamp::now());
        return settle_keeping_failure(
            state,
            row,
            State::Refused,
            Some((reef_api::error::FETCH_DISABLED, &message, advice)),
        );
    }
    retry_later(
        state,
        row,
        reef_api::error::GIT_FETCH_FAILED,
        &message,
        Advice {
            hint: format!(
                "{}/{} moved and Reef could not fetch it; it tries again (`reef landing push` tries now).",
                row.git_remote, row.branch
            ),
            next: Vec::new(),
        },
        None,
    )
}

/// `retrying`, with the next backoff, for `reason`.
fn retry_later(
    state: &Arc<AppState>,
    row: &mut DestinationPushRow,
    reason: &str,
    message: &str,
    advice: Advice,
    stderr: Option<String>,
) -> Result<(), LandingError> {
    let now = Timestamp::now();
    row.attempts = row.attempts.saturating_add(1);
    row.failing_since.get_or_insert(now);
    row.git_stderr = stderr;
    let at = Timestamp(now.0.saturating_add(millis(backoff(row.attempts))));
    row.next_attempt_at = Some(at);
    settle_keeping_failure(state, row, State::Retrying, Some((reason, message, advice)))?;
    schedule(state, row.repo_id, &row.destination, at);
    Ok(())
}

type Failure<'a> = (&'a str, &'a str, Advice);

/// Writes `row` in `to`: a success clears the failure and the retry
/// schedule; a failure records its reason, message and advice.
fn settle(
    state: &AppState,
    row: &mut DestinationPushRow,
    to: State,
    failure: Option<Failure<'_>>,
) -> Result<(), LandingError> {
    if failure.is_none() {
        row.attempts = 0;
        row.next_attempt_at = None;
        row.failing_since = None;
        row.git_stderr = None;
    }
    if to != State::Diverged && to != State::Refused {
        row.remote_head = None;
    }
    settle_keeping_failure(state, row, to, failure)
}

fn settle_keeping_failure(
    state: &AppState,
    row: &mut DestinationPushRow,
    to: State,
    failure: Option<Failure<'_>>,
) -> Result<(), LandingError> {
    to.as_str().clone_into(&mut row.state);
    if let Some((reason, message, advice)) = failure {
        row.reason = Some(reason.to_owned());
        row.message = Some(message.to_owned());
        row.hint = Some(advice.hint).filter(|hint| !hint.is_empty());
        row.next = (!advice.next.is_empty())
            .then(|| serde_json::to_string(&advice.next).unwrap_or_default());
    } else {
        row.reason = None;
        row.message = None;
        row.hint = None;
        row.next = None;
    }
    row.updated_at = Timestamp::now();
    state.db.put_destination_push(row)?;
    Ok(())
}

/// Wakes the destination when its retry is due, once per destination.
fn schedule(state: &Arc<AppState>, repo: RepoId, destination: &str, at: Timestamp) {
    let key = (repo, destination.to_owned());
    {
        let mut timers = state
            .pushes
            .timers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !timers.insert(key.clone()) {
            return;
        }
    }
    let weak = Arc::downgrade(state);
    let shutdown = state.shutdown.clone();
    let wait = Duration::from_millis(u64::try_from(at.0 - Timestamp::now().0).unwrap_or(0));
    tokio::spawn(async move {
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = shutdown.cancelled() => return,
        }
        let Some(state) = weak.upgrade() else {
            return;
        };
        state
            .pushes
            .timers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key);
        wake(&state, key.0, &key.1, Trigger::Due);
    });
}

/// The destination's push row, or `None` when its policy does not push:
/// what `PushLanded` without `wait` answers.
pub(crate) async fn current(
    state: &Arc<AppState>,
    repo: RepoId,
    destination: &str,
) -> Result<Option<DestinationPushRow>, LandingError> {
    if let Some(row) = state.db.destination_push(repo, destination)? {
        return Ok(Some(row));
    }
    let settings = super::batch::settings(state, repo, destination).await?;
    Ok(settings
        .push
        .map(|remote| DestinationPushRow::new(repo, destination, &remote, Timestamp::now())))
}

/// `DESTINATION_NOT_FOUND` for `PushLanded`: no bookmark `destination`.
pub(crate) fn not_found(destination: &str) -> tonic::Status {
    let detail = reef_api::v1::ErrorDetail {
        kind: crate::error::proto_kind(ErrorKind::NotFound).into(),
        reason: reef_api::error::DESTINATION_NOT_FOUND.to_owned(),
        chain: Vec::new(),
        hint: "Name a landing destination (`reef landing pushes` lists the ones that push), or omit it for trunk()'s bookmark.".to_owned(),
        next: vec!["reef".into(), "landing".into(), "pushes".into()],
        metadata: [("destination".to_owned(), destination.to_owned())]
            .into_iter()
            .collect(),
    };
    reef_api::error::status(
        crate::error::code_of(ErrorKind::NotFound),
        format!("no bookmark {destination:?} to push"),
        &detail,
    )
}

/// `DESTINATION_NOT_PUSHED`: `destination`'s policy has no `[landing]
/// push`.
pub(crate) fn not_pushed(destination: &str) -> tonic::Status {
    let detail = reef_api::v1::ErrorDetail {
        kind: crate::error::proto_kind(ErrorKind::Conflict).into(),
        reason: reef_api::error::DESTINATION_NOT_PUSHED.to_owned(),
        chain: Vec::new(),
        hint: format!(
            "{destination}'s policy has no [landing] push: add `push = \"<remote>\"` to [landing] in reef-policy.toml (a policy change needs the owners' sign-off), then land onto it."
        ),
        next: Vec::new(),
        metadata: [("destination".to_owned(), destination.to_owned())]
            .into_iter()
            .collect(),
    };
    reef_api::error::status(
        crate::error::code_of(ErrorKind::Conflict),
        format!("{destination} is not pushed to a git remote"),
        &detail,
    )
}

/// Waits until `landing` is pushed, or an attempt of its destination's
/// worker that started after the landing finished has ended, at most until
/// `deadline`: what `reef land` reports. `None` when the destination does
/// not push.
pub(crate) async fn wait_for_push(
    state: &AppState,
    repo: RepoId,
    landing_id: &str,
    deadline: tokio::time::Instant,
) -> Result<Option<(crate::db::LandingRow, DestinationPushRow)>, LandingError> {
    loop {
        let attempted = state.pushes.attempted();
        let Some(landing) = state.db.landing(repo, landing_id)? else {
            return Ok(None);
        };
        let key = (repo, landing.row.destination.clone());
        let finished = landing.row.finished_at.unwrap_or(landing.row.created_at);
        let seen = state
            .pushes
            .attempted_from(&key)
            .is_some_and(|from| from.0 >= finished.0);
        let running = state.pushes.is_running(&key);
        let row = state.db.destination_push(repo, &landing.row.destination)?;
        let done = landing.row.pushed_at.is_some()
            || seen
            || !running
            || tokio::time::Instant::now() >= deadline;
        if done {
            return Ok(row.map(|row| (landing.row, row)));
        }
        let _ = tokio::time::timeout_at(deadline, attempted).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_fifteen_minutes() {
        assert_eq!(backoff(1), Duration::from_secs(30));
        assert_eq!(backoff(2), Duration::from_secs(60));
        assert_eq!(backoff(5), Duration::from_secs(480));
        assert_eq!(backoff(6), MAX_RETRY);
        assert_eq!(backoff(100), MAX_RETRY);
    }
}
