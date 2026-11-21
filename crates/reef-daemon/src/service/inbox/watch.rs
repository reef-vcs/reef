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
//! to state. Reads are paced by R2.19's rule: one pass at a time, starting
//! no sooner than `max(250 ms, 4 × the last pass)` after the last began.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

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
use crate::subscribe::{Slot, SubscribeError};

/// The least time between the starts of two read passes.
const MIN_SPACING: Duration = Duration::from_millis(250);
/// A pass starts no sooner than this many times the last one's length
/// after it began, so reading takes at most a quarter of the task's time.
const SPACING_FACTOR: u32 = 4;
/// The first retry of a repository that failed to read, doubled up to
/// [`MAX_RETRY`].
const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_mins(1);

/// What every stream reads: each registered repository's snapshot.
#[derive(Debug, Default)]
pub(crate) struct Board {
    pub(crate) repos: BTreeMap<RepoId, Arc<RepoBoard>>,
    /// Every repository registered when the task started has been read.
    pub(crate) ready: bool,
}

/// The board's owner; one in [`AppState::inbox`].
#[derive(Debug)]
pub struct InboxWatch {
    shared: Arc<Mutex<Weak<Shared>>>,
    marks: watch::Sender<u64>,
    passes: Arc<AtomicU64>,
}

impl Default for InboxWatch {
    fn default() -> InboxWatch {
        InboxWatch {
            shared: Arc::new(Mutex::new(Weak::new())),
            marks: watch::Sender::new(0),
            passes: Arc::new(AtomicU64::new(0)),
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
    /// A watcher of the board, starting the task if none runs. The task
    /// takes a subscription slot of its own for its operation stream,
    /// refused here (`TOO_MANY_SUBSCRIPTIONS`) rather than retried later.
    pub(crate) fn watch(&self, state: &Arc<AppState>) -> Result<Watch, SubscribeError> {
        let mut held = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        let shared = if let Some(shared) = held.upgrade() {
            shared
        } else {
            let slot = state
                .subscriptions
                .admit(state.config.subscribe.max_subscriptions)?;
            let (tx, rx) = watch::channel(Arc::new(Board::default()));
            let stop = state.shutdown.child_token();
            let shared = Arc::new(Shared {
                board: rx,
                _stop: stop.clone().drop_guard(),
            });
            let mine = Arc::downgrade(&shared);
            *held = mine.clone();
            let (state, passes, owner) = (
                Arc::clone(state),
                Arc::clone(&self.passes),
                Arc::clone(&self.shared),
            );
            tokio::spawn(async move {
                let ran = AssertUnwindSafe(run(state, tx, stop, passes, slot))
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
        Ok(Watch {
            board: shared.board.clone(),
            marks: self.marks.subscribe(),
            _shared: shared,
        })
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

    /// Read passes the task has run since the daemon started.
    pub fn passes(&self) -> u64 {
        self.passes.load(Ordering::Relaxed)
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
    last_start: Option<Instant>,
    last_length: Duration,
    rev: u64,
    passes: Arc<AtomicU64>,
}

async fn run(
    state: Arc<AppState>,
    board: watch::Sender<Arc<Board>>,
    stop: CancellationToken,
    passes: Arc<AtomicU64>,
    slot: Slot,
) {
    // The triggers before the first read, so nothing between is missed.
    let filter = PresenceFilter {
        kinds: Some(refresh_presence().into_iter().collect()),
        ..PresenceFilter::default()
    };
    let (_, mut presence) = state.presence.subscribe(&filter);
    let mut operations = subscribe(&state, Some(slot)).await;
    let mut events = state.events.changes();
    let mut task = Task {
        open: state.events.repos().into_iter().collect(),
        listed: false,
        state,
        board,
        dirty: BTreeSet::new(),
        retry: HashMap::new(),
        last_start: None,
        last_length: Duration::ZERO,
        rev: 0,
        passes,
    };
    task.reconcile(true);
    // A refused subscription is retried like one that ended.
    let mut reconnect: Option<Instant> = operations.is_none().then(|| Instant::now() + FIRST_RETRY);
    let mut backoff = FIRST_RETRY;
    loop {
        if task.due(Instant::now()) {
            task.pass().await;
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
                    operations = subscribe(&task.state, None).await;
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
/// repository, those registered later included, in `slot` or a new one;
/// `None` when the subscription fails (it is retried).
async fn subscribe(state: &Arc<AppState>, slot: Option<Slot>) -> Option<Frames> {
    let handler = crate::service::operations::OperationsHandler::new(Arc::clone(state));
    let request = SubscribeOperationsRequest {
        filter: Some(v1::OperationFilter {
            kinds: refresh_operations(),
            ..v1::OperationFilter::default()
        }),
        ..SubscribeOperationsRequest::default()
    };
    // The first subscription takes the slot admitted with the board's first
    // watcher. One that follows a stream's end is the same one place back,
    // taken past the cap, so clients holding every slot cannot leave the
    // board without its triggers.
    let slot = match slot {
        Some(slot) => slot,
        None => match state.subscriptions.admit(usize::MAX) {
            Ok(slot) => slot,
            Err(error) => {
                tracing::warn!(%error, "the inbox could not follow the operation log");
                return None;
            }
        },
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
    fn dirty_all(&mut self) {
        let repos: Vec<RepoId> = self.board.borrow().repos.keys().copied().collect();
        self.dirty.extend(repos);
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
        // With nothing to read, the board is complete as it stands.
        let ready = board.ready || self.dirty.is_empty();
        if !gone.is_empty() || ready != board.ready {
            let mut repos = board.repos.clone();
            for repo in &gone {
                repos.remove(repo);
            }
            self.board.send_replace(Arc::new(Board { repos, ready }));
        }
    }

    /// When the next pass may start, as of `now`, if anything waits for
    /// one.
    fn next_due(&self, now: Instant) -> Option<Instant> {
        let wall = Timestamp::now();
        let restale = self
            .board
            .borrow()
            .repos
            .values()
            .filter_map(|repo| repo.restale_at)
            .min()
            .map(|at| {
                now + Duration::from_millis(at.0.saturating_sub(wall.0).max(0).unsigned_abs())
            });
        let retry = self.retry.values().map(|(at, _)| *at).min();
        let wanted = if self.dirty.is_empty() {
            [restale, retry].into_iter().flatten().min()?
        } else {
            now
        };
        let spaced = self
            .last_start
            .map(|at| at + (self.last_length * SPACING_FACTOR).max(MIN_SPACING));
        Some(spaced.map_or(wanted, |spaced| spaced.max(wanted)))
    }

    fn due(&self, now: Instant) -> bool {
        self.next_due(now).is_some_and(|at| at <= now)
    }

    /// Reads every repository due, and publishes what a stream could see.
    async fn pass(&mut self) {
        let start = Instant::now();
        let now = Timestamp::now();
        let mut due: BTreeSet<RepoId> = std::mem::take(&mut self.dirty);
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
        if due.is_empty() {
            // A deadline a moment early by the wall clock: wait the spacing.
            self.last_start = Some(start);
            return;
        }
        let scope = match due.iter().collect::<Vec<_>>().as_slice() {
            [one] => one.as_hex(),
            _ => String::new(),
        };
        let mut attention = super::attention(&self.state, &scope).await;
        let mut read = Vec::with_capacity(due.len());
        for repo in due {
            let workspaces = match &mut attention {
                Ok(by_repo) => Ok(by_repo.remove(&repo.as_hex()).unwrap_or_default()),
                Err(error) => Err(error.clone()),
            };
            let last = self.board.borrow().repos.get(&repo).cloned();
            let result = match workspaces {
                Ok(workspaces) => self.read(repo, workspaces, last.as_deref()).await,
                Err(error) => Err(error),
            };
            let board = match result {
                Ok(board) => {
                    self.retry.remove(&repo);
                    board
                }
                Err(error) => {
                    let wait = self.retry.get(&repo).map_or(FIRST_RETRY, |(_, wait)| *wait);
                    self.retry
                        .insert(repo, (Instant::now() + wait, (wait * 2).min(MAX_RETRY)));
                    RepoBoard::failed(last.as_deref(), super::inbox_error(&error))
                }
            };
            read.push((repo, last, board));
        }
        self.passes.fetch_add(1, Ordering::Relaxed);
        // A repository forgotten during the pass leaves with `reconcile`.
        let registered = super::registered(&self.state).ok();
        if let Some(registered) = &registered {
            self.retry.retain(|repo, _| registered.contains(repo));
        }
        let changed: Vec<(RepoId, RepoBoard)> = read
            .into_iter()
            .filter(|(repo, last, board)| {
                registered
                    .as_ref()
                    .is_none_or(|registered| registered.contains(repo))
                    && last.as_deref().is_none_or(|last| !last.same(board))
            })
            .map(|(repo, _, board)| (repo, board))
            .collect();
        let ready = !self.board.borrow().ready;
        if !changed.is_empty() || ready {
            self.board.send_modify(|board| {
                let mut next = Board {
                    repos: board.repos.clone(),
                    ready: true,
                };
                for (repo, mut repo_board) in changed {
                    self.rev += 1;
                    repo_board.rev = self.rev;
                    next.repos.insert(repo, Arc::new(repo_board));
                }
                *board = Arc::new(next);
            });
        }
        self.last_start = Some(start);
        self.last_length = start.elapsed();
    }

    /// One repository's snapshot, its subjects reused from `last` when its
    /// newest landings did not move.
    async fn read(
        &self,
        repo: RepoId,
        workspaces: Vec<v1::Workspace>,
        last: Option<&RepoBoard>,
    ) -> Result<RepoBoard, tonic::Status> {
        let buckets = super::buckets(&self.state, repo, workspaces).await?;
        let newest = super::newest_landed(&self.state, repo)?;
        let subjects = match last {
            Some(last)
                if last.subjects.len() == newest.len()
                    && newest
                        .iter()
                        .all(|row| last.subjects.contains_key(&row.change_id)) =>
            {
                Arc::clone(&last.subjects)
            }
            _ => Arc::new(super::read_subjects(&self.state, &repo.as_hex(), &newest).await?),
        };
        Ok(RepoBoard::read(repo, buckets, subjects))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spacing_is_a_quarter_of_the_time_at_most() {
        let length = Duration::from_millis(100);
        assert_eq!(
            (length * SPACING_FACTOR).max(MIN_SPACING),
            Duration::from_millis(400)
        );
        let short = Duration::from_millis(10);
        assert_eq!((short * SPACING_FACTOR).max(MIN_SPACING), MIN_SPACING);
    }
}
