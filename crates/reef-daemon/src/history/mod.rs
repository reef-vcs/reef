//! The history index (R2.16, ADR 0044): per repository, one row per change
//! the operation log has shown, with its outcome, its activity by agent and
//! workspace, and the paths and symbols its net diff touches, in a SQLite
//! cache (`store.rs`) that `QueryChanges` reads (`query.rs`).
//!
//! - **Following** (`follow.rs`): one task reads every open repository's
//!   log after its frontier when [`crate::subscribe::OpEvents`] says it
//!   changed, and backfills older operations when an index is new.
//! - **Touches** (`touch.rs`): one worker computes each change's paths and
//!   symbols once it settles, throttled.
//! - **GC** (`gc.rs`): a sixth step per repository; and GC's rewrite of
//!   operation ids is mapped with the follower paused (`History::pause`,
//!   `History::rewrite`).

pub(crate) mod follow;
pub(crate) mod gc;
pub(crate) mod query;
pub mod store;
pub(crate) mod touch;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use reef_lib::error::{Classify, ErrorKind};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::config::{HistorySection, Paths};
use crate::daemon::AppState;
use crate::error::{Advice, Advise};
use crate::model::RepoId;
use crate::read::ReadError;
use store::{Store, StoreError};
use touch::Throttle;

/// The index's file name in a repository's directory.
pub const HISTORY_FILE: &str = "history.sqlite3";

/// How often the follower looks again with nothing new, as a fallback to
/// [`crate::subscribe::OpEvents`]' wake-ups.
const FOLLOW_EVERY: Duration = Duration::from_secs(5);

/// How long the follower lets operations gather after a wake-up, so a
/// burst is read in a few batches rather than one per operation.
const FOLLOW_GATHER: Duration = Duration::from_millis(200);

/// Failure of the history index.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HistoryError {
    /// `[history] enabled = false`.
    #[error("the history index is disabled")]
    Disabled,
    /// A malformed `QueryChanges` request.
    #[error("invalid history query: {message}")]
    InvalidQuery {
        /// The field.
        field: &'static str,
        /// What is wrong.
        message: String,
    },
    /// The repository is not open.
    #[error("repository {0} is not open")]
    NotOpen(RepoId),
    /// The cache failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The repository could not be read.
    #[error(transparent)]
    Loaded(#[from] ReadError),
    /// Reading the log or a commit failed.
    #[error("reading the repository failed: {0}")]
    Read(String),
}

impl From<reef_lib::jj::oplog::OpLogError> for HistoryError {
    fn from(error: reef_lib::jj::oplog::OpLogError) -> Self {
        HistoryError::Read(reef_lib::error::ErrorChain(&error).to_string())
    }
}

impl From<reef_lib::jj::oplog::OpLookupError> for HistoryError {
    fn from(error: reef_lib::jj::oplog::OpLookupError) -> Self {
        HistoryError::Read(reef_lib::error::ErrorChain(&error).to_string())
    }
}

impl Classify for HistoryError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Disabled => ErrorKind::Conflict,
            Self::InvalidQuery { .. } => ErrorKind::InvalidArgument,
            Self::NotOpen(_) => ErrorKind::Unavailable,
            Self::Loaded(error) => error.kind(),
            Self::Store(_) | Self::Read(_) => ErrorKind::Internal,
        }
    }
}

impl Advise for HistoryError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Disabled => Some(Advice::hint(
                "Set `[history] enabled = true` in config.toml and restart the daemon.",
            )),
            Self::InvalidQuery { .. } => Some(Advice::run(
                "See which filters `reef history` takes and their bounds.",
                ["reef", "history", "--help"],
            )),
            Self::Loaded(error) => error.advice(),
            _ => None,
        }
    }
}

/// One repository's index and the follower's lock on it.
#[derive(Debug)]
pub(crate) struct RepoHistory {
    pub(crate) store: Arc<Store>,
    /// Held by the follower while it reads and writes, and by GC from
    /// before its jj step until the ids it rewrote are mapped.
    pub(crate) lock: Arc<tokio::sync::Mutex<()>>,
    /// A backfill or catch-up walk is unfinished.
    pub(crate) backfilling: AtomicBool,
}

/// The history index of every repository.
pub struct History {
    config: HistorySection,
    paths: Paths,
    repos: Mutex<HashMap<RepoId, Arc<RepoHistory>>>,
    worker: Notify,
    throttle: Mutex<Throttle>,
    /// Touch computations since start, for tests.
    computed: AtomicU64,
    /// A test's clock for GC, in milliseconds; 0 for the real one.
    now: AtomicI64,
    /// Stops backfills after their first chunk, for tests.
    #[cfg(any(test, feature = "testing"))]
    backfill_paused: AtomicBool,
}

impl std::fmt::Debug for History {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("History")
            .field("enabled", &self.config.enabled)
            .field("computed", &self.computed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl History {
    /// The index for `config`.
    pub fn new(config: &HistorySection, paths: &Paths) -> Arc<History> {
        Arc::new(History {
            config: config.clone(),
            paths: paths.clone(),
            repos: Mutex::new(HashMap::new()),
            worker: Notify::new(),
            throttle: Mutex::new(Throttle::default()),
            computed: AtomicU64::new(0),
            now: AtomicI64::new(0),
            #[cfg(any(test, feature = "testing"))]
            backfill_paused: AtomicBool::new(false),
        })
    }

    /// Whether the index runs.
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// The index file of `repo`.
    pub fn path_of(&self, repo: RepoId) -> PathBuf {
        self.paths.repo_dir(repo).join(HISTORY_FILE)
    }

    /// Touch computations since the daemon started.
    pub fn computed(&self) -> u64 {
        self.computed.load(Ordering::Relaxed)
    }

    /// Sets when touches are computed, so a test need not wait a minute.
    #[cfg(any(test, feature = "testing"))]
    pub fn set_throttle(&self, quiet: Duration, stale: Duration) {
        *self.throttle.lock().unwrap_or_else(PoisonError::into_inner) = Throttle { quiet, stale };
        self.worker.notify_one();
    }

    /// Stops (or lets go on) every backfill after its current chunk.
    #[cfg(any(test, feature = "testing"))]
    pub fn pause_backfill(&self, paused: bool) {
        self.backfill_paused.store(paused, Ordering::Relaxed);
    }

    /// Sets GC's clock, in milliseconds since the epoch; 0 for the real one.
    #[cfg(any(test, feature = "testing"))]
    pub fn set_now(&self, now_ms: i64) {
        self.now.store(now_ms, Ordering::Relaxed);
    }

    pub(crate) fn now_ms(&self) -> Option<i64> {
        Some(self.now.load(Ordering::Relaxed)).filter(|now| *now != 0)
    }

    fn throttle(&self) -> Throttle {
        *self.throttle.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn wake_worker(&self) {
        self.worker.notify_one();
    }

    /// `repo`'s index, opened on first use; `None` with the index off.
    /// `create`: make the repository's directory if it is missing, for an
    /// open repository; GC never does, so a repository forgotten meanwhile
    /// leaves nothing behind.
    pub(crate) fn held(
        &self,
        repo: RepoId,
        create: bool,
    ) -> Result<Option<Arc<RepoHistory>>, HistoryError> {
        if !self.config.enabled {
            return Ok(None);
        }
        let mut repos = self.repos.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = repos.get(&repo) {
            return Ok(Some(Arc::clone(held)));
        }
        let path = self.path_of(repo);
        if let Some(dir) = path.parent() {
            if !create && !dir.is_dir() {
                // GC of a repository being forgotten: nothing to make.
                return Ok(None);
            }
            std::fs::create_dir_all(dir).map_err(|source| StoreError::Recreate {
                path: path.clone(),
                source,
            })?;
        }
        let (store, created) = Store::open(&path, 4)?;
        if created {
            tracing::info!(repo = %repo, path = %path.display(), "history index created; it is backfilled from the operation log");
        }
        let held = Arc::new(RepoHistory {
            store: Arc::new(store),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            backfilling: AtomicBool::new(created),
        });
        repos.insert(repo, Arc::clone(&held));
        Ok(Some(held))
    }

    /// `repo`'s index for a query.
    pub(crate) fn for_query(&self, repo: RepoId) -> Result<Arc<RepoHistory>, HistoryError> {
        self.held(repo, true)?.ok_or(HistoryError::Disabled)
    }

    /// Pauses `repo`'s follower until the guard drops: GC holds it from
    /// before its jj step until [`History::rewrite`].
    pub(crate) async fn pause(&self, repo: RepoId) -> Option<PauseGuard> {
        let held = self.held(repo, false).ok().flatten()?;
        Some(Arc::clone(&held.lock).lock_owned().await)
    }

    /// Closes `repo`'s index, before its directory is removed.
    pub fn forget(&self, repo: RepoId) {
        self.repos
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&repo);
    }

    /// Starts the follower and the worker, which run until `shutdown`.
    pub(crate) fn start(self: &Arc<Self>, state: Weak<AppState>, shutdown: CancellationToken) {
        if !self.config.enabled {
            return;
        }
        let history = Arc::clone(self);
        let (follow_state, follow_shutdown) = (state.clone(), shutdown.clone());
        tokio::spawn(async move {
            history.follow_loop(follow_state, follow_shutdown).await;
        });
        let history = Arc::clone(self);
        tokio::spawn(async move {
            history.touch_loop(state, shutdown).await;
        });
    }

    async fn follow_loop(self: Arc<Self>, state: Weak<AppState>, shutdown: CancellationToken) {
        let Some(mut changes) = state.upgrade().map(|s| s.events.changes()) else {
            return;
        };
        let mut follows = HashMap::new();
        loop {
            changes.borrow_and_update();
            let Some(state) = state.upgrade() else {
                return;
            };
            let open = state.events.repos();
            follows.retain(|repo, _| open.contains(repo));
            let mut more = false;
            for repo in open {
                if shutdown.is_cancelled() {
                    return;
                }
                // Followed, so the poll notices other jj writers.
                follows
                    .entry(repo)
                    .or_insert_with(|| state.events.follow(repo));
                // Forgotten since the list was read: leave it.
                if state.events.view(repo).is_none() {
                    continue;
                }
                let held = match self.held(repo, true) {
                    Ok(Some(held)) => held,
                    Ok(None) => return,
                    Err(error) => {
                        tracing::warn!(repo = %repo, error = %error, "cannot open the history index");
                        continue;
                    }
                };
                match self.step(&state, repo, &held).await {
                    Ok(again) => more |= again,
                    Err(error) => {
                        tracing::warn!(repo = %repo, error = %error, "following the operation log for history failed");
                    }
                }
            }
            drop(state);
            if more {
                tokio::task::yield_now().await;
                continue;
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                woken = changes.changed() => {
                    if woken.is_err() {
                        return;
                    }
                    tokio::select! {
                        () = shutdown.cancelled() => return,
                        () = tokio::time::sleep(FOLLOW_GATHER) => {}
                    }
                }
                () = tokio::time::sleep(FOLLOW_EVERY) => {}
            }
        }
    }

    async fn touch_loop(self: Arc<Self>, state: Weak<AppState>, shutdown: CancellationToken) {
        loop {
            let Some(app) = state.upgrade() else {
                return;
            };
            let mut soonest: Option<i64> = None;
            let mut worked = false;
            let repos: Vec<(RepoId, Arc<RepoHistory>)> = self
                .repos
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .map(|(id, held)| (*id, Arc::clone(held)))
                .collect();
            let now = follow::ms(std::time::SystemTime::now());
            let mut failed = false;
            for (repo, held) in repos {
                if shutdown.is_cancelled() {
                    return;
                }
                let (store, throttle) = (Arc::clone(&held.store), self.throttle());
                let due = follow::on_pool(move || touch::next_due(&store, throttle, now)).await;
                match due {
                    Ok(Ok(pending)) => {
                        if let Err(error) = self.touch(&app, repo, &held.store, pending).await {
                            // Not marked: back off rather than take the
                            // same change again at once.
                            tracing::warn!(repo = %repo, error = %error, "computing history touches failed");
                            failed = true;
                        } else {
                            worked = true;
                        }
                    }
                    Ok(Err(due)) => {
                        if let Some(due) = due {
                            soonest = Some(soonest.map_or(due, |s: i64| s.min(due)));
                        }
                    }
                    Err(error) => {
                        tracing::warn!(repo = %repo, error = %error, "reading the history index failed");
                    }
                }
            }
            drop(app);
            if worked && !failed {
                tokio::task::yield_now().await;
                continue;
            }
            if failed {
                soonest = Some(now + 1000);
            }
            let wait = soonest.map_or(Duration::from_secs(60), |due| {
                Duration::from_millis(u64::try_from((due - now).max(50)).unwrap_or(50))
            });
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = self.worker.notified() => {}
                () = tokio::time::sleep(wait.min(Duration::from_secs(60))) => {}
            }
        }
    }

    /// Opens every registered repository's index and starts following:
    /// after the repositories are open, before any request.
    pub(crate) fn attach(state: &Arc<AppState>) {
        state
            .history
            .start(Arc::downgrade(state), state.shutdown.clone());
    }
}

/// [`History::pause`]'s guard.
pub(crate) type PauseGuard = tokio::sync::OwnedMutexGuard<()>;
