//! The board `WatchInbox` streams are served from (#372, ADR 0091): one
//! task, started by the first watcher and stopped with the last, that reads
//! every registered repository's inbox and reads one again when something
//! that can change it is recorded.
//!
//! Triggers are levels, not events: an operation of a kind
//! `inbox::Object::triggers` lists, a change to a presence entry of a kind
//! it lists, a repository opened or forgotten, or an item's words turning
//! stale mark a repository dirty, and the next read replaces its snapshot.
//! Any gap (the operation stream ending, presence lagging) marks every
//! repository. So a restart is a fresh read, and nothing applies an event
//! to state. Each repository is read on its own, one read of it at a time,
//! so a repository whose thread is busy holds back only its own reads.
//! Reads are paced by R2.19's rule, counting the CPU time they used
//! (`work::worked`), not the time they waited: batches of reads start at
//! most every 250 ms, and each read, from its start, holds back the next
//! batch for four times its work, laid end to end with every other read's.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{FutureExt as _, StreamExt as _};
use reef_api::v1::{self, SubscribeOperationsRequest};
use tokio::sync::{broadcast, watch};
use tokio::time::Instant;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::daemon::AppState;
use crate::inbox::board::RepoBoard;
use crate::inbox::{refresh_operations, refresh_presence};
use crate::model::{RepoId, Timestamp};
use crate::presence::PresenceFilter;

/// The least time between two starts of reads.
const MIN_SPACING: Duration = Duration::from_millis(250);
/// A read holds back the next for this many times its work, so reading
/// takes at most a quarter of the task's time.
const SPACING_FACTOR: u32 = 4;
/// The first retry of a repository that failed to read, doubled up to
/// [`MAX_RETRY`].
const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_mins(1);

/// What every stream reads: each registered repository's snapshot.
#[derive(Debug, Default)]
pub(crate) struct Board {
    pub(crate) repos: BTreeMap<RepoId, Arc<RepoBoard>>,
    /// Every repository registered when this was set had been read; it
    /// stays set.
    pub(crate) ready: bool,
}

/// The board's owner; one in [`AppState::inbox`].
#[derive(Debug)]
pub struct InboxWatch {
    shared: Arc<Mutex<Weak<Shared>>>,
    marks: watch::Sender<u64>,
    reads: Arc<AtomicU64>,
}

impl Default for InboxWatch {
    fn default() -> InboxWatch {
        InboxWatch {
            shared: Arc::new(Mutex::new(Weak::new())),
            marks: watch::Sender::new(0),
            reads: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// What the watchers of one running task share; the task stops when the
/// last is dropped.
#[derive(Debug)]
struct Shared {
    board: watch::Receiver<Arc<Board>>,
    _stop: DropGuard,
}

/// One watcher's view: the newest board, and a bump whenever a mark moved.
#[derive(Debug)]
pub(crate) struct Watch {
    pub(crate) board: watch::Receiver<Arc<Board>>,
    pub(crate) marks: watch::Receiver<u64>,
    _shared: Arc<Shared>,
}

impl InboxWatch {
    /// A watcher of the board, starting the task if none runs.
    pub(crate) fn watch(&self, state: &Arc<AppState>) -> Watch {
        let mut held = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        let shared = if let Some(shared) = held.upgrade() {
            shared
        } else {
            let (tx, rx) = watch::channel(Arc::new(Board::default()));
            let stop = state.shutdown.child_token();
            let shared = Arc::new(Shared {
                board: rx,
                _stop: stop.clone().drop_guard(),
            });
            let mine = Arc::downgrade(&shared);
            *held = mine.clone();
            let (state, reads, owner) = (
                Arc::clone(state),
                Arc::clone(&self.reads),
                Arc::clone(&self.shared),
            );
            tokio::spawn(async move {
                let ran = AssertUnwindSafe(run(state, tx, stop, reads))
                    .catch_unwind()
                    .await;
                if ran.is_err() {
                    tracing::error!("the inbox's board task panicked");
                }
                // A watcher that comes later starts a new task.
                let mut held = owner.lock().unwrap_or_else(PoisonError::into_inner);
                if held.ptr_eq(&mine) {
                    *held = Weak::new();
                }
            });
            shared
        };
        Watch {
            board: shared.board.clone(),
            marks: self.marks.subscribe(),
            _shared: shared,
        }
    }

    /// A watcher of a board a test publishes, with no task.
    #[cfg(test)]
    pub(crate) fn of(&self, board: watch::Receiver<Arc<Board>>) -> Watch {
        Watch {
            board: board.clone(),
            marks: self.marks.subscribe(),
            _shared: Arc::new(Shared {
                board,
                _stop: CancellationToken::new().drop_guard(),
            }),
        }
    }

    /// `MarkInboxSeen` moved a mark: watchers read their digests again.
    pub(crate) fn marks_moved(&self) {
        self.marks.send_modify(|n| *n += 1);
    }

    /// Repository reads the task has finished since the daemon started.
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }
}

/// What the task knows besides the board.
struct Task {
    state: Arc<AppState>,
    board: watch::Sender<Arc<Board>>,
    dirty: BTreeSet<RepoId>,
    /// Repositories that failed to read: when to retry, and the next wait.
    retry: HashMap<RepoId, (Instant, Duration)>,
    /// The open repositories as `OpEvents` last listed them.
    open: HashSet<RepoId>,
    /// The registered repositories were listed since `open` last moved.
    listed: bool,
    /// Repositories with a read in flight.
    reading: HashSet<RepoId>,
    pacing: Pacing,
    rev: u64,
    reads: Arc<AtomicU64>,
}

/// When the next batch of reads may start.
#[derive(Debug)]
struct Pacing {
    last_start: Option<Instant>,
    /// Each read's work, times [`SPACING_FACTOR`], laid end to end from
    /// its start.
    paced: Instant,
}

impl Pacing {
    fn new(now: Instant) -> Pacing {
        Pacing {
            last_start: None,
            paced: now,
        }
    }

    fn next(&self) -> Instant {
        self.last_start
            .map_or(self.paced, |at| self.paced.max(at + MIN_SPACING))
    }

    fn started(&mut self, at: Instant) {
        self.last_start = Some(at);
    }

    fn finished(&mut self, started: Instant, worked: Duration) {
        self.paced = self.paced.max(started) + worked * SPACING_FACTOR;
    }
}

/// One repository's finished read.
struct Read {
    repo: RepoId,
    last: Option<Arc<RepoBoard>>,
    result: Result<RepoBoard, tonic::Status>,
    started: Instant,
    /// The time spent working for it, not waiting (`work::worked`).
    worked: Duration,
}

type Reads = FuturesUnordered<BoxFuture<'static, Read>>;

async fn run(
    state: Arc<AppState>,
    board: watch::Sender<Arc<Board>>,
    stop: CancellationToken,
    reads: Arc<AtomicU64>,
) {
    // The triggers before the first read, so nothing between is missed.
    let filter = PresenceFilter {
        kinds: Some(refresh_presence().into_iter().collect()),
        ..PresenceFilter::default()
    };
    let (_, mut presence) = state.presence.subscribe(&filter);
    let mut operations = subscribe(&state).await;
    let mut events = state.events.changes();
    let mut task = Task::new(state, board, reads);
    let mut reading = Reads::new();
    task.reconcile(true);
    // A refused subscription is retried like one that ended.
    let mut reconnect: Option<Instant> = operations.is_none().then(|| Instant::now() + FIRST_RETRY);
    let mut backoff = FIRST_RETRY;
    loop {
        if task.due(Instant::now()) {
            task.start(&mut reading);
            continue;
        }
        // Repositories that could not be listed are listed again.
        let relist = (!task.listed).then(|| Instant::now() + FIRST_RETRY * 5);
        let wake = [task.next_due(Instant::now()), reconnect, relist]
            .into_iter()
            .flatten()
            .min();
        let sleep = async {
            match wake {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            Some(read) = reading.next(), if !reading.is_empty() => task.finish(read),
            frame = next(&mut operations) => {
                if let Some(Ok(frame)) = frame {
                    backoff = FIRST_RETRY;
                    for operation in frame.operations {
                        if let Ok(repo) = operation.repo_id.parse() {
                            task.dirty.insert(repo);
                        }
                    }
                } else {
                    // The stream ended (a rewrite, too far behind, an
                    // error): anything may have been missed.
                    operations = None;
                    reconnect = Some(Instant::now() + backoff);
                    backoff = (backoff * 2).min(MAX_RETRY);
                    task.dirty_all();
                }
            }
            received = presence.recv() => match received {
                // The broadcast carries every kind; the filter shaped only
                // the set.
                Ok(change) if !filter.matches(&change) => {}
                Ok(change) => match change.repo {
                    Some(repo) => {
                        task.dirty.insert(repo);
                    }
                    None => task.dirty_all(),
                },
                Err(broadcast::error::RecvError::Lagged(_)) => task.dirty_all(),
                Err(broadcast::error::RecvError::Closed) => return,
            },
            changed = events.changed() => {
                if changed.is_err() {
                    return;
                }
                task.reconcile(false);
            }
            () = sleep => {
                if !task.listed {
                    task.reconcile(true);
                }
                if reconnect.is_some_and(|at| at <= Instant::now()) {
                    reconnect = None;
                    operations = subscribe(&task.state).await;
                    if operations.is_none() {
                        reconnect = Some(Instant::now() + backoff);
                        backoff = (backoff * 2).min(MAX_RETRY);
                    }
                    task.dirty_all();
                }
            }
        }
    }
}

type Frames = std::pin::Pin<
    Box<dyn futures::Stream<Item = Result<v1::SubscribeOperationsResponse, tonic::Status>> + Send>,
>;

/// The operations of every kind some inbox object changes on, in every
/// repository, those registered later included; `None` when the
/// subscription fails (it is retried).
async fn subscribe(state: &Arc<AppState>) -> Option<Frames> {
    let handler = crate::service::operations::OperationsHandler::new(Arc::clone(state));
    let request = SubscribeOperationsRequest {
        filter: Some(v1::OperationFilter {
            kinds: refresh_operations(),
            ..v1::OperationFilter::default()
        }),
        ..SubscribeOperationsRequest::default()
    };
    // One subscription for the daemon, taken past the cap, so clients
    // holding every slot cannot leave the board without its triggers, and
    // a stream opens even at `max_subscriptions = 1`.
    let slot = match state.subscriptions.admit(usize::MAX) {
        Ok(slot) => slot,
        Err(error) => {
            tracing::warn!(%error, "the inbox could not follow the operation log");
            return None;
        }
    };
    match handler.subscribe_admitted(request, move || Ok(slot)).await {
        Ok(frames) => Some(frames),
        Err(error) => {
            tracing::warn!(%error, "the inbox could not follow the operation log");
            None
        }
    }
}

async fn next(
    frames: &mut Option<Frames>,
) -> Option<Result<v1::SubscribeOperationsResponse, tonic::Status>> {
    match frames {
        Some(frames) => frames.next().await,
        None => std::future::pending().await,
    }
}

impl Task {
    fn new(state: Arc<AppState>, board: watch::Sender<Arc<Board>>, reads: Arc<AtomicU64>) -> Task {
        Task {
            open: state.events.repos().into_iter().collect(),
            listed: false,
            state,
            board,
            dirty: BTreeSet::new(),
            retry: HashMap::new(),
            reading: HashSet::new(),
            pacing: Pacing::new(Instant::now()),
            rev: 0,
            reads,
        }
    }

    fn dirty_all(&mut self) {
        let repos: Vec<RepoId> = self.board.borrow().repos.keys().copied().collect();
        self.dirty.extend(repos);
        // A read in flight may have read some sources before the gap.
        self.dirty.extend(self.reading.iter().copied());
    }

    /// Follows repositories opened or forgotten: a new one is read, and a
    /// forgotten one leaves the board. `force` lists them whatever
    /// `OpEvents` says, as at the start.
    fn reconcile(&mut self, force: bool) {
        let open: HashSet<RepoId> = self.state.events.repos().into_iter().collect();
        if open == self.open && self.listed && !force {
            return;
        }
        self.open = open;
        let registered = match super::registered(&self.state) {
            Ok(repos) => repos,
            Err(error) => {
                // Listed again at the next change `OpEvents` reports.
                self.listed = false;
                tracing::warn!(%error, "the inbox could not list the repositories");
                return;
            }
        };
        self.listed = true;
        let board = Arc::clone(&self.board.borrow());
        for repo in &registered {
            if !board.repos.contains_key(repo) {
                self.dirty.insert(*repo);
            }
        }
        let gone: Vec<RepoId> = board
            .repos
            .keys()
            .filter(|repo| !registered.contains(repo))
            .copied()
            .collect();
        for repo in &gone {
            self.dirty.remove(repo);
            self.retry.remove(repo);
        }
        let mut repos = board.repos.clone();
        for repo in &gone {
            repos.remove(repo);
        }
        // With nothing to read, the board is complete as it stands.
        let ready = board.ready || registered.iter().all(|repo| repos.contains_key(repo));
        if !gone.is_empty() || ready != board.ready {
            self.board.send_replace(Arc::new(Board { repos, ready }));
        }
    }

    /// When the next read may start, as of `now`, if any repository with
    /// no read in flight waits for one.
    fn next_due(&self, now: Instant) -> Option<Instant> {
        let wall = Timestamp::now();
        let idle = |repo: &RepoId| !self.reading.contains(repo);
        let restale = self
            .board
            .borrow()
            .repos
            .iter()
            .filter(|(repo, _)| idle(repo))
            .filter_map(|(_, repo)| repo.restale_at)
            .min()
            .map(|at| {
                now + Duration::from_millis(at.0.saturating_sub(wall.0).max(0).unsigned_abs())
            });
        let retry = self
            .retry
            .iter()
            .filter(|(repo, _)| idle(repo))
            .map(|(_, (at, _))| *at)
            .min();
        let wanted = if self.dirty.iter().any(idle) {
            now
        } else {
            [restale, retry].into_iter().flatten().min()?
        };
        Some(self.pacing.next().max(wanted))
    }

    fn due(&self, now: Instant) -> bool {
        self.next_due(now).is_some_and(|at| at <= now)
    }

    /// Starts a read of every repository due that has none in flight.
    fn start(&mut self, reading: &mut Reads) {
        let start = Instant::now();
        let now = Timestamp::now();
        let mut due: BTreeSet<RepoId> = self.dirty.clone();
        for (repo, (at, _)) in &self.retry {
            if *at <= start {
                due.insert(*repo);
            }
        }
        for (repo, board) in &self.board.borrow().repos {
            if board.restale_at.is_some_and(|at| at <= now) {
                due.insert(*repo);
            }
        }
        // A repository being read is read again once that read finishes.
        due.retain(|repo| !self.reading.contains(repo));
        // Even with nothing due (a deadline a moment early by the wall
        // clock), the spacing holds.
        self.pacing.started(start);
        for repo in due {
            self.dirty.remove(&repo);
            self.reading.insert(repo);
            let last = self.board.borrow().repos.get(&repo).cloned();
            reading.push(read(Arc::clone(&self.state), repo, last).boxed());
        }
    }

    /// Publishes a finished read, if a stream could tell it from the last.
    fn finish(&mut self, read: Read) {
        let Read {
            repo,
            last,
            result,
            started,
            worked,
        } = read;
        self.reading.remove(&repo);
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.pacing.finished(started, worked);
        let wait = self.retry.get(&repo).map(|(_, wait)| *wait);
        let (board, again) = match result {
            Ok(board) => {
                // Read, but the ledger's write failed: the items stand, raise
                // 0, and are read again so the write lands once it can.
                let again = !board.settled;
                (board, again)
            }
            Err(error) => (
                RepoBoard::failed(last.as_deref(), super::inbox_error(&error)),
                true,
            ),
        };
        self.retry.remove(&repo);
        if again {
            let wait = wait.unwrap_or(FIRST_RETRY);
            self.retry
                .insert(repo, (Instant::now() + wait, (wait * 2).min(MAX_RETRY)));
        }
        // A repository forgotten during the read leaves with `reconcile`.
        let registered = super::registered(&self.state).ok();
        let kept = registered
            .as_ref()
            .is_none_or(|registered| registered.contains(&repo));
        if !kept {
            self.retry.remove(&repo);
        }
        let changed = kept && last.as_deref().is_none_or(|last| !last.same(&board));
        let current = Arc::clone(&self.board.borrow());
        let mut repos = current.repos.clone();
        if changed {
            self.rev += 1;
            let mut board = board;
            board.rev = self.rev;
            repos.insert(repo, Arc::new(board));
        }
        let ready = current.ready
            || registered
                .is_some_and(|registered| registered.iter().all(|repo| repos.contains_key(repo)));
        if changed || ready != current.ready {
            self.board.send_replace(Arc::new(Board { repos, ready }));
        }
    }
}

/// One repository's read, timed.
async fn read(state: Arc<AppState>, repo: RepoId, last: Option<Arc<RepoBoard>>) -> Read {
    let started = Instant::now();
    let (result, worked) = crate::work::worked(snapshot(&state, repo, last.as_deref())).await;
    Read {
        repo,
        last,
        result,
        started,
        worked,
    }
}

/// One repository's snapshot, its subjects reused from `last` when its
/// newest landings did not move.
async fn snapshot(
    state: &Arc<AppState>,
    repo: RepoId,
    last: Option<&RepoBoard>,
) -> Result<RepoBoard, tonic::Status> {
    let hex = repo.as_hex();
    let workspaces = super::attention(state, &hex)
        .await?
        .remove(&hex)
        .unwrap_or_default();
    let buckets = super::buckets(state, repo, workspaces).await?;
    let newest = super::newest_landed(state, repo)?;
    let subjects = match last {
        Some(last)
            if last.subjects.len() == newest.len()
                && newest
                    .iter()
                    .all(|row| last.subjects.contains_key(&row.change_id)) =>
        {
            Arc::clone(&last.subjects)
        }
        _ => Arc::new(super::read_subjects(state, &hex, &newest).await?),
    };
    let mut board = RepoBoard::read(repo, buckets, subjects);
    if let Err(error) = crate::inbox::raises::settle(&state.db, repo, &mut board, last) {
        // The items stand, unnumbered; the next read writes the ledger again.
        tracing::warn!(
            repo = %hex,
            error = %reef_lib::error::ErrorChain(&error),
            "the raise ledger's write failed; the inbox reads its items with raise 0"
        );
    }
    Ok(board)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestDaemon;

    // #387 review: a gap while a repository's first read is in flight is
    // that repository's too, though it is not on the board yet.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_gap_during_a_read_reads_that_repository_again() {
        let (daemon, repo) =
            TestDaemon::with_synthetic_repo(reef_testing::synth::Preset::Small).await;
        let repo: RepoId = repo.id.parse().unwrap();
        let (board, _watch) = watch::channel(Arc::new(Board::default()));
        let mut task = Task::new(
            Arc::clone(daemon.handle.state()),
            board,
            Arc::new(AtomicU64::new(0)),
        );
        task.reconcile(true);
        let mut reading = Reads::new();
        task.start(&mut reading);
        assert!(task.reading.contains(&repo));
        task.dirty_all();
        let read = reading.next().await.unwrap();
        task.finish(read);
        assert!(task.dirty.contains(&repo), "read again after the gap");
    }

    // #400 review P2: a read the ledger did not settle is read again on the
    // same backoff a failed read uses, so the write lands once it can, and a
    // settled read ends the retries.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_the_ledger_did_not_settle_is_read_again_and_backs_off() {
        let (daemon, repo) =
            TestDaemon::with_synthetic_repo(reef_testing::synth::Preset::Small).await;
        let repo: RepoId = repo.id.parse().unwrap();
        let (board, _watch) = watch::channel(Arc::new(Board::default()));
        let mut task = Task::new(
            Arc::clone(daemon.handle.state()),
            board,
            Arc::new(AtomicU64::new(0)),
        );
        let unsettled = || {
            let mut board = RepoBoard::failed(None, v1::InboxError::default());
            board.error = None;
            board.read = true;
            board
        };
        let finished = |result: RepoBoard| Read {
            repo,
            last: None,
            result: Ok(result),
            started: Instant::now(),
            worked: Duration::ZERO,
        };
        // Each failure doubles the wait the next read is armed with.
        for wait in [2, 4, 8, 16, 32, 60, 60] {
            task.finish(finished(unsettled()));
            assert_eq!(
                task.retry.get(&repo).map(|(_, wait)| *wait),
                Some(Duration::from_secs(wait)),
                "the next retry"
            );
        }
        let mut settled = unsettled();
        settled.settled = true;
        task.finish(finished(settled));
        assert!(!task.retry.contains_key(&repo), "a settled read ends them");
    }

    #[test]
    fn reads_take_at_most_a_quarter_of_the_time() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut one = Pacing::new(t0);
        one.started(t0);
        one.finished(t0, ms(100));
        assert_eq!(one.next(), t0 + ms(400));

        let mut cheap = Pacing::new(t0);
        cheap.started(t0);
        cheap.finished(t0, ms(10));
        assert_eq!(cheap.next(), t0 + MIN_SPACING);

        // Reads that run at once are paid for one after another.
        let mut batch = Pacing::new(t0);
        batch.started(t0);
        for _ in 0..3 {
            batch.finished(t0, ms(100));
        }
        assert_eq!(batch.next(), t0 + ms(1_200));
        // A later read is paid for from its own start once the batch is.
        batch.started(t0 + ms(1_200));
        batch.finished(t0 + ms(2_000), ms(50));
        assert_eq!(batch.next(), t0 + ms(2_200));
    }
}
