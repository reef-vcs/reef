//! The task behind each `WatchInbox` stream (#372, ADR 0091).
//!
//! Per repository it holds the board it last sent and the digest it last
//! sent. It takes the channel's one slot before it compares them with the
//! newest board, so while the client does not read it builds nothing, and
//! when the slot frees it sends the difference to the inbox as it is then:
//! a slow client misses intermediate states, never the latest (ADR 0037
//! decisions 8 and 9). The frames one difference is cut into wait here, at
//! most one inbox's worth.

use std::collections::{BTreeMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use reef_api::v1::{self, WatchInboxResponse};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tonic::Status;

use super::Viewer;
use super::watch::{Board, Watch};
use crate::daemon::AppState;
use crate::inbox::board::{self, RepoBoard};
use crate::model::RepoId;
use crate::subscribe::Slot;

/// One frame, or the error that ends the stream.
pub(crate) type Frame = Result<WatchInboxResponse, Status>;

/// What a stream follows.
pub(crate) struct Start {
    pub(crate) viewer: Viewer,
    /// The repositories the request named; `None` for every one, those
    /// registered later included.
    pub(crate) named: Option<Vec<RepoId>>,
    pub(crate) watch: Watch,
    pub(crate) slot: Slot,
}

/// Starts the stream's task; its frames arrive on the receiver.
pub(crate) fn spawn(state: Arc<AppState>, start: Start) -> mpsc::Receiver<Frame> {
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        let Start {
            viewer,
            named,
            watch,
            slot,
        } = start;
        let _slot = slot;
        let mut session = Session {
            keepalive: state.config.subscribe.keepalive,
            state,
            viewer,
            named,
            watch,
            sent: BTreeMap::new(),
            frames: VecDeque::new(),
            synced: false,
        };
        let served = AssertUnwindSafe(session.serve(&tx)).catch_unwind().await;
        let error = match served {
            Ok(Ok(())) => return,
            Ok(Err(error)) => error,
            Err(_) => {
                tracing::error!("inbox stream task panicked");
                Status::internal("the stream failed")
            }
        };
        // The client may not be reading; the stream ends either way.
        let _ = tokio::time::timeout(Duration::from_secs(1), tx.send(Err(error))).await;
    });
    rx
}

/// What a stream last sent of one repository.
struct Sent {
    board: Arc<RepoBoard>,
    landed: v1::LandedDigest,
}

struct Session {
    state: Arc<AppState>,
    viewer: Viewer,
    named: Option<Vec<RepoId>>,
    watch: Watch,
    keepalive: Duration,
    sent: BTreeMap<RepoId, Sent>,
    /// The rest of one difference, cut into frames.
    frames: VecDeque<Vec<v1::InboxSectionChange>>,
    /// The first inbox has been sent whole.
    synced: bool,
}

fn shutting_down() -> Status {
    Status::unavailable("the daemon is shutting down")
}

/// The board's task ended under a stream that still watches it: the daemon
/// is shutting down, or the task failed and the next watcher starts another.
fn board_ended(state: &AppState) -> Status {
    if state.shutdown.is_cancelled() {
        shutting_down()
    } else {
        Status::unavailable("the inbox stopped; watch it again")
    }
}

impl Session {
    async fn serve(&mut self, tx: &mpsc::Sender<Frame>) -> Result<(), Status> {
        // Each repository is sent as the board reads it, so one whose
        // thread is busy holds back no other; `synced` marks the frame
        // that completes the first inbox.
        let mut last_frame = Instant::now();
        let mut marks_moved = false;
        let mut first = true;
        // The board the frames in hand were cut from held the first inbox.
        let mut whole = false;
        loop {
            let mut keepalive = false;
            if self.frames.is_empty() && !marks_moved && !first {
                tokio::select! {
                    biased;
                    () = self.state.shutdown.cancelled() => return Err(shutting_down()),
                    () = tx.closed() => return Ok(()),
                    changed = self.watch.board.changed() => {
                        if changed.is_err() {
                            return Err(board_ended(&self.state));
                        }
                    }
                    changed = self.watch.marks.changed() => {
                        if changed.is_err() {
                            return Err(shutting_down());
                        }
                        marks_moved = true;
                    }
                    () = tokio::time::sleep_until(last_frame + self.keepalive) => keepalive = true,
                }
            }
            first = false;
            let permit = tokio::select! {
                biased;
                () = self.state.shutdown.cancelled() => return Err(shutting_down()),
                permit = tx.reserve() => match permit {
                    Ok(permit) => permit,
                    Err(_) => return Ok(()),
                },
            };
            if self.frames.is_empty() {
                marks_moved |= self.watch.marks.has_changed().unwrap_or(false);
                self.watch.marks.borrow_and_update();
                let board = Arc::clone(&self.watch.board.borrow_and_update());
                whole = self.complete(&board);
                let changes = self.changes(&board, std::mem::take(&mut marks_moved)).await;
                self.frames = board::frames(changes).into();
            }
            let sections = self.frames.pop_front().unwrap_or_default();
            let synced = self.synced || (self.frames.is_empty() && whole);
            if sections.is_empty() && synced == self.synced && !keepalive {
                continue;
            }
            self.synced = synced;
            permit.send(Ok(WatchInboxResponse { sections, synced }));
            last_frame = Instant::now();
        }
    }

    /// Whether `board` holds every repository this stream's first inbox
    /// shows: all once read, or each named one still registered.
    fn complete(&self, board: &Board) -> bool {
        let Some(named) = &self.named else {
            return board.ready;
        };
        let missing: Vec<&RepoId> = named
            .iter()
            .filter(|repo| !board.repos.contains_key(repo))
            .collect();
        missing.is_empty()
            || super::registered(&self.state)
                .is_ok_and(|registered| missing.iter().all(|repo| !registered.contains(repo)))
    }

    /// Every followed repository's change since what this stream sent:
    /// its items and keys, and its digest when `marks_moved` or its board
    /// did. Sent state moves to `board` as each change is built.
    async fn changes(&mut self, board: &Board, marks_moved: bool) -> Vec<v1::InboxSectionChange> {
        let mut changes = Vec::new();
        for (repo, now) in &board.repos {
            if self
                .named
                .as_ref()
                .is_some_and(|named| !named.contains(repo))
            {
                continue;
            }
            let sent = self.sent.get(repo);
            // A digest that failed is read again on the next pass (a
            // keepalive's at the latest), not only when the board moves.
            if !marks_moved
                && sent
                    .is_some_and(|sent| sent.board.rev == now.rev && sent.board.error == now.error)
            {
                continue;
            }
            let (items, left) = board::diff(sent.map(|sent| &*sent.board), now);
            let (landed, digest_error) =
                match super::digest(&self.state, *repo, &self.viewer, None, Some(&now.subjects))
                    .await
                {
                    Ok(landed) => (landed, None),
                    Err(error) => (
                        sent.map(|sent| sent.landed.clone()).unwrap_or_default(),
                        Some(super::inbox_error(&error)),
                    ),
                };
            let error = now.error.clone().or(digest_error);
            let unchanged = sent.is_some_and(|sent| {
                items.is_empty()
                    && left.is_empty()
                    && sent.board.needs_you_now_total == now.needs_you_now_total
                    && sent.board.watching_total == now.watching_total
                    && sent.board.error == error
                    && sent.landed == landed
            });
            if !unchanged {
                changes.push(v1::InboxSectionChange {
                    repo_id: repo.as_hex(),
                    items,
                    left,
                    needs_you_now_total: now.needs_you_now_total,
                    watching_total: now.watching_total,
                    landed: Some(landed.clone()),
                    error: error.clone(),
                    removed: false,
                });
            }
            let mut held = Arc::clone(now);
            if held.error != error {
                // A digest that failed is this stream's error, not the board's.
                let mut own = (*held).clone();
                own.error = error;
                held = Arc::new(own);
            }
            self.sent.insert(
                *repo,
                Sent {
                    board: held,
                    landed,
                },
            );
        }
        let gone: Vec<RepoId> = self
            .sent
            .keys()
            .filter(|repo| !board.repos.contains_key(repo))
            .copied()
            .collect();
        for repo in gone {
            if let Some(sent) = self.sent.remove(&repo) {
                changes.push(v1::InboxSectionChange {
                    repo_id: repo.as_hex(),
                    left: sent.board.items.keys().cloned().collect(),
                    removed: true,
                    ..v1::InboxSectionChange::default()
                });
            }
        }
        changes
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tokio::sync::watch;

    use super::*;
    use crate::testing::TestDaemon;

    fn board(repo: RepoId, n: u64) -> Arc<Board> {
        let key = format!("help:{}:{n}", repo.as_hex());
        let item = v1::InboxItem {
            key: key.clone(),
            repo_id: repo.as_hex(),
            bucket: v1::InboxBucket::NeedsYouNow.into(),
            ..v1::InboxItem::default()
        };
        let repo_board = RepoBoard {
            rev: n + 1,
            items: BTreeMap::from([(key, item)]),
            needs_you_now_total: 1,
            watching_total: 0,
            error: None,
            subjects: Arc::default(),
            restale_at: None,
        };
        Arc::new(Board {
            repos: BTreeMap::from([(repo, Arc::new(repo_board))]),
            ready: true,
        })
    }

    // ADR 0037 decisions 8 and 9 for the inbox: a client that reads nothing
    // while the board moves 200 times holds the stream to one built frame,
    // and once it reads it gets the latest board, never the history.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_that_stops_reading_gets_the_latest_board() {
        let (daemon, repo) =
            TestDaemon::with_synthetic_repo(reef_testing::synth::Preset::Small).await;
        let state = Arc::clone(daemon.handle.state());
        let repo: RepoId = repo.id.parse().unwrap();
        let (publish, read) = watch::channel(board(repo, 0));
        let start = Start {
            viewer: Viewer::Nobody,
            named: None,
            watch: state.inbox.of(read),
            slot: state.subscriptions.admit(usize::MAX).unwrap(),
        };
        let mut frames = spawn(Arc::clone(&state), start);
        let first = frames.recv().await.unwrap().unwrap();
        assert!(first.synced);
        assert_eq!(first.sections[0].items.len(), 1);
        for n in 1..=200 {
            publish.send_replace(board(repo, n));
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut held: Vec<String> = first.sections[0]
            .items
            .iter()
            .map(|i| i.key.clone())
            .collect();
        let mut count = 0;
        while let Ok(Some(frame)) =
            tokio::time::timeout(Duration::from_millis(500), frames.recv()).await
        {
            let frame = frame.unwrap();
            count += 1;
            for section in frame.sections {
                held.retain(|key| !section.left.contains(key));
                held.extend(section.items.into_iter().map(|item| item.key));
            }
        }
        assert!(count <= 2, "{count} frames for 200 boards read late");
        assert_eq!(held, [format!("help:{}:200", repo.as_hex())]);
    }
}
