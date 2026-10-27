//! [`OpEvents`]: the hub every subscription waits on.
//!
//! Per repository it holds a version that changes whenever the log may
//! have, the last tip the repository thread loaded (so a subscription reads
//! the op store without a job on that thread, which may be busy), and the
//! state of a rewrite (GC). One daemon-wide `watch` is bumped on every
//! change; a subscription awaits it and compares the versions of the
//! repositories it follows.
//!
//! The repository thread calls [`OpEvents::moved`] after every job that
//! moved its head, which covers every operation Reef records. Operations
//! other `jj` processes write are found by [`OpEvents::poll`], which reads
//! the op heads of each followed repository every
//! [`super::Limits::outside_poll`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use reef_lib::jj::oplog;
use reef_lib::jj::{OperationIdHex, ReadonlyRepo};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::model::RepoId;

/// What [`OpEvents`] knows of one repository.
#[derive(Debug)]
struct Watched {
    /// The repository as its thread last loaded it.
    tip: Arc<ReadonlyRepo>,
    view: RepoView,
    /// The op heads last seen, by the poll or a wake-up.
    heads: Vec<OperationIdHex>,
    /// Subscriptions following it; the poll reads only these.
    followers: usize,
}

/// What a subscription compares between reads of one repository.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RepoView {
    /// Changes whenever the log may have.
    pub(crate) version: u64,
    /// The op head when it was opened: where a subscription to every
    /// repository follows one registered after it started.
    pub(crate) opened_at: Option<OperationIdHex>,
    /// Odd while a rewrite is under way.
    pub(crate) rewrite_seq: u64,
    /// Rewrites that moved the op heads.
    pub(crate) rewrites: u64,
    /// The last of those: the head it rewrote, and the head after.
    pub(crate) last_rewrite: Option<(OperationIdHex, OperationIdHex)>,
}

/// The hub; one in [`crate::AppState::events`], shared with the
/// repository registry.
#[derive(Debug)]
pub struct OpEvents {
    repos: Mutex<HashMap<RepoId, Watched>>,
    changed: watch::Sender<u64>,
}

impl Default for OpEvents {
    fn default() -> OpEvents {
        OpEvents {
            repos: Mutex::new(HashMap::new()),
            changed: watch::Sender::new(0),
        }
    }
}

impl OpEvents {
    fn with<T>(&self, f: impl FnOnce(&mut HashMap<RepoId, Watched>) -> T) -> T {
        let mut repos = self.repos.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut repos)
    }

    fn bump(&self) {
        self.changed.send_modify(|version| *version += 1);
    }

    /// A receiver that sees every change.
    pub(crate) fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// A repository's thread opened, at `tip`.
    pub fn added(&self, repo: RepoId, tip: Arc<ReadonlyRepo>) {
        let head = oplog::op_id_of(&tip);
        self.with(|repos| {
            repos.insert(
                repo,
                Watched {
                    tip,
                    view: RepoView {
                        opened_at: Some(head.clone()),
                        ..RepoView::default()
                    },
                    heads: vec![head],
                    followers: 0,
                },
            );
        });
        self.bump();
    }

    /// A repository was forgotten.
    pub fn removed(&self, repo: RepoId) {
        if self.with(|repos| repos.remove(&repo)).is_some() {
            self.bump();
        }
    }

    /// The repository thread's head moved to `tip`: it recorded an
    /// operation, merged concurrent ones, or loaded another process's.
    pub fn moved(&self, repo: RepoId, tip: Arc<ReadonlyRepo>) {
        let found = self.with(|repos| {
            let watched = repos.get_mut(&repo)?;
            watched.heads = vec![oplog::op_id_of(&tip)];
            watched.tip = tip;
            watched.view.version += 1;
            Some(())
        });
        if found.is_some() {
            self.bump();
        }
    }

    /// The op heads may have moved without the thread loading them.
    fn poked(&self, repo: RepoId, heads: Vec<OperationIdHex>) {
        let changed = self.with(|repos| {
            let watched = repos.get_mut(&repo)?;
            if watched.heads == heads {
                return None;
            }
            watched.heads = heads;
            watched.view.version += 1;
            Some(())
        });
        if changed.is_some() {
            self.bump();
        }
    }

    /// A rewrite of `repo`'s log begins. The returned guard ends it when
    /// dropped, after [`Rewrite::moved_to`] if the op heads moved.
    pub fn rewrite(self: &Arc<Self>, repo: RepoId) -> Rewrite {
        self.with(|repos| {
            if let Some(watched) = repos.get_mut(&repo) {
                watched.view.rewrite_seq += 1;
                watched.view.version += 1;
            }
        });
        self.bump();
        Rewrite {
            events: Arc::clone(self),
            repo,
        }
    }

    /// The repositories open now.
    pub(crate) fn repos(&self) -> Vec<RepoId> {
        self.with(|repos| repos.keys().copied().collect())
    }

    /// What a subscription compares for `repo`, if it is open.
    pub(crate) fn view(&self, repo: RepoId) -> Option<RepoView> {
        self.with(|repos| repos.get(&repo).map(|watched| watched.view.clone()))
    }

    /// `repo` as its thread last loaded it, if it is open.
    pub(crate) fn tip(&self, repo: RepoId) -> Option<Arc<ReadonlyRepo>> {
        self.with(|repos| repos.get(&repo).map(|watched| Arc::clone(&watched.tip)))
    }

    /// Counts a subscription following `repo` until the guard drops.
    pub(crate) fn follow(self: &Arc<Self>, repo: RepoId) -> Follow {
        self.with(|repos| {
            if let Some(watched) = repos.get_mut(&repo) {
                watched.followers += 1;
            }
        });
        Follow {
            events: Arc::clone(self),
            repo,
        }
    }

    /// Reads the op heads of every followed repository every `every`
    /// until `shutdown`, waking subscriptions when they changed: how
    /// operations other `jj` processes write are found.
    pub fn poll(self: Arc<Self>, every: Duration, shutdown: CancellationToken) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    _ = tick.tick() => {}
                }
                let followed: Vec<(RepoId, Arc<ReadonlyRepo>)> = self.with(|repos| {
                    repos
                        .iter()
                        .filter(|(_, watched)| watched.followers > 0)
                        .map(|(repo, watched)| (*repo, Arc::clone(&watched.tip)))
                        .collect()
                });
                if followed.is_empty() {
                    continue;
                }
                let read = tokio::task::spawn_blocking(move || {
                    followed
                        .into_iter()
                        .filter_map(|(repo, tip)| {
                            let heads = pollster::block_on(oplog::op_heads(&tip)).ok()?;
                            Some((repo, heads))
                        })
                        .collect::<Vec<_>>()
                })
                .await;
                for (repo, heads) in read.unwrap_or_default() {
                    self.poked(repo, heads);
                }
            }
        });
    }
}

/// A subscription following one repository; see [`OpEvents::follow`].
#[derive(Debug)]
pub(crate) struct Follow {
    events: Arc<OpEvents>,
    repo: RepoId,
}

impl Drop for Follow {
    fn drop(&mut self) {
        self.events.with(|repos| {
            if let Some(watched) = repos.get_mut(&self.repo) {
                watched.followers = watched.followers.saturating_sub(1);
            }
        });
    }
}

/// A rewrite under way; see [`OpEvents::rewrite`].
#[derive(Debug)]
pub struct Rewrite {
    events: Arc<OpEvents>,
    repo: RepoId,
}

impl Rewrite {
    /// The op heads moved from `before` to `after`, its rewrite.
    pub fn moved_to(&self, before: &OperationIdHex, after: &OperationIdHex) {
        self.events.with(|repos| {
            if let Some(watched) = repos.get_mut(&self.repo) {
                watched.view.rewrites += 1;
                watched.view.last_rewrite = Some((before.clone(), after.clone()));
                watched.heads = vec![after.clone()];
                watched.view.version += 1;
            }
        });
        self.events.bump();
    }
}

impl Drop for Rewrite {
    fn drop(&mut self) {
        self.events.with(|repos| {
            if let Some(watched) = repos.get_mut(&self.repo) {
                watched.view.rewrite_seq += 1;
                watched.view.version += 1;
            }
        });
        self.events.bump();
    }
}
