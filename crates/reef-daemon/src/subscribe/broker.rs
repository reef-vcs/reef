//! The subscription broker (R2.22, ADR 0038): each followed repository's
//! new operations read from the log once, converted once, and held in a
//! bounded ring that every session following the repository reads from.
//!
//! - `Delivered` is one operation as the ring holds it: what a frame
//!   sends and what a filter reads.
//! - The ingest is one task per followed repository, run by a supervisor:
//!   it reads what follows its ring's head when [`super::OpEvents`] says
//!   the log may have moved, and restarts the ring at the op heads after a
//!   rewrite or when it cannot go on from its head.
//! - `BrokerSource` is a session's `Source`: a ring hit when the
//!   position is in the ring, else R2.09's log read (`LogSource`), so a
//!   frame's operations are the same either way.
//!
//! The rings themselves are `reef_broker`'s, which knows nothing of jj or
//! gRPC; the store reuses it with its own producer and reader.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use reef_api::v1;
use reef_broker::{Broker, Item, Lookup};
use reef_lib::error::ErrorChain;
use reef_lib::jj::effects::OpEffects;
use reef_lib::jj::oplog::{self, Frontier, OpClass, OpEntry, OpLookupError};
use reef_lib::jj::{OperationIdHex, ReadonlyRepo};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::Status;

use super::events::{self, RepoView};
use super::feed::{self, Batch, Budget, LogSource, ReadFailure};
use super::session::Source;
use crate::daemon::AppState;
use crate::error::status;
use crate::model::RepoId;
use crate::service::op_filter::OpMatcher;
use crate::service::reading::Records;

/// How often the supervisor drops rings nobody follows, at most; sooner
/// when the linger is shorter.
const SWEEP_EVERY: Duration = Duration::from_secs(10);
/// How often the supervisor logs its summary line.
const SUMMARY_EVERY: Duration = Duration::from_mins(1);
/// How long an ingest waits after a failed read before the next.
const RETRY_AFTER: Duration = Duration::from_secs(1);

/// One operation, converted once: what a frame sends and what a filter
/// reads.
#[derive(Debug)]
pub(crate) struct Delivered {
    /// For the filter and the frontier.
    pub(crate) entry: OpEntry,
    /// `oplog::classify(&entry)`, once.
    pub(crate) class: OpClass,
    /// The owner of its workspace, as the daemon's records said when it
    /// was read.
    pub(crate) owner: Option<String>,
    /// For a filter on changes.
    pub(crate) effects: Option<OpEffects>,
    /// What a frame sends, effects included.
    pub(crate) message: v1::Operation,
    /// `message`'s encoded length.
    pub(crate) encoded: usize,
}

impl Item for Delivered {
    fn weight(&self) -> usize {
        // The message, the effects it was converted from, and the entry.
        2 * self.encoded + 256
    }
}

/// The rings, by repository, between operation-log frontiers.
pub(crate) type Rings = Broker<RepoId, Frontier, Delivered>;

/// The daemon's subscription broker; one in [`crate::AppState::broker`].
#[derive(Debug)]
pub struct OpBroker {
    pub(crate) rings: Arc<Rings>,
    /// False when [`super::Limits::ring_operations`] is 0: sessions read
    /// the log, as before R2.22.
    enabled: bool,
    counters: Counters,
}

#[derive(Debug, Default)]
struct Counters {
    /// Operations the ingest appended.
    ingested: AtomicU64,
    /// Ingest reads of the log.
    ingest_reads: AtomicU64,
    /// Each ingest read's time, since the last summary.
    ingest_ms: Mutex<Vec<u64>>,
    /// Session reads that went to the log.
    log_reads: AtomicU64,
    /// Changes of the repositories' set or rewrites forwarded to sessions.
    wakes: AtomicU64,
}

/// The broker's counters and sizes, for tests and the summary line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BrokerStats {
    /// Rings, primed or not.
    pub topics: usize,
    /// Operations held in every ring.
    pub items: usize,
    /// Their weight in bytes.
    pub bytes: usize,
    /// Operations the ingest read from the log and appended.
    pub ingested: u64,
    /// Reads of the log by the ingest.
    pub ingest_reads: u64,
    /// Session reads served from a ring.
    pub ring_reads: u64,
    /// Session reads that went to the log: behind a ring, or no ring.
    pub log_reads: u64,
    /// Rings restarted: rewrites, and an ingest that could not go on.
    pub resets: u64,
    /// Operations evicted by the bounds.
    pub evicted: u64,
    /// Wake-ups forwarded to sessions for repositories coming and going
    /// and rewrites.
    pub wakes: u64,
}

impl OpBroker {
    /// A broker with `limits`' ring bounds; disabled when
    /// `ring_operations` is 0.
    pub fn new(limits: &super::Limits) -> OpBroker {
        OpBroker {
            rings: Broker::new(reef_broker::Limits {
                items: limits.ring_operations.min(limits.max_behind).max(1),
                bytes: limits.ring_bytes,
                total_bytes: limits.ring_total_bytes,
                linger: limits.ring_linger,
            }),
            enabled: limits.ring_operations > 0,
            counters: Counters::default(),
        }
    }

    /// Whether sessions are served from rings.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// `repo`'s ring's operations and bytes, if it has one.
    pub fn size_of(&self, repo: RepoId) -> Option<(usize, usize)> {
        self.rings.size_of(&repo)
    }

    /// The counters and sizes now.
    pub fn stats(&self) -> BrokerStats {
        let rings = self.rings.stats();
        BrokerStats {
            topics: rings.topics,
            items: rings.items,
            bytes: rings.bytes,
            ingested: self.counters.ingested.load(Ordering::Relaxed),
            ingest_reads: self.counters.ingest_reads.load(Ordering::Relaxed),
            ring_reads: rings.hits,
            log_reads: self.counters.log_reads.load(Ordering::Relaxed),
            resets: rings.restarts,
            evicted: rings.evicted,
            wakes: self.counters.wakes.load(Ordering::Relaxed),
        }
    }
}

/// Starts the supervisor, which runs one ingest per followed repository
/// until `state.shutdown`. Does nothing when the broker is disabled.
pub(crate) fn supervise(state: &Arc<AppState>) {
    if !state.broker.enabled {
        return;
    }
    tokio::spawn(Supervisor::new(Arc::clone(state)).run());
}

struct Supervisor {
    state: Arc<AppState>,
    tasks: HashMap<RepoId, (CancellationToken, JoinHandle<()>)>,
    /// Ingests told to stop that may still be finishing a read. A
    /// repository gets no new ingest until its old one has ended, so two
    /// never write one ring (R2.22 review).
    stopping: Vec<(RepoId, JoinHandle<()>)>,
    /// Notified when an ingest ends.
    ended: Arc<tokio::sync::Notify>,
    /// What sessions read from `OpEvents` besides versions, as last
    /// forwarded.
    shape: Vec<events::Shape>,
    /// The counters at the last summary.
    last: BrokerStats,
}

impl Supervisor {
    fn new(state: Arc<AppState>) -> Supervisor {
        let shape = state.events.shapes();
        Supervisor {
            state,
            tasks: HashMap::new(),
            stopping: Vec::new(),
            ended: Arc::new(tokio::sync::Notify::new()),
            shape,
            last: BrokerStats::default(),
        }
    }

    async fn run(mut self) {
        let rings = Arc::clone(&self.state.broker.rings);
        let shutdown = self.state.shutdown.clone();
        let mut events = self.state.events.changes();
        let mut wanted = rings.wanted_changes();
        let ended = Arc::clone(&self.ended);
        let linger = rings.limits().linger;
        let mut sweep =
            tokio::time::interval((linger / 2).clamp(Duration::from_millis(100), SWEEP_EVERY));
        let mut summary =
            tokio::time::interval_at(tokio::time::Instant::now() + SUMMARY_EVERY, SUMMARY_EVERY);
        loop {
            wanted.borrow_and_update();
            self.reconcile();
            tokio::select! {
                () = shutdown.cancelled() => break,
                changed = events.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    self.forward();
                }
                changed = wanted.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                () = ended.notified() => {}
                _ = sweep.tick() => rings.sweep(tokio::time::Instant::now()),
                _ = summary.tick() => self.summary(),
            }
        }
        for (_, (stop, task)) in self.tasks.drain() {
            stop.cancel();
            let _ = task.await;
        }
        for (_, task) in self.stopping.drain(..) {
            let _ = task.await;
        }
    }

    /// One ingest per wanted repository; none for the rest.
    fn reconcile(&mut self) {
        let wanted: HashSet<RepoId> = self.state.broker.rings.wanted().into_iter().collect();
        let done: Vec<RepoId> = self
            .tasks
            .iter()
            .filter(|(repo, (_, task))| !wanted.contains(*repo) || task.is_finished())
            .map(|(repo, _)| *repo)
            .collect();
        for repo in done {
            if let Some((stop, task)) = self.tasks.remove(&repo) {
                stop.cancel();
                self.stopping.push((repo, task));
            }
        }
        self.stopping.retain(|(_, task)| !task.is_finished());
        for repo in wanted {
            if self.tasks.contains_key(&repo) || self.stopping.iter().any(|(old, _)| *old == repo) {
                continue;
            }
            let stop = self.state.shutdown.child_token();
            let (state, ended, token) = (
                Arc::clone(&self.state),
                Arc::clone(&self.ended),
                stop.clone(),
            );
            let task = tokio::spawn(async move {
                ingest(state, repo, token).await;
                ended.notify_one();
            });
            self.tasks.insert(repo, (stop, task));
        }
    }

    /// Wakes sessions when a repository came or went or a rewrite moved:
    /// what they read from `OpEvents` rather than from a ring. New
    /// operations wake them through the rings instead.
    fn forward(&mut self) {
        let shape = self.state.events.shapes();
        if shape != self.shape {
            self.shape = shape;
            self.state.broker.rings.notify();
            self.state
                .broker
                .counters
                .wakes
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    fn summary(&mut self) {
        let now = self.state.broker.stats();
        if now.topics == 0 && now == self.last {
            return;
        }
        let mut times = std::mem::take(
            &mut *self
                .state
                .broker
                .counters
                .ingest_ms
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        times.sort_unstable();
        let p50 = times.get(times.len() / 2).copied().unwrap_or(0);
        let max = times.last().copied().unwrap_or(0);
        let last = self.last;
        // R2.22's load test parses this line from the log; keep its shape.
        tracing::info!(
            "subscription broker summary: topics={} items={} bytes={} ingested={} ingest_reads={} \
             ingest_p50_ms={p50} ingest_max_ms={max} ring_reads={} log_reads={} resets={} \
             evicted={} wakes={}",
            now.topics,
            now.items,
            now.bytes,
            now.ingested - last.ingested,
            now.ingest_reads - last.ingest_reads,
            now.ring_reads - last.ring_reads,
            now.log_reads - last.log_reads,
            now.resets - last.resets,
            now.evicted - last.evicted,
            now.wakes - last.wakes,
        );
        self.last = now;
    }
}

/// What one ingest read found.
enum Ingested {
    /// The ring was not primed: the op heads to start it at.
    Prime(Vec<OperationIdHex>),
    /// The operations after the ring's head, each with the frontier after
    /// it.
    Items(Vec<(Arc<Delivered>, Frontier)>),
    /// The ring cannot go on from its head: start again at these op heads.
    Restart(Vec<OperationIdHex>, &'static str),
}

/// Keeps `repo`'s ring at the log's head until `stop`, or until the
/// repository is forgotten.
async fn ingest(state: Arc<AppState>, repo: RepoId, stop: CancellationToken) {
    let rings = Arc::clone(&state.broker.rings);
    let mut changes = state.events.changes();
    // The version the ring was last read at, and the rewrites it has
    // been moved across.
    let mut read_at: Option<u64> = None;
    let mut rewrites: Option<u64> = None;
    loop {
        changes.borrow_and_update();
        let Some(view) = state.events.view(repo) else {
            rings.remove(&repo);
            return;
        };
        let mut retry = false;
        if view.rewrite_seq.is_multiple_of(2) {
            if rewrites.is_some_and(|seen| seen != view.rewrites) {
                // A caught-up session moves to the rewritten head and finds
                // it here; one behind reads up to the head the rewrite
                // replaced from the log, as R2.09 does.
                match &view.last_rewrite {
                    Some((_, after)) => rings.start(&repo, Frontier::new([after.clone()])),
                    // Primed again from the op heads below, keeping its
                    // followers.
                    None => rings.unprime(&repo),
                }
                read_at = None;
            }
            rewrites = Some(view.rewrites);
            if read_at != Some(view.version) || rings.head(&repo).is_none() {
                match step(&state, repo, &view).await {
                    Step::Done => read_at = Some(view.version),
                    Step::Stale => {}
                    Step::Failed => {
                        // Sessions must not wait on a ring that no longer
                        // moves: unprimed, they read the log, as before
                        // R2.22, and meet its error if it persists. The
                        // next read primes it again from the op heads.
                        rings.unprime(&repo);
                        retry = true;
                    }
                }
            }
        }
        let wait = async {
            if retry {
                tokio::time::sleep(RETRY_AFTER).await;
                Ok(())
            } else {
                changes.changed().await
            }
        };
        tokio::select! {
            () = stop.cancelled() => return,
            woken = wait => {
                if woken.is_err() {
                    return;
                }
            }
        }
    }
}

/// How one ingest step ended.
enum Step {
    /// The ring is at the log as of the view.
    Done,
    /// A rewrite overlapped the read; nothing was kept.
    Stale,
    /// The read failed; try again.
    Failed,
}

async fn step(state: &Arc<AppState>, repo: RepoId, view: &RepoView) -> Step {
    let rings = &state.broker.rings;
    let (Some(tip), Some(handle)) = (state.events.tip(repo), state.repos.get(repo)) else {
        return Step::Stale;
    };
    let head = rings.head(&repo).map(|(_, at)| at);
    let limit = rings.limits().items;
    let state2 = Arc::clone(state);
    let begun = std::time::Instant::now();
    let read = state
        .reads
        .run_unbounded(&handle, move || async move {
            ingest_read(&state2, repo, &tip, head, limit).await
        })
        .await;
    let took = u64::try_from(begun.elapsed().as_millis()).unwrap_or(u64::MAX);
    // A rewrite that began while reading makes the read stale.
    if state
        .events
        .view(repo)
        .is_none_or(|now| now.rewrite_seq != view.rewrite_seq || now.rewrites != view.rewrites)
    {
        return Step::Stale;
    }
    let counters = &state.broker.counters;
    counters.ingest_reads.fetch_add(1, Ordering::Relaxed);
    counters
        .ingest_ms
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(took);
    match read {
        Ok(Ok(Ingested::Prime(heads))) => rings.start(&repo, Frontier::new(heads)),
        Ok(Ok(Ingested::Items(items))) => {
            counters
                .ingested
                .fetch_add(items.len() as u64, Ordering::Relaxed);
            rings.append(&repo, items);
        }
        Ok(Ok(Ingested::Restart(heads, why))) => {
            tracing::warn!(repo = %repo, reason = why, "subscription ring restarted at the op heads");
            rings.start(&repo, Frontier::new(heads));
        }
        Ok(Err(error)) => {
            tracing::warn!(repo = %repo, error = %error.message(), "subscription ring read failed");
            return Step::Failed;
        }
        Err(error) => {
            tracing::warn!(repo = %repo, error = %ErrorChain(&error), "subscription ring read failed");
            return Step::Failed;
        }
    }
    Step::Done
}

/// The operations after `from` (the ring's head) up to the op heads, at
/// most `limit`, each converted; or the op heads to (re)start at.
async fn ingest_read(
    state: &AppState,
    repo: RepoId,
    tip: &Arc<ReadonlyRepo>,
    from: Option<Frontier>,
    limit: usize,
) -> Result<Ingested, Status> {
    let heads = oplog::op_heads(tip).await.map_err(status)?;
    let Some(from) = from else {
        return Ok(Ingested::Prime(heads));
    };
    let range = match oplog::walk_after(tip, &heads, &from, limit).await {
        Ok(range) => range,
        Err(OpLookupError::NotFound(_)) => {
            return Ok(Ingested::Restart(
                heads,
                "the ring's head is gone from the log",
            ));
        }
        Err(error) => return Err(status(error)),
    };
    if range.truncated {
        return Ok(Ingested::Restart(
            heads,
            "more operations than the ring holds were recorded at once",
        ));
    }
    if !range.unreached.is_empty() {
        return Ok(Ingested::Restart(heads, "the operation log was rewritten"));
    }
    let entries: Vec<OpEntry> = range.operations.iter().map(oplog::entry).collect();
    let records = Records::of(state, &entries)?;
    let mut frontier = from;
    let mut items = Vec::with_capacity(entries.len());
    for (operation, entry) in range.operations.iter().zip(entries) {
        frontier.advance(&entry);
        let delivered = feed::deliver(state, repo, tip, &records, operation, entry).await;
        items.push((Arc::new(delivered), frontier.clone()));
    }
    Ok(Ingested::Items(items))
}

/// A session's [`Source`] over the rings, falling back to the log.
#[derive(Debug)]
pub(crate) struct BrokerSource {
    log: LogSource,
}

impl BrokerSource {
    pub(crate) fn new(state: Arc<AppState>, matcher: Arc<OpMatcher>) -> BrokerSource {
        BrokerSource {
            log: LogSource { state, matcher },
        }
    }

    fn rings(&self) -> &Rings {
        &self.log.state.broker.rings
    }

    /// A ring hit as a batch: the entries the filter keeps, within the
    /// budget's bytes, as `feed::read` would cut them.
    fn batch(
        &self,
        after: &Frontier,
        entries: Vec<Arc<reef_broker::Entry<Frontier, Delivered>>>,
        mut more: bool,
        budget: Budget,
    ) -> Batch {
        let matcher = &self.log.matcher;
        let mut batch = Batch {
            operations: Vec::new(),
            frontier: after.clone(),
            more: false,
        };
        let mut bytes = 0;
        for entry in entries {
            let delivered = &entry.item;
            let kept = matcher.keeps_classified(&delivered.entry, &delivered.class, |_| {
                delivered.owner.as_deref()
            }) && (!matcher.names_changes()
                || matcher.touches(delivered.effects.as_ref()));
            if kept {
                let first = batch.operations.is_empty() && budget.first_free;
                if !first && bytes + delivered.encoded > budget.bytes {
                    more = true;
                    break;
                }
                bytes += delivered.encoded;
                batch.operations.push(delivered.message.clone());
            }
            batch.frontier.clone_from(&entry.after);
        }
        batch.more = more;
        batch
    }
}

impl Source for BrokerSource {
    type Follow = (
        events::Follow,
        reef_broker::Following<RepoId, Frontier, Delivered>,
    );

    fn repos(&self) -> Vec<RepoId> {
        self.log.repos()
    }

    /// `OpEvents`' view with the ring's version: a session is due when its
    /// ring changed, and still sees rewrites and repositories coming and
    /// going from `OpEvents`.
    fn view(&self, repo: RepoId) -> Option<RepoView> {
        let mut view = self.log.view(repo)?;
        view.version = self.rings().version(&repo).unwrap_or(0);
        Some(view)
    }

    fn changes(&self) -> watch::Receiver<u64> {
        self.rings().changes()
    }

    fn follow(&self, repo: RepoId) -> Self::Follow {
        (
            self.log.follow(repo),
            self.log.state.broker.rings.follow(repo),
        )
    }

    async fn read(
        &self,
        repo: RepoId,
        after: &Frontier,
        upto: Option<&[OperationIdHex]>,
        budget: Budget,
    ) -> Result<Batch, ReadFailure> {
        let upto_frontier = upto.map(|heads| Frontier::new(heads.iter().cloned()));
        match self
            .rings()
            .read(&repo, after, upto_frontier.as_ref(), budget.operations)
        {
            Lookup::Hit { entries, more } => Ok(self.batch(after, entries, more, budget)),
            Lookup::Miss | Lookup::Unknown => {
                self.log
                    .state
                    .broker
                    .counters
                    .log_reads
                    .fetch_add(1, Ordering::Relaxed);
                self.log.read(repo, after, upto, budget).await
            }
        }
    }
}
