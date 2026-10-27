//! What the daemon reports itself (`by_daemon`): lease heartbeats, the
//! files a workspace's snapshots change, and the progress of creates and
//! store builds. Each entry names the workspace's owner as its agent, so
//! one workspace's daemon entries agree.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use reef_api::v1::{self, PresenceEndReason, PresenceKind};
use reef_lib::error::ErrorChain;
use reef_lib::jj::CommitIdHex;
use reef_lib::jj::diff;
use tokio::sync::Semaphore;

use super::hub::{DaemonKey, PresenceHub, Report};
use super::{MAX_ENTRY_BYTES, MAX_PATHS};
use crate::daemon::AppState;
use crate::model::{RepoId, WorkspaceId};
use crate::snapshot::{SnapshotHook, SnapshotKind, SnapshotOutcome};

/// How long a progress entry lasts after its last update.
const PROGRESS_TTL: Duration = Duration::from_secs(600);

/// Publishes or replaces `workspace`'s lease entry after a heartbeat: it
/// lapses after two missed heartbeats, long before the lease does.
pub fn lease(
    hub: &PresenceHub,
    repo: RepoId,
    workspace: WorkspaceId,
    change_id: &str,
    lease: v1::Lease,
    heartbeat_every: Duration,
) {
    let owner = lease.owner.clone();
    hub.publish_daemon(DaemonKey::Lease(workspace), |_| Report {
        agent: owner,
        repo: Some(repo),
        workspace: Some(workspace),
        change_id: change_id.to_owned(),
        kind: PresenceKind::Lease,
        lease: Some(lease),
        ttl: heartbeat_every * 2,
        ..Report::default()
    });
}

/// Publishes or replaces `workspace`'s editing entry after a snapshot
/// changed `paths`: they go in front of the entry's previous paths, most
/// recent first, within the entry's bounds.
pub fn editing(
    hub: &PresenceHub,
    repo: RepoId,
    workspace: WorkspaceId,
    owner: &str,
    change_id: &str,
    paths: Vec<String>,
) {
    let ttl = hub.limits().editing_ttl;
    hub.publish_daemon(DaemonKey::Editing(workspace), |previous| {
        let mut merged = paths;
        if let Some(previous) = previous {
            for path in &previous.paths {
                if !merged.contains(path) {
                    merged.push(path.clone());
                }
            }
        }
        merged.truncate(MAX_PATHS);
        // Paths alone stay under the entry's bound, leaving room for the
        // rest of it.
        let mut bytes = 0;
        let fits = merged
            .iter()
            .take_while(|path| {
                bytes += path.len() + 2;
                bytes <= MAX_ENTRY_BYTES / 2
            })
            .count();
        merged.truncate(fits);
        Report {
            agent: owner.to_owned(),
            repo: Some(repo),
            workspace: Some(workspace),
            change_id: change_id.to_owned(),
            kind: PresenceKind::Editing,
            paths: merged,
            ttl,
            ..Report::default()
        }
    });
}

/// The post-snapshot hook behind editing entries: after a watcher or
/// explicit snapshot that recorded an operation, the files it changed,
/// from a diff of the commit it rewrote to its new commit, so ignored files
/// never appear. The diff runs on the blocking pool under its own permits,
/// off the snapshot's path; one that waits more than a second for a permit
/// refreshes the entry without new paths.
pub(crate) fn editing_hook(state: Weak<AppState>) -> SnapshotHook {
    let permits = Arc::new(Semaphore::new(2));
    Box::new(move |outcome: &SnapshotOutcome| {
        if !outcome.changed
            || !matches!(outcome.kind, SnapshotKind::Watcher | SnapshotKind::Explicit)
        {
            return;
        }
        let Some(state) = state.upgrade() else {
            return;
        };
        let Some(entry) = state.manager.entry(outcome.workspace) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let record = entry.record();
        let (rewrote, to) = (outcome.rewrote.clone(), outcome.commit_id.clone());
        let change_id = outcome.change_id.clone();
        let permits = Arc::clone(&permits);
        runtime.spawn(async move {
            let paths = match rewrote {
                Some(from) => changed(&state, record.repo_id, from, to, permits).await,
                None => Vec::new(),
            };
            editing(
                state.manager.presence(),
                record.repo_id,
                record.id,
                &record.owner,
                &change_id,
                paths,
            );
        });
    })
}

/// The paths that differ from `from` to `to`, [`MAX_PATHS`] at most;
/// empty when the read cannot run soon or fails.
async fn changed(
    state: &AppState,
    repo: RepoId,
    from: CommitIdHex,
    to: CommitIdHex,
    permits: Arc<Semaphore>,
) -> Vec<String> {
    let Ok(Ok(permit)) =
        tokio::time::timeout(Duration::from_secs(1), permits.acquire_owned()).await
    else {
        return Vec::new();
    };
    let Ok(handle) = state.manager.handle(repo) else {
        return Vec::new();
    };
    let tip = match state.events.tip(repo) {
        Some(tip) => tip,
        None => match state.reads.loaded(&handle).await {
            Ok(tip) => tip,
            Err(_) => return Vec::new(),
        },
    };
    let read = state
        .reads
        .run_holding(&handle, permit, move || async move {
            diff::changed_paths(&tip, &from, &to, MAX_PATHS).await
        })
        .await;
    match read {
        Ok(Ok(paths)) => paths,
        Ok(Err(error)) => {
            tracing::debug!(error = %ErrorChain(&error), "cannot list the paths a snapshot changed");
            Vec::new()
        }
        Err(_) => Vec::new(),
    }
}

/// Ends `workspace`'s editing entry: a move put it on another change.
pub fn moved(hub: &PresenceHub, workspace: WorkspaceId) {
    hub.end_daemon(DaemonKey::Editing(workspace), PresenceEndReason::Cleared);
}

/// One long-running operation's progress entry, published once the
/// operation has run [`super::Limits::progress_after`] and ended
/// (`CLEARED`) when the last clone is dropped, whatever the outcome.
#[derive(Clone, Debug)]
pub struct Progress {
    inner: Arc<ProgressInner>,
}

#[derive(Debug)]
struct ProgressInner {
    hub: Arc<PresenceHub>,
    key: DaemonKey,
    state: Mutex<ProgressState>,
}

#[derive(Debug)]
struct ProgressState {
    base: Report,
    progress: v1::Progress,
    /// Past the delay: every update publishes.
    shown: bool,
    published: bool,
}

/// Progress keys, daemon-wide.
static NEXT_PROGRESS: AtomicU64 = AtomicU64::new(1);

impl Progress {
    /// Starts an operation's progress: `base` gives its agent, repository
    /// and workspace, `phase` its first phase.
    pub fn start(
        hub: &Arc<PresenceHub>,
        base: Report,
        phase: &str,
        total: u64,
        unit: &str,
    ) -> Progress {
        let after = hub.limits().progress_after;
        let inner = Arc::new(ProgressInner {
            hub: Arc::clone(hub),
            key: DaemonKey::Progress(NEXT_PROGRESS.fetch_add(1, Ordering::Relaxed)),
            state: Mutex::new(ProgressState {
                base,
                progress: v1::Progress {
                    phase: phase.to_owned(),
                    done: 0,
                    total,
                    unit: unit.to_owned(),
                },
                shown: false,
                published: false,
            }),
        });
        if after.is_zero() {
            inner.show();
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let weak: Weak<ProgressInner> = Arc::downgrade(&inner);
            runtime.spawn(async move {
                tokio::time::sleep(after).await;
                if let Some(inner) = weak.upgrade() {
                    inner.show();
                }
            });
        }
        Progress { inner }
    }

    /// Moves to `phase`, with `done` units finished.
    pub fn phase(&self, phase: &str, done: u64) {
        self.inner.update(|progress| {
            phase.clone_into(&mut progress.phase);
            progress.done = done;
        });
    }

    /// Adds `n` units done.
    pub fn add(&self, n: u64) {
        self.inner.update(|progress| progress.done += n);
    }
}

impl ProgressInner {
    fn lock(&self) -> std::sync::MutexGuard<'_, ProgressState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn show(&self) {
        let mut state = self.lock();
        state.shown = true;
        self.publish(&mut state);
    }

    fn update(&self, change: impl FnOnce(&mut v1::Progress)) {
        let mut state = self.lock();
        change(&mut state.progress);
        if state.shown {
            self.publish(&mut state);
        }
    }

    fn publish(&self, state: &mut ProgressState) {
        let report = Report {
            kind: PresenceKind::Progress,
            progress: Some(state.progress.clone()),
            ttl: PROGRESS_TTL,
            ..state.base.clone()
        };
        self.hub.publish_daemon(self.key, |_| report);
        state.published = true;
    }
}

impl Drop for ProgressInner {
    fn drop(&mut self) {
        let published = self
            .state
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .published;
        if published {
            self.hub.end_daemon(self.key, PresenceEndReason::Cleared);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presence::{Limits, PresenceFilter};

    fn hub(after: Duration) -> Arc<PresenceHub> {
        Arc::new(PresenceHub::new(Limits {
            progress_after: after,
            ..Limits::default()
        }))
    }

    fn base() -> Report {
        Report {
            agent: "owner".to_owned(),
            repo: Some(RepoId::generate()),
            ..Report::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn progress_waits_then_publishes_and_ends_on_drop() {
        let hub = hub(Duration::from_millis(250));
        let progress = Progress::start(&hub, base(), "resolve", 8, "steps");
        progress.phase("base", 1);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(hub.entries().len(), 0, "nothing before the delay");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let entries = hub.entries();
        assert_eq!(entries.len(), 1);
        let shown = entries[0].progress.as_ref().unwrap();
        assert_eq!(
            (shown.phase.as_str(), shown.done, shown.total),
            ("base", 1, 8)
        );
        assert!(entries[0].by_daemon);
        progress.phase("materialize", 2);
        assert_eq!(hub.entries()[0].progress.as_ref().unwrap().done, 2);
        let (_, mut rx) = hub.subscribe(&PresenceFilter::default());
        drop(progress);
        assert_eq!(hub.entries().len(), 0);
        let ended = rx.try_recv().unwrap();
        assert_eq!(ended.end, Some(PresenceEndReason::Cleared));
    }

    #[tokio::test(start_paused = true)]
    async fn a_short_operation_publishes_nothing() {
        let hub = hub(Duration::from_millis(250));
        let (_, mut rx) = hub.subscribe(&PresenceFilter::default());
        let progress = Progress::start(&hub, base(), "resolve", 8, "steps");
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(progress);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(rx.try_recv().is_err());
        assert_eq!(hub.entries().len(), 0);
    }

    #[tokio::test]
    async fn progress_ends_on_panic() {
        let hub = hub(Duration::ZERO);
        let progress = Progress::start(&hub, base(), "build", 0, "lines");
        let moved = progress.clone();
        drop(progress);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            moved.add(1);
            panic!("the operation failed");
        }));
        assert_eq!(hub.entries().len(), 0);
    }

    #[tokio::test]
    async fn editing_merges_paths_most_recent_first() {
        let hub = hub(Duration::ZERO);
        let (repo, ws) = (RepoId::generate(), WorkspaceId::generate());
        editing(&hub, repo, ws, "o", "c", vec!["a".into(), "b".into()]);
        editing(&hub, repo, ws, "o", "c", vec!["c".into(), "a".into()]);
        assert_eq!(
            hub.daemon_entry(DaemonKey::Editing(ws)).unwrap().paths,
            ["c", "a", "b"]
        );
        moved(&hub, ws);
        assert_eq!(hub.entries().len(), 0);
    }
}
