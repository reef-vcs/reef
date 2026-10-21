//! Snapshots of workspaces: why one is taken ([`SnapshotKind`]) and the
//! [`Scheduler`] that takes them automatically (R1.13).
//!
//! # Scheduling
//!
//! The daemon's watchers ([`crate::watcher`]: at most one notify and one
//! Watchman watcher, each created when a workspace first needs it) report
//! debounced changes per workspace. Each tracked workspace has one scheduler entry: an event
//! marks it dirty, and a snapshot starts once `watcher.min_interval` has
//! passed since its last one finished. Events during a snapshot mark it
//! dirty again, so exactly one more follows. A workspace never has more than
//! one snapshot in flight, and the queue behind it is never deeper than one.
//!
//! Every `watcher.fallback_interval`, a workspace with no event since its
//! last snapshot is snapshotted anyway, which covers lost events (queue
//! overflow, `FSEvents` coalescing, a watch that failed to attach).
//!
//! Explicit snapshots ([`Scheduler::snapshot_now`], the `Snapshot` RPC,
//! `Inspect` with `snapshot_first`, release) wait for one in flight and then
//! run, so they always see the files as they are when they start.
//!
//! # Concurrency
//!
//! Scans run on tokio's blocking pool, at most `watcher.max_concurrent_scans`
//! at once; the repository thread runs only each snapshot's transaction
//! (R1.03, ADR 0012). The pipeline itself is
//! [`Manager`]'s, in `workspace/snapshot.rs`.
//!
//! jj-lib walks the working copy on rayon's current thread pool. Each scan
//! slot has its own pool: on the global pool, a worker waiting for one
//! scan's directory jobs steals jobs of other scans, so every scan in the
//! daemon finishes only when the slowest one it touched does (a workspace
//! hashing ten thousand new files would hold up all the others).

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};
use std::time::{Duration, Instant};

use reef_lib::error::ErrorChain;
use reef_lib::jj::snapshot::UntrackedReasonKind;
use reef_lib::jj::{CommitIdHex, OperationIdHex};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::config::{WatcherBackend, WatcherSection};
use crate::model::{WatcherKind, WorkspaceId};
use crate::watcher::{
    HealthHook, NoWatcher, WatchError, WatchEvent, Watcher, new_watcher, skip_prefixes,
};
use crate::workspace::{Manager, ManagerError};

pub mod index;

/// Events waiting for the scheduler; more are dropped (and counted) by the
/// watcher, and the fallback timer covers their workspaces.
const EVENT_QUEUE: usize = 4096;

/// How often the summary line is logged.
const SUMMARY_INTERVAL: Duration = Duration::from_mins(1);

/// The longest the scheduler waits between checks for due snapshots.
const MAX_TICK: Duration = Duration::from_millis(100);

/// Why a snapshot was taken, as R1.16's `snapshots.kind` records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotKind {
    /// The filesystem watcher saw changes, or the fallback timer fired.
    Watcher,
    /// A client asked: the `Snapshot` RPC, or `Inspect` with
    /// `snapshot_first`.
    Explicit,
    /// Before a release or reclaim.
    Final,
    /// Before a workspace moves to another change (R1.06).
    Move,
    /// While the daemon shuts down.
    Shutdown,
}

impl SnapshotKind {
    /// Every kind, in declaration order.
    pub const ALL: &[SnapshotKind] = &[
        Self::Watcher,
        Self::Explicit,
        Self::Final,
        Self::Move,
        Self::Shutdown,
    ];

    /// The stored name: `watcher`, `explicit`, `final`, `move`, `shutdown`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Watcher => "watcher",
            Self::Explicit => "explicit",
            Self::Final => "final",
            Self::Move => "move",
            Self::Shutdown => "shutdown",
        }
    }
}

/// Text that is not a [`SnapshotKind`]'s stored name.
#[derive(Debug, thiserror::Error)]
#[error("unknown snapshot kind {0:?}")]
pub struct UnknownSnapshotKind(pub String);

impl FromStr for SnapshotKind {
    type Err = UnknownSnapshotKind;

    /// Parses a stored name, as [`SnapshotKind::as_str`] writes it.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == text)
            .ok_or_else(|| UnknownSnapshotKind(text.to_owned()))
    }
}

impl ToSql for SnapshotKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for SnapshotKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|error| FromSqlError::Other(Box::new(error)))
    }
}

/// The operation description for a snapshot of `kind` (README, "Operation
/// descriptions"). `reason` is the parenthesized reason of a final snapshot
/// (default `released`) and the target change of a move.
pub fn description(kind: SnapshotKind, id: WorkspaceId, reason: Option<&str>) -> String {
    match (kind, reason) {
        (SnapshotKind::Final, reason) => format!(
            "reef: final snapshot of workspace {id} ({})",
            reason.unwrap_or("released")
        ),
        (SnapshotKind::Shutdown, _) => format!("reef: final snapshot of workspace {id} (shutdown)"),
        (SnapshotKind::Move, Some(change)) => format!("reef: move workspace {id} to {change}"),
        (SnapshotKind::Watcher | SnapshotKind::Explicit | SnapshotKind::Move, _) => {
            format!("reef: snapshot workspace {id}")
        }
    }
}

/// What one snapshot did.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent facts R1.16 and R1.20 read, not states of one machine"
)]
pub struct SnapshotOutcome {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// Why it ran.
    pub kind: SnapshotKind,
    /// Whether an operation was written.
    pub changed: bool,
    /// The working copy was stale with no edits of its own and was updated
    /// to its rewritten commit; no operation was written.
    pub adopted: bool,
    /// The snapshot rewrote a commit another workspace had already
    /// rewritten, so the change has more than one visible commit.
    pub divergent: bool,
    /// The view had no working-copy commit for the workspace, so it was
    /// given a new change.
    pub recovered: bool,
    /// The new operation when `changed`, else the head the scan ran at.
    pub op_id: OperationIdHex,
    /// The workspace's working-copy commit afterwards.
    pub commit_id: CommitIdHex,
    /// Its change, in hex.
    pub change_id: String,
    /// From the first event that triggered it (or from the request) to the
    /// working-copy state being saved.
    pub latency_ms: u64,
    /// Paths the watcher reported since the previous snapshot.
    pub paths_count: usize,
    /// How long jj's scan of the working copy took, in milliseconds: all
    /// tracked files for a notify workspace, the paths Watchman reported
    /// for a Watchman one (R1.14).
    pub scan_ms: u64,
}

/// A post-snapshot hook; see [`Scheduler::on_snapshot`].
pub type SnapshotHook = Box<dyn Fn(&SnapshotOutcome) + Send + Sync>;

/// Schedules every workspace's snapshots; one per daemon, owned by the
/// [`Manager`].
pub struct Scheduler {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("tracked", &self.inner.lock_entries().len())
            .finish_non_exhaustive()
    }
}

struct Inner {
    config: WatcherSection,
    /// Idle scan pools, one per permit in use at most.
    pools: Mutex<Vec<Arc<rayon::ThreadPool>>>,
    /// Threads in each scan pool.
    pool_threads: usize,
    manager: Weak<Manager>,
    /// The watchers, by kind, created on first use.
    watchers: Mutex<HashMap<WatcherKind, Arc<dyn Watcher>>>,
    /// What every watcher sends to.
    events_tx: mpsc::Sender<WatchEvent>,
    events: Mutex<Option<mpsc::Receiver<WatchEvent>>>,
    /// Given to watchers that report health.
    health: HealthHook,
    entries: Mutex<HashMap<WorkspaceId, Entry>>,
    scans: Arc<Semaphore>,
    hooks: RwLock<Vec<SnapshotHook>>,
    tasks: Mutex<JoinSet<()>>,
    stats: Mutex<Stats>,
    stop: CancellationToken,
    /// Test hook: drop every event, as if the watcher had lost them.
    drop_events: AtomicBool,
    /// Test hook: fail every watch, as if no watcher could start.
    refuse_watches: AtomicBool,
}

/// One tracked workspace.
#[derive(Debug)]
struct Entry {
    /// Which watcher watches it.
    watcher: WatcherKind,
    /// When the first event since the last snapshot began arrived.
    dirty_since: Option<Instant>,
    /// Paths reported since then.
    paths_count: usize,
    /// A watcher or fallback snapshot has been started and has not ended.
    task: bool,
    /// When the last snapshot ended (or tracking began).
    last_completed: Instant,
    /// Open [`PauseGuard`]s.
    paused: usize,
    /// Untracked paths already warned about.
    untracked_reported: HashSet<String>,
}

#[derive(Debug, Default)]
struct Stats {
    /// Scans holding a permit now, and the most ever.
    running: usize,
    running_max: usize,
    /// Scans waiting for a permit now.
    waiting: usize,
    /// This summary window: completed scans, the longest queue, the longest
    /// wait.
    window_scans: u64,
    window_queued_max: usize,
    window_wait_max: Duration,
}

/// What a snapshot run takes from its scheduler entry when it starts.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Trigger {
    /// The first event since the previous snapshot, if any.
    pub(crate) first_event: Option<Instant>,
    /// Paths reported since then.
    pub(crate) paths_count: usize,
}

impl Scheduler {
    /// A scheduler for `manager`. The notify watcher starts here unless
    /// `watcher.backend = "watchman"`; the Watchman watcher (and notify,
    /// then) when a workspace first needs it. A watcher that cannot start
    /// is replaced by none: its workspaces live on the fallback timer.
    /// Nothing is snapshotted automatically until [`Scheduler::start`].
    pub(crate) fn new(config: &WatcherSection, max_scans: usize, manager: Weak<Manager>) -> Self {
        let (tx, rx) = mpsc::channel(EVENT_QUEUE);
        let max_scans = max_scans.max(1);
        let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let scheduler = Self {
            inner: Arc::new_cyclic(|weak| Inner {
                config: config.clone(),
                pools: Mutex::new(Vec::new()),
                // Every pool can use every CPU, so a lone scan walks as fast
                // as on the global pool; idle pool threads only sleep. At
                // least two, so `install` leaves a worker for jj's scope.
                pool_threads: cpus.max(2),
                manager,
                watchers: Mutex::new(HashMap::new()),
                events_tx: tx,
                events: Mutex::new(Some(rx)),
                health: health_hook(weak.clone()),
                entries: Mutex::new(HashMap::new()),
                scans: Arc::new(Semaphore::new(max_scans)),
                hooks: RwLock::new(Vec::new()),
                tasks: Mutex::new(JoinSet::new()),
                stats: Mutex::new(Stats::default()),
                stop: CancellationToken::new(),
                drop_events: AtomicBool::new(false),
                refuse_watches: AtomicBool::new(false),
            }),
        };
        if config.backend != WatcherBackend::Watchman {
            // At start, so the daemon log shows the inotify limits.
            scheduler.inner.watcher(WatcherKind::Notify);
        }
        scheduler
    }

    /// Starts scheduling: watcher events, the fallback timer, and the
    /// per-minute summary, until `shutdown` is cancelled or
    /// [`Scheduler::stop`]. A second call does nothing.
    pub fn start(&self, shutdown: &CancellationToken) {
        let Some(events) = self
            .inner
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return;
        };
        let inner = Arc::clone(&self.inner);
        let shutdown = shutdown.clone();
        tokio::spawn(async move { inner.run(events, shutdown).await });
    }

    /// Stops scheduling and waits for watcher and fallback snapshots in
    /// flight to end. Explicit snapshots are their callers' to wait for.
    pub async fn stop(&self) {
        self.inner.stop.cancel();
        let mut tasks = std::mem::take(&mut *self.inner.lock_tasks());
        while tasks.join_next().await.is_some() {}
    }

    /// Starts watching a loaded workspace, with the watcher its row
    /// records, and scheduling its snapshots: on create, at daemon start for
    /// each active workspace, and after an aborted release. If its root
    /// cannot be watched, it is marked `attention = 'watch-failed'` and
    /// snapshotted on the fallback timer alone; a Watchman workspace is
    /// marked (and cleared) as its subscription goes down and comes up.
    /// Tracking a tracked workspace again re-reads its skip list.
    pub fn track(&self, ws: WorkspaceId) {
        let Some(manager) = self.inner.manager.upgrade() else {
            return;
        };
        let Some(entry) = manager.entry(ws) else {
            return;
        };
        let kind = entry.record().watcher;
        self.inner
            .lock_entries()
            .entry(ws)
            .or_insert_with(|| Entry {
                watcher: kind,
                dirty_since: None,
                paths_count: 0,
                task: false,
                last_completed: Instant::now(),
                paused: 0,
                untracked_reported: HashSet::new(),
            });
        // Events carry real paths (`/private/tmp` on macOS for `/tmp`).
        let root =
            std::fs::canonicalize(&entry.paths.root).unwrap_or_else(|_| entry.paths.root.clone());
        let spec = entry.spec();
        let skip = skip_prefixes(&root, &spec.excludes, &spec.mounts);
        let watcher = self.inner.watcher(kind);
        let outcome = if self.inner.refuse_watches.load(Ordering::Relaxed) {
            Err(WatchError::Unavailable(kind.as_str()))
        } else {
            watcher.watch(ws, &root, skip)
        };
        match outcome {
            Err(error) => {
                tracing::warn!(
                    workspace_id = %ws,
                    path = %root.display(),
                    watcher = kind.as_str(),
                    error = %ErrorChain(&error),
                    "cannot watch the workspace; it is snapshotted on the fallback timer only"
                );
                manager.mark_watch_failed(&entry);
            }
            // The health hook reports the outcome.
            Ok(()) if watcher.reports_health() => {}
            Ok(()) => manager.clear_watch_failed(&entry),
        }
    }

    /// Stops watching `ws` and scheduling its snapshots; events still in
    /// the queue for it are ignored. A snapshot in flight finishes.
    pub fn untrack(&self, ws: WorkspaceId) {
        let removed = self.inner.lock_entries().remove(&ws);
        let watchers: Vec<Arc<dyn Watcher>> = {
            let watchers = lock(&self.inner.watchers);
            match removed {
                Some(entry) => watchers.get(&entry.watcher).into_iter().cloned().collect(),
                None => watchers.values().cloned().collect(),
            }
        };
        for watcher in watchers {
            if let Err(error) = watcher.unwatch(ws) {
                tracing::warn!(workspace_id = %ws, error = %ErrorChain(&error), "cannot stop watching the workspace");
            }
        }
    }

    /// Snapshots `ws` now, after any snapshot in flight, and returns what
    /// it did. `reason` completes the description of a `Final` snapshot
    /// (`lease expired`; default `released`) or a `Move` (the target
    /// change). A `Final` or `Shutdown` snapshot also runs on a workspace
    /// that is `releasing`.
    pub async fn snapshot_now(
        &self,
        ws: WorkspaceId,
        kind: SnapshotKind,
        reason: Option<&str>,
    ) -> Result<SnapshotOutcome, SnapshotFailure> {
        let manager = self
            .inner
            .manager
            .upgrade()
            .ok_or(ManagerError::NotLoaded(ws))?;
        manager
            .snapshot_with(ws, kind, description(kind, ws, reason))
            .await
    }

    /// Holds off watcher and fallback snapshots of `ws` until the guard
    /// drops. Events still mark it dirty, and one snapshot runs after the
    /// drop if any arrived; [`Scheduler::snapshot_now`] still runs. R1.06
    /// (move) and R1.17 (GC) use it.
    pub fn pause(&self, ws: WorkspaceId) -> PauseGuard {
        let tracked = match self.inner.lock_entries().get_mut(&ws) {
            Some(entry) => {
                entry.paused += 1;
                true
            }
            None => false,
        };
        PauseGuard {
            inner: tracked.then(|| Arc::downgrade(&self.inner)),
            ws,
        }
    }

    /// Registers `hook` to run after every snapshot that ends well, after
    /// the working-copy state is saved and the row updated, in registration
    /// order, on the tokio runtime. R1.20's lockfile check runs just before
    /// them, in the pipeline itself, so it is done when the snapshot
    /// answers. The `snapshots` index row (R1.16) is not written here but before the
    /// working-copy state is saved, with the workspace's row; see
    /// `workspace/snapshot.rs`.
    pub fn on_snapshot(&self, hook: SnapshotHook) {
        self.inner
            .hooks
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(hook);
    }

    /// The tracked workspaces with watcher events not yet snapshotted.
    pub fn dirty(&self) -> Vec<WorkspaceId> {
        self.inner
            .lock_entries()
            .iter()
            .filter(|(_, entry)| entry.dirty_since.is_some())
            .map(|(ws, _)| *ws)
            .collect()
    }

    /// Whether `ws` is tracked.
    pub fn is_tracked(&self, ws: WorkspaceId) -> bool {
        self.inner.lock_entries().contains_key(&ws)
    }

    /// The most scans that ever ran at once.
    pub fn max_running(&self) -> usize {
        self.inner.lock_stats().running_max
    }

    /// Test hook: from now on, every watcher event is dropped, as if the
    /// watcher had lost it; only the fallback timer and explicit snapshots
    /// run.
    #[cfg(any(test, feature = "testing"))]
    pub fn pause_watcher(&self) {
        self.inner.drop_events.store(true, Ordering::Relaxed);
    }

    /// Test hook: from now on, every workspace tracked fails to be watched,
    /// as if its watcher could not start: it is marked `watch-failed` and
    /// lives on the fallback timer.
    #[cfg(any(test, feature = "testing"))]
    pub fn refuse_watches(&self) {
        self.inner.refuse_watches.store(true, Ordering::Relaxed);
    }

    /// Whether automatic snapshots of `ws` are paused.
    pub(crate) fn is_paused(&self, ws: WorkspaceId) -> bool {
        self.inner
            .lock_entries()
            .get(&ws)
            .is_some_and(|entry| entry.paused > 0)
    }

    /// The manager, while it exists.
    pub(crate) fn manager(&self) -> Option<Arc<Manager>> {
        self.inner.manager.upgrade()
    }

    /// A scan permit, waiting for one if `watcher.max_concurrent_scans`
    /// are running. Counted in the summary.
    pub(crate) async fn acquire(&self) -> ScanPermit {
        /// Counts a waiter, also when the wait is cancelled.
        struct Waiting<'a>(&'a Inner);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                let mut stats = self.0.lock_stats();
                stats.waiting = stats.waiting.saturating_sub(1);
            }
        }
        let waiting = {
            let mut stats = self.inner.lock_stats();
            stats.waiting += 1;
            stats.window_queued_max = stats.window_queued_max.max(stats.waiting);
            Waiting(&self.inner)
        };
        let begun = Instant::now();
        // The semaphore is never closed.
        let permit = Arc::clone(&self.inner.scans).acquire_owned().await.ok();
        let waited = begun.elapsed();
        drop(waiting);
        {
            let mut stats = self.inner.lock_stats();
            stats.running += 1;
            stats.running_max = stats.running_max.max(stats.running);
            stats.window_wait_max = stats.window_wait_max.max(waited);
        }
        let pool = self.inner.take_pool();
        ScanPermit {
            _permit: permit,
            pool,
            inner: Arc::clone(&self.inner),
        }
    }

    /// What a starting snapshot of `ws` covers: clears its dirty mark.
    pub(crate) fn take_trigger(&self, ws: WorkspaceId) -> Trigger {
        let mut entries = self.inner.lock_entries();
        let Some(entry) = entries.get_mut(&ws) else {
            return Trigger::default();
        };
        Trigger {
            first_event: entry.dirty_since.take(),
            paths_count: std::mem::take(&mut entry.paths_count),
        }
    }

    /// Records that a snapshot of `ws` ended.
    pub(crate) fn completed(&self, ws: WorkspaceId) {
        if let Some(entry) = self.inner.lock_entries().get_mut(&ws) {
            entry.last_completed = Instant::now();
        }
    }

    /// Warns once per path about new files a snapshot of `ws` left untracked.
    pub(crate) fn report_untracked(
        &self,
        ws: WorkspaceId,
        untracked: &[(String, UntrackedReasonKind)],
    ) {
        if untracked.is_empty() {
            return;
        }
        let mut entries = self.inner.lock_entries();
        let mut reported = entries
            .get_mut(&ws)
            .map(|entry| &mut entry.untracked_reported);
        for (path, reason) in untracked {
            if reported
                .as_mut()
                .is_some_and(|reported| !reported.insert(path.clone()))
            {
                continue;
            }
            if let UntrackedReasonKind::FileTooLarge { size, max_size } = reason {
                tracing::warn!(
                    workspace_id = %ws,
                    path = %path,
                    size_bytes = size,
                    max_size_bytes = max_size,
                    "new file left untracked: larger than snapshot.max_new_file_size"
                );
            } else {
                tracing::warn!(workspace_id = %ws, path = %path, "new file left untracked");
            }
        }
    }

    /// Runs the post-snapshot hooks.
    pub(crate) fn run_hooks(&self, outcome: &SnapshotOutcome) {
        let hooks = self
            .inner
            .hooks
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        for hook in hooks.iter() {
            hook(outcome);
        }
    }

    /// Watches a tracked workspace again with its current skip list.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn rewatch(&self, ws: WorkspaceId) {
        if self.is_tracked(ws) {
            self.track(ws);
        }
    }
}

/// The error of [`Scheduler::snapshot_now`].
pub type SnapshotFailure = ManagerError;

/// Holds off automatic snapshots of one workspace; see
/// [`Scheduler::pause`].
#[must_use = "dropping the guard resumes automatic snapshots at once"]
#[derive(Debug)]
pub struct PauseGuard {
    inner: Option<Weak<Inner>>,
    ws: WorkspaceId,
}

impl Drop for PauseGuard {
    fn drop(&mut self) {
        let Some(inner) = self.inner.as_ref().and_then(Weak::upgrade) else {
            return;
        };
        if let Some(entry) = inner.lock_entries().get_mut(&self.ws) {
            entry.paused = entry.paused.saturating_sub(1);
        }
    }
}

/// A scan slot and its thread pool; released on drop.
pub(crate) struct ScanPermit {
    _permit: Option<OwnedSemaphorePermit>,
    pool: Option<Arc<rayon::ThreadPool>>,
    inner: Arc<Inner>,
}

impl ScanPermit {
    /// The slot's own rayon pool, or `None` (the global pool) if one could
    /// not be built.
    pub(crate) fn pool(&self) -> Option<Arc<rayon::ThreadPool>> {
        self.pool.clone()
    }
}

impl Drop for ScanPermit {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            self.inner.lock_pools().push(pool);
        }
        let mut stats = self.inner.lock_stats();
        stats.running = stats.running.saturating_sub(1);
        stats.window_scans += 1;
    }
}

impl Inner {
    fn lock_entries(&self) -> std::sync::MutexGuard<'_, HashMap<WorkspaceId, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_stats(&self) -> std::sync::MutexGuard<'_, Stats> {
        self.stats.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_tasks(&self) -> std::sync::MutexGuard<'_, JoinSet<()>> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_pools(&self) -> std::sync::MutexGuard<'_, Vec<Arc<rayon::ThreadPool>>> {
        self.pools.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The watcher of `kind`, starting it if this is its first use. One
    /// that cannot start is logged once and replaced by a [`NoWatcher`].
    fn watcher(&self, kind: WatcherKind) -> Arc<dyn Watcher> {
        let mut watchers = lock(&self.watchers);
        if let Some(watcher) = watchers.get(&kind) {
            return Arc::clone(watcher);
        }
        let started = new_watcher(
            kind,
            &self.config,
            self.events_tx.clone(),
            Arc::clone(&self.health),
        );
        let watcher: Arc<dyn Watcher> = match started {
            Ok(watcher) => Arc::from(watcher),
            Err(error) => {
                tracing::error!(
                    watcher = kind.as_str(),
                    error = %ErrorChain(&error),
                    "cannot start the filesystem watcher; its workspaces are snapshotted on the \
                     fallback timer only"
                );
                Arc::new(NoWatcher { kind })
            }
        };
        watchers.insert(kind, Arc::clone(&watcher));
        watcher
    }

    /// An idle scan pool, building one if none is idle. Never more pools
    /// than permits exist, since each permit holds at most one.
    fn take_pool(&self) -> Option<Arc<rayon::ThreadPool>> {
        if let Some(pool) = self.lock_pools().pop() {
            return Some(pool);
        }
        let built = rayon::ThreadPoolBuilder::new()
            .num_threads(self.pool_threads)
            .thread_name(|i| format!("reef-scan-{i}"))
            .build();
        match built {
            Ok(pool) => Some(Arc::new(pool)),
            Err(error) => {
                tracing::warn!(
                    error = %ErrorChain(&error),
                    "cannot build a scan thread pool; using the global pool"
                );
                None
            }
        }
    }

    /// The scheduling loop.
    async fn run(
        self: Arc<Self>,
        mut events: mpsc::Receiver<WatchEvent>,
        shutdown: CancellationToken,
    ) {
        let period = (self.config.min_interval / 2).clamp(Duration::from_millis(10), MAX_TICK);
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut summary = tokio::time::interval_at(
            tokio::time::Instant::now() + SUMMARY_INTERVAL,
            SUMMARY_INTERVAL,
        );
        // Without a watcher the channel is closed from the start; the timers
        // still run.
        let mut events_open = true;
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = self.stop.cancelled() => break,
                event = events.recv(), if events_open => match event {
                    Some(event) => self.on_event(&event),
                    None => events_open = false,
                },
                _ = tick.tick() => self.start_due(),
                _ = summary.tick() => self.log_summary(),
            }
        }
        tracing::debug!("snapshot scheduler stopped");
    }

    fn on_event(&self, event: &WatchEvent) {
        if self.drop_events.load(Ordering::Relaxed) {
            return;
        }
        let mut entries = self.lock_entries();
        // Events for an untracked workspace (released, or racing its
        // unwatch) are ignored.
        let Some(entry) = entries.get_mut(&event.workspace) else {
            return;
        };
        entry.dirty_since.get_or_insert(event.at);
        entry.paths_count += event.paths.len();
    }

    /// Starts every snapshot that is due: dirty workspaces past
    /// `min_interval`, and clean ones past `fallback_interval`.
    fn start_due(self: &Arc<Self>) {
        if self.stop.is_cancelled() {
            return;
        }
        let now = Instant::now();
        let mut due = Vec::new();
        for (ws, entry) in self.lock_entries().iter_mut() {
            if entry.task || entry.paused > 0 {
                continue;
            }
            let since = now.saturating_duration_since(entry.last_completed);
            let fallback = match entry.dirty_since {
                Some(_) if since >= self.config.min_interval => false,
                None if since >= self.config.fallback_interval => true,
                _ => continue,
            };
            entry.task = true;
            due.push((*ws, fallback));
        }
        let mut tasks = self.lock_tasks();
        while tasks.try_join_next().is_some() {}
        for (ws, fallback) in due {
            let inner = Arc::clone(self);
            tasks.spawn(async move { inner.run_task(ws, fallback).await });
        }
    }

    async fn run_task(self: Arc<Self>, ws: WorkspaceId, fallback: bool) {
        /// Clears the entry's task flag however the task ends, so a panic
        /// does not stop the workspace's scheduled snapshots for good.
        struct TaskDone<'a>(&'a Inner, WorkspaceId);
        impl Drop for TaskDone<'_> {
            fn drop(&mut self) {
                if let Some(entry) = self.0.lock_entries().get_mut(&self.1) {
                    entry.task = false;
                }
            }
        }
        let _done = TaskDone(&self, ws);
        let ran = match self.manager.upgrade() {
            Some(manager) => manager.scheduled_snapshot(ws, fallback).await,
            None => false,
        };
        if ran && let Some(entry) = self.lock_entries().get_mut(&ws) {
            entry.last_completed = Instant::now();
        }
    }

    fn log_summary(&self) {
        if self.lock_entries().is_empty() {
            return;
        }
        let (scans, queued_max, wait_max) = {
            let mut stats = self.lock_stats();
            let window = (
                stats.window_scans,
                stats.window_queued_max,
                stats.window_wait_max,
            );
            stats.window_scans = 0;
            stats.window_queued_max = stats.waiting;
            stats.window_wait_max = Duration::ZERO;
            window
        };
        // R1.23 parses this line from the log; keep its shape.
        tracing::info!(
            "watcher summary: scans={scans} queued_max={queued_max} wait_max_ms={}",
            wait_max.as_millis()
        );
    }
}

/// The scheduler's [`HealthHook`]: marks or clears `watch-failed` on a
/// tracked workspace as its watch goes down or comes up.
fn health_hook(inner: Weak<Inner>) -> HealthHook {
    Arc::new(move |ws, healthy| {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        if !inner.lock_entries().contains_key(&ws) {
            return;
        }
        let Some(manager) = inner.manager.upgrade() else {
            return;
        };
        let Some(entry) = manager.entry(ws) else {
            return;
        };
        // A release untracks first, but a report already past the check
        // above must not mark a row on its way out.
        if entry.record().state != crate::model::WorkspaceState::Active {
            return;
        }
        if healthy {
            manager.clear_watch_failed(&entry);
        } else {
            manager.mark_watch_failed(&entry);
        }
    })
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Manager {
    /// Clears `attention = 'watch-failed'` from `entry` once it is watched
    /// again (a later start, a raised limit); failures are logged.
    pub(crate) fn clear_watch_failed(&self, entry: &crate::workspace::WorkspaceEntry) {
        if entry.record().attention != Some(crate::model::Attention::WatchFailed) {
            return;
        }
        let id = entry.paths.id;
        match self.db().set_workspace_attention(id, None, None) {
            Ok(()) => entry.set_attention(None),
            Err(error) => {
                tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "cannot clear the workspace's mark");
            }
        }
    }

    /// Marks `entry` as unwatched (`attention = 'watch-failed'`) in the
    /// database and the loaded row; failures are logged. Another mark
    /// (`stale-recovered`, R1.20's store marks) is kept rather than replaced:
    /// a Watchman restart marks and clears every Watchman workspace, and
    /// would otherwise erase it.
    pub(crate) fn mark_watch_failed(&self, entry: &crate::workspace::WorkspaceEntry) {
        let id = entry.paths.id;
        let attention = Some(crate::model::Attention::WatchFailed);
        let current = entry.record().attention;
        if current == attention {
            return;
        }
        if let Some(other) = current {
            tracing::debug!(
                workspace_id = %id,
                attention = other.as_str(),
                "watch failed; keeping the workspace's other mark"
            );
            return;
        }
        match self.db().set_workspace_attention(id, attention, None) {
            Ok(()) => entry.set_attention(attention),
            Err(error) => {
                tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "cannot mark the workspace");
            }
        }
    }
}
