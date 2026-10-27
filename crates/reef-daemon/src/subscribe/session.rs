//! The task behind each `SubscribeOperations` stream.
//!
//! It feeds a channel of one that tonic drains, and reserves the channel's
//! slot before it reads the log for the next frame: it reads only when the
//! transport has taken the previous frame, so a slow client costs one
//! built frame and its position, never a queue. Between frames it waits
//! for [`super::OpEvents`] to report a repository it follows changed, or
//! for the keepalive. It ends when the client goes away, the daemon shuts
//! down, or the subscription cannot continue.

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use futures::FutureExt as _;
use reef_api::v1::{self, SubscribeOperationsResponse};
use reef_lib::jj::OperationIdHex;
use reef_lib::jj::oplog::Frontier;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tonic::Status;

use super::events::RepoView;
use super::feed::{Batch, Budget, ReadFailure};
use super::{Limits, Slot, SubscribeError, cursor};
use crate::error::status;
use crate::model::RepoId;

/// One frame, or the error that ends the stream.
pub(crate) type Frame = Result<SubscribeOperationsResponse, Status>;

/// Where a session's operations come from: [`super::feed`] over the
/// daemon, or a fake in tests.
pub(crate) trait Source: Send + Sync + 'static {
    /// Held while a repository is followed.
    type Follow: Send;

    /// The open repositories.
    fn repos(&self) -> Vec<RepoId>;
    /// What is known of `repo`, if it is open.
    fn view(&self, repo: RepoId) -> Option<RepoView>;
    /// A receiver that sees every change.
    fn changes(&self) -> watch::Receiver<u64>;
    /// Counts this session as following `repo`.
    fn follow(&self, repo: RepoId) -> Self::Follow;
    /// The operations after `after` in `repo`, up to `upto` or the current
    /// op heads, within `budget`.
    fn read(
        &self,
        repo: RepoId,
        after: &Frontier,
        upto: Option<&[OperationIdHex]>,
        budget: Budget,
    ) -> impl Future<Output = Result<Batch, ReadFailure>> + Send;
}

/// Where one followed repository stands.
#[derive(Debug, Clone)]
pub(crate) struct Position {
    /// Everything up to here was sent or filtered out.
    pub(crate) frontier: Frontier,
    /// The op heads a replay runs to; `None` once replayed, or without a
    /// start.
    pub(crate) upto: Option<Vec<OperationIdHex>>,
    /// The view the position was read at; `None` to read it at once.
    pub(crate) view: Option<RepoView>,
}

/// How a session starts.
#[derive(Debug)]
pub(crate) struct Start {
    /// The repositories the request named; `None` for every repository,
    /// including ones opened later.
    pub(crate) named: Option<Vec<RepoId>>,
    /// Each followed repository's position.
    pub(crate) positions: BTreeMap<RepoId, Position>,
}

struct Followed<F> {
    position: Position,
    /// The budget cut the last read short.
    more: bool,
    /// Rewrites of the log (GC) this position has been moved across.
    rewrites: u64,
    /// A rewrite this position was behind: read up to the head it rewrote
    /// (its old operations outlive the rewrite by the undo window), then
    /// move to the rewritten head.
    rebase: Option<(OperationIdHex, OperationIdHex)>,
    _follow: F,
}

/// Starts the session's task; its frames arrive on the receiver.
pub(crate) fn spawn<S: Source>(
    source: S,
    limits: Limits,
    shutdown: CancellationToken,
    slot: Slot,
    start: Start,
) -> mpsc::Receiver<Frame> {
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        let _slot = slot;
        let followed = start
            .positions
            .into_iter()
            .map(|(repo, position)| {
                let follow = source.follow(repo);
                let rewrites = position.view.as_ref().map_or(0, |view| view.rewrites);
                (
                    repo,
                    Followed {
                        position,
                        more: false,
                        rewrites,
                        rebase: None,
                        _follow: follow,
                    },
                )
            })
            .collect();
        let single = start.named.as_ref().is_some_and(|named| named.len() == 1);
        let mut session = Session {
            source,
            limits,
            shutdown,
            named: start.named,
            single,
            followed,
        };
        let served = AssertUnwindSafe(session.serve(&tx)).catch_unwind().await;
        let error = match served {
            Ok(Ok(())) => return,
            Ok(Err(error)) => error,
            Err(_) => {
                tracing::error!("subscription task panicked");
                Status::internal("the subscription failed")
            }
        };
        // The client may not be reading; the stream ends either way.
        let _ = tokio::time::timeout(Duration::from_secs(1), tx.send(Err(error))).await;
    });
    rx
}

struct Session<S: Source> {
    source: S,
    limits: Limits,
    shutdown: CancellationToken,
    named: Option<Vec<RepoId>>,
    single: bool,
    followed: BTreeMap<RepoId, Followed<S::Follow>>,
}

impl<S: Source> Session<S> {
    /// Sends frames until the client goes away (`Ok`) or the stream must
    /// end with an error.
    async fn serve(&mut self, tx: &mpsc::Sender<Frame>) -> Result<(), Status> {
        let mut changes = self.source.changes();
        // The first live frame goes out as soon as there is no replay
        // left, empty if nothing is new.
        let mut pending_live = true;
        let mut last_frame = Instant::now();
        let mut last_live_operations: Option<Instant> = None;
        loop {
            changes.borrow_and_update();
            self.follow_repositories()?;
            self.apply_rewrites()?;
            let replaying = self.replaying();
            let due = self.due();
            let first_live = pending_live && !replaying;
            let mut keepalive = false;
            if due.is_empty() && !first_live {
                tokio::select! {
                    biased;
                    () = self.shutdown.cancelled() => return Err(status(SubscribeError::ShuttingDown)),
                    () = tx.closed() => return Ok(()),
                    woken = changes.changed() => {
                        if woken.is_err() {
                            return Err(status(SubscribeError::ShuttingDown));
                        }
                        continue;
                    }
                    () = tokio::time::sleep_until(last_frame + self.limits.keepalive) => {
                        keepalive = true;
                    }
                }
            }
            // Live frames carrying operations are spaced; everything
            // recorded meanwhile goes in the next one. The rest of a frame
            // a bound cut goes at once.
            let cut = self.followed.values().any(|followed| followed.more);
            if !keepalive
                && !cut
                && !replaying
                && !pending_live
                && let Some(at) = last_live_operations
            {
                let next = at + self.limits.frame_interval;
                tokio::select! {
                    biased;
                    () = self.shutdown.cancelled() => return Err(status(SubscribeError::ShuttingDown)),
                    () = tx.closed() => return Ok(()),
                    () = tokio::time::sleep_until(next) => {}
                }
            }
            let permit = tokio::select! {
                biased;
                () = self.shutdown.cancelled() => return Err(status(SubscribeError::ShuttingDown)),
                permit = tx.reserve() => match permit {
                    Ok(permit) => permit,
                    Err(_) => return Ok(()),
                },
            };
            let shutdown = self.shutdown.clone();
            let operations = tokio::select! {
                biased;
                () = shutdown.cancelled() => return Err(status(SubscribeError::ShuttingDown)),
                read = self.read(&due) => read?,
            };
            let live = !replaying;
            if operations.is_empty() && !keepalive && !first_live {
                continue;
            }
            let carried = !operations.is_empty();
            permit.send(Ok(self.frame(operations, live)));
            last_frame = Instant::now();
            if live {
                pending_live = false;
                if carried {
                    last_live_operations = Some(last_frame);
                }
            }
        }
    }

    /// Whether any followed repository is still replaying.
    fn replaying(&self) -> bool {
        self.followed
            .values()
            .any(|followed| followed.position.upto.is_some())
    }

    /// The repositories to read now: replaying, cut short, or changed
    /// since the last read, and not in the middle of a rewrite.
    fn due(&self) -> Vec<RepoId> {
        self.followed
            .iter()
            .filter(|(repo, followed)| {
                let Some(view) = self.source.view(**repo) else {
                    return false;
                };
                view.rewrite_seq.is_multiple_of(2)
                    && (followed.position.upto.is_some()
                        || followed.rebase.is_some()
                        || followed.more
                        || followed
                            .position
                            .view
                            .as_ref()
                            .is_none_or(|seen| seen.version != view.version))
            })
            .map(|(repo, _)| *repo)
            .collect()
    }

    /// Follows repositories opened since, for a subscription to every
    /// repository, and drops forgotten ones; a forgotten repository the
    /// request named ends the stream.
    fn follow_repositories(&mut self) -> Result<(), Status> {
        let open = self.source.repos();
        let gone: Vec<RepoId> = self
            .followed
            .keys()
            .filter(|repo| !open.contains(repo))
            .copied()
            .collect();
        for repo in gone {
            if self.named.is_some() {
                return Err(status(SubscribeError::Forgotten(repo)));
            }
            self.followed.remove(&repo);
        }
        if self.named.is_none() {
            for repo in open {
                if self.followed.contains_key(&repo) {
                    continue;
                }
                let Some(view) = self.source.view(repo) else {
                    continue;
                };
                let Some(opened_at) = view.opened_at.clone() else {
                    continue;
                };
                let follow = self.source.follow(repo);
                self.followed.insert(
                    repo,
                    Followed {
                        position: Position {
                            frontier: Frontier::new([opened_at]),
                            upto: None,
                            view: None,
                        },
                        more: false,
                        rewrites: view.rewrites,
                        rebase: None,
                        _follow: follow,
                    },
                );
            }
        }
        Ok(())
    }

    /// Moves a position across a rewrite of the log: to the rewritten head
    /// at once when it had everything before it, else after reading up to
    /// the head the rewrite replaced. A position replaying, already moving
    /// across one, or two rewrites behind cannot continue.
    fn apply_rewrites(&mut self) -> Result<(), Status> {
        for (repo, followed) in &mut self.followed {
            let Some(view) = self.source.view(*repo) else {
                continue;
            };
            if view.rewrites == followed.rewrites {
                continue;
            }
            let movable = view.rewrites == followed.rewrites + 1
                && followed.position.upto.is_none()
                && followed.rebase.is_none();
            let Some((before, after)) = view.last_rewrite.clone().filter(|_| movable) else {
                return Err(status(SubscribeError::Rewritten { repo: *repo }));
            };
            followed.rewrites = view.rewrites;
            if followed.position.frontier.ids() == std::slice::from_ref(&before) {
                followed.position.frontier = Frontier::new([after]);
            } else {
                followed.rebase = Some((before, after));
            }
        }
        Ok(())
    }

    /// Reads `due` repositories in order into one frame's operations,
    /// within the frame's bounds.
    async fn read(&mut self, due: &[RepoId]) -> Result<Vec<v1::Operation>, Status> {
        let mut operations = Vec::new();
        let mut bytes = 0;
        for repo in due {
            let Some(followed) = self.followed.get_mut(repo) else {
                continue;
            };
            let left = self.limits.frame_operations - operations.len();
            if left == 0 || bytes >= self.limits.frame_bytes {
                followed.more = true;
                continue;
            }
            let Some(view) = self.source.view(*repo) else {
                continue;
            };
            // During a rewrite, or after one not applied yet, the position
            // is not one the log can be read from.
            if !view.rewrite_seq.is_multiple_of(2) || view.rewrites != followed.rewrites {
                followed.more = true;
                continue;
            }
            let rebase_upto = followed
                .rebase
                .as_ref()
                .map(|(before, _)| vec![before.clone()]);
            let budget = Budget {
                operations: left,
                bytes: self.limits.frame_bytes - bytes,
                first_free: operations.is_empty(),
                effects: self.limits.effects_per_frame,
                behind: self.limits.max_behind,
            };
            let read = self
                .source
                .read(
                    *repo,
                    &followed.position.frontier,
                    rebase_upto.as_deref().or(followed.position.upto.as_deref()),
                    budget,
                )
                .await;
            // A rewrite that began while reading makes the read stale.
            let rewritten = self
                .source
                .view(*repo)
                .is_none_or(|now| now.rewrite_seq != view.rewrite_seq);
            if rewritten {
                followed.more = true;
                continue;
            }
            let batch = match read {
                Ok(batch) => batch,
                Err(ReadFailure::Behind) => {
                    return Err(status(SubscribeError::TooFarBehind {
                        repo: *repo,
                        limit: self.limits.max_behind,
                    }));
                }
                Err(ReadFailure::Rewritten) => {
                    return Err(status(SubscribeError::Rewritten { repo: *repo }));
                }
                Err(ReadFailure::Failed(error)) => return Err(error),
            };
            followed.position.view = Some(view);
            followed.position.frontier = batch.frontier;
            followed.more = batch.more;
            if !batch.more {
                if let Some((_, after)) = followed.rebase.take() {
                    followed.position.frontier = Frontier::new([after]);
                    followed.more = true;
                } else if followed.position.upto.take().is_some() {
                    // Replayed: read once more for what came since the start.
                    followed.more = true;
                }
            }
            bytes += batch
                .operations
                .iter()
                .map(prost::Message::encoded_len)
                .sum::<usize>();
            operations.extend(batch.operations);
        }
        Ok(operations)
    }

    fn frame(&self, operations: Vec<v1::Operation>, live: bool) -> SubscribeOperationsResponse {
        let positions: BTreeMap<RepoId, Frontier> = self
            .followed
            .iter()
            .map(|(repo, followed)| (*repo, followed.position.frontier.clone()))
            .collect();
        let head_operation_ids = if self.single {
            positions
                .values()
                .flat_map(|frontier| frontier.ids().iter().map(ToString::to_string))
                .collect()
        } else {
            Vec::new()
        };
        SubscribeOperationsResponse {
            operations,
            cursor: cursor::encode(&positions),
            live,
            head_operation_ids,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::subscribe::Subscriptions;

    const REPO: &str = "0123456789abcdef";

    fn repo() -> RepoId {
        REPO.parse().unwrap()
    }

    fn op_id(n: usize) -> OperationIdHex {
        format!("{n:04x}").parse().unwrap()
    }

    /// One repository with a linear log of operations `0000`, `0001`, ...
    #[derive(Debug)]
    struct Log {
        operations: usize,
        view: RepoView,
    }

    #[derive(Clone)]
    struct Fake {
        log: Arc<Mutex<Log>>,
        changes: Arc<watch::Sender<u64>>,
    }

    impl Fake {
        fn new(operations: usize) -> Fake {
            Fake {
                log: Arc::new(Mutex::new(Log {
                    operations,
                    view: RepoView::default(),
                })),
                changes: Arc::new(watch::Sender::new(0)),
            }
        }

        fn record(&self, n: usize) {
            let mut log = self.log.lock().unwrap();
            log.operations += n;
            log.view.version += 1;
            drop(log);
            self.changes.send_modify(|v| *v += 1);
        }

        fn edit(&self, f: impl FnOnce(&mut Log)) {
            f(&mut self.log.lock().unwrap());
            self.changes.send_modify(|v| *v += 1);
        }

        fn view(&self) -> RepoView {
            self.log.lock().unwrap().view.clone()
        }

        fn start(&self, at: usize, upto: Option<usize>, limits: Limits) -> mpsc::Receiver<Frame> {
            let position = Position {
                frontier: Frontier::new([op_id(at)]),
                upto: upto.map(|n| vec![op_id(n)]),
                view: Some(self.view()),
            };
            let slot = Subscriptions::default().admit(1).unwrap();
            spawn(
                self.clone(),
                limits,
                CancellationToken::new(),
                slot,
                Start {
                    named: Some(vec![repo()]),
                    positions: BTreeMap::from([(repo(), position)]),
                },
            )
        }
    }

    impl Source for Fake {
        type Follow = ();

        fn repos(&self) -> Vec<RepoId> {
            vec![repo()]
        }

        fn view(&self, _: RepoId) -> Option<RepoView> {
            Some(Fake::view(self))
        }

        fn changes(&self) -> watch::Receiver<u64> {
            self.changes.subscribe()
        }

        fn follow(&self, _: RepoId) {}

        async fn read(
            &self,
            _: RepoId,
            after: &Frontier,
            upto: Option<&[OperationIdHex]>,
            budget: Budget,
        ) -> Result<Batch, ReadFailure> {
            // A read is a task on the blocking pool in the daemon.
            tokio::task::yield_now().await;
            let operations = self.log.lock().unwrap().operations;
            let parse = |id: &OperationIdHex| usize::from_str_radix(id.as_str(), 16).unwrap();
            let Some(at) = after.ids().first().map(parse) else {
                return Err(ReadFailure::Rewritten);
            };
            if at >= operations {
                return Err(ReadFailure::Rewritten);
            }
            let end = upto.map_or(operations - 1, |ids| parse(&ids[0]));
            if end < at {
                // The position is not behind the heads: not in their history.
                return Err(ReadFailure::Rewritten);
            }
            if end - at > budget.behind {
                return Err(ReadFailure::Behind);
            }
            let last = end.min(at + budget.operations);
            Ok(Batch {
                operations: (at + 1..=last)
                    .map(|n| v1::Operation {
                        id: op_id(n).to_string(),
                        ..v1::Operation::default()
                    })
                    .collect(),
                frontier: Frontier::new([op_id(last)]),
                more: last < end,
            })
        }
    }

    async fn next(frames: &mut mpsc::Receiver<Frame>) -> SubscribeOperationsResponse {
        frames.recv().await.unwrap().unwrap()
    }

    fn ids(frame: &SubscribeOperationsResponse) -> Vec<usize> {
        frame
            .operations
            .iter()
            .map(|op| usize::from_str_radix(&op.id, 16).unwrap())
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn live_frames_are_prompt_and_spaced() {
        let fake = Fake::new(1);
        let mut frames = fake.start(0, None, Limits::default());
        let started = Instant::now();
        let first = next(&mut frames).await;
        assert!(first.live && first.operations.is_empty());
        assert_eq!(first.head_operation_ids, vec![op_id(0).to_string()]);
        assert_eq!(started.elapsed(), Duration::ZERO, "at once");

        // An isolated operation is not delayed.
        tokio::time::sleep(Duration::from_secs(1)).await;
        fake.record(1);
        let at = Instant::now();
        let frame = next(&mut frames).await;
        assert_eq!(ids(&frame), [1]);
        assert_eq!(at.elapsed(), Duration::ZERO);

        // One right after waits out the interval, and takes what came
        // meanwhile with it.
        fake.record(1);
        tokio::time::sleep(Duration::from_millis(10)).await;
        fake.record(2);
        let frame = next(&mut frames).await;
        assert_eq!(ids(&frame), [2, 3, 4]);
        assert_eq!(at.elapsed(), Duration::from_millis(50));
        assert_eq!(frame.head_operation_ids, vec![op_id(4).to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn frames_are_cut_and_the_rest_follows_at_once() {
        let fake = Fake::new(1);
        let limits = Limits {
            frame_operations: 10,
            ..Limits::default()
        };
        let mut frames = fake.start(0, None, limits);
        next(&mut frames).await;
        fake.record(25);
        let at = Instant::now();
        let sizes: Vec<usize> = [
            next(&mut frames).await,
            next(&mut frames).await,
            next(&mut frames).await,
        ]
        .iter()
        .map(|frame| frame.operations.len())
        .collect();
        assert_eq!(sizes, [10, 10, 5]);
        assert_eq!(
            at.elapsed(),
            Duration::ZERO,
            "a cut frame's rest is not spaced"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn keepalives_carry_the_position() {
        let fake = Fake::new(1);
        let mut frames = fake.start(0, None, Limits::default());
        let first = next(&mut frames).await;
        let at = Instant::now();
        let keepalive = next(&mut frames).await;
        assert_eq!(at.elapsed(), Duration::from_secs(15));
        assert!(keepalive.operations.is_empty() && keepalive.live);
        assert_eq!(keepalive.cursor, first.cursor);
    }

    #[tokio::test(start_paused = true)]
    async fn replay_then_live() {
        let fake = Fake::new(30);
        let limits = Limits {
            frame_operations: 10,
            ..Limits::default()
        };
        let mut frames = fake.start(4, Some(29), limits);
        let mut replayed = Vec::new();
        for _ in 0..3 {
            let frame = next(&mut frames).await;
            assert!(!frame.live);
            replayed.extend(ids(&frame));
        }
        assert_eq!(replayed, (5..30).collect::<Vec<_>>());
        let live = next(&mut frames).await;
        assert!(live.live && live.operations.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_reader_catches_up_or_is_told_it_is_too_far_behind() {
        let fake = Fake::new(1);
        let limits = Limits {
            max_behind: 100,
            ..Limits::default()
        };
        let mut frames = fake.start(0, None, limits);
        next(&mut frames).await;
        // The channel holds one frame: the session waits on it, not on
        // the log.
        fake.record(1);
        tokio::time::sleep(Duration::from_secs(1)).await;
        fake.record(50);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(ids(&next(&mut frames).await), [1]);
        assert_eq!(ids(&next(&mut frames).await), (2..=51).collect::<Vec<_>>());
        fake.record(1);
        tokio::time::sleep(Duration::from_secs(1)).await;
        fake.record(150);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(ids(&next(&mut frames).await), [52]);
        let error = frames.recv().await.unwrap().unwrap_err();
        let detail = reef_api::error::detail(&error).unwrap();
        assert_eq!(detail.reason, reef_api::error::TOO_FAR_BEHIND);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_ends_the_stream_without_a_reason() {
        let fake = Fake::new(1);
        let shutdown = CancellationToken::new();
        let slot = Subscriptions::default().admit(1).unwrap();
        let mut frames = spawn(
            fake.clone(),
            Limits::default(),
            shutdown.clone(),
            slot,
            Start {
                named: None,
                positions: BTreeMap::from([(
                    repo(),
                    Position {
                        frontier: Frontier::new([op_id(0)]),
                        upto: None,
                        view: Some(fake.view()),
                    },
                )]),
            },
        );
        let first = next(&mut frames).await;
        assert!(
            first.head_operation_ids.is_empty(),
            "not one named repository"
        );
        shutdown.cancel();
        let error = frames.recv().await.unwrap().unwrap_err();
        assert_eq!(error.code(), tonic::Code::Unavailable);
        assert_eq!(reef_api::error::detail(&error).unwrap().reason, "");
        assert!(frames.recv().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_position_behind_a_rewrite_reads_up_to_it_first() {
        let fake = Fake::new(4);
        // Operations 2 and 3 are recorded but not read yet when GC begins.
        let mut frames = fake.start(1, None, Limits::default());
        next(&mut frames).await;
        fake.edit(|log| {
            log.view.rewrite_seq += 1;
            log.view.version += 1;
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            frames.try_recv().is_err(),
            "nothing is read during a rewrite"
        );
        // GC rewrote from `0003` to `0009`, and the rewrite ended.
        fake.edit(|log| {
            log.operations = 10;
            log.view.rewrite_seq += 1;
            log.view.rewrites += 1;
            log.view.last_rewrite = Some((op_id(3), op_id(9)));
            log.view.version += 1;
        });
        let frame = next(&mut frames).await;
        assert_eq!(ids(&frame), [2, 3], "up to the head GC rewrote");
        assert!(frame.live);
        tokio::time::sleep(Duration::from_secs(1)).await;
        fake.record(1);
        assert_eq!(
            ids(&next(&mut frames).await),
            [10],
            "then on from its rewrite"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rewrites_move_a_caught_up_position_and_end_others() {
        let fake = Fake::new(3);
        let mut frames = fake.start(2, None, Limits::default());
        next(&mut frames).await;
        // GC rewrote the log from `0002`, where the session stands, to
        // `0009`: it moves there and sends nothing.
        fake.edit(|log| {
            log.operations = 10;
            log.view.rewrite_seq += 2;
            log.view.rewrites += 1;
            log.view.last_rewrite = Some((op_id(2), op_id(9)));
            log.view.version += 1;
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        fake.record(1);
        assert_eq!(ids(&next(&mut frames).await), [10]);

        // A rewrite from elsewhere ends the stream.
        fake.edit(|log| {
            log.view.rewrite_seq += 2;
            log.view.rewrites += 1;
            log.view.last_rewrite = Some((op_id(3), op_id(20)));
            log.view.version += 1;
        });
        let error = frames.recv().await.unwrap().unwrap_err();
        assert_eq!(
            reef_api::error::detail(&error).unwrap().reason,
            reef_api::error::OPERATION_LOG_REWRITTEN
        );
    }
}
