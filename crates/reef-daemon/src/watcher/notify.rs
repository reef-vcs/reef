//! [`NotifyWatcher`]: one debounced `notify` watcher for every workspace.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{
    DebounceEventHandler, DebounceEventResult, Debouncer, RecommendedCache, new_debouncer_opt,
};
use reef_lib::error::ErrorChain;
use tokio::sync::mpsc;

use super::{Demux, WatchError, WatchEvent, Watcher, WatcherKind};
use crate::model::WorkspaceId;

/// How often dropped events are reported.
const DROP_REPORT_INTERVAL: Duration = Duration::from_mins(1);

/// The `notify` watcher: inotify on Linux, `FSEvents` on macOS, behind
/// `notify-debouncer-full` so a burst of writes arrives as one batch.
pub struct NotifyWatcher {
    debouncer: Mutex<Debouncer<RecommendedWatcher, RecommendedCache>>,
    demux: Arc<Mutex<Demux>>,
}

impl std::fmt::Debug for NotifyWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotifyWatcher")
            .field("roots", &lock(&self.demux).len())
            .finish_non_exhaustive()
    }
}

impl NotifyWatcher {
    /// Starts the watcher, with nothing watched yet. Batches arrive
    /// `debounce` after the last event in them. On Linux, logs the inotify
    /// limits first.
    pub fn new(debounce: Duration, events: mpsc::Sender<WatchEvent>) -> Result<Self, WatchError> {
        #[cfg(target_os = "linux")]
        log_inotify_limits();
        let demux = Arc::new(Mutex::new(Demux::default()));
        let handler = Handler {
            demux: Arc::clone(&demux),
            events,
            dropped: 0,
            reported_at: Instant::now(),
        };
        // Store mounts (R1.20) are symlinks; following them would watch the
        // shared store from every workspace.
        let config = notify::Config::default().with_follow_symlinks(false);
        let debouncer = new_debouncer_opt::<_, RecommendedWatcher, RecommendedCache>(
            debounce,
            None,
            handler,
            RecommendedCache::new(),
            config,
        )
        .map_err(|source| WatchError::Start {
            kind: "notify",
            source,
        })?;
        Ok(Self {
            debouncer: Mutex::new(debouncer),
            demux,
        })
    }
}

impl Watcher for NotifyWatcher {
    fn kind(&self) -> WatcherKind {
        WatcherKind::Notify
    }

    fn watch(
        &self,
        workspace: WorkspaceId,
        root: &Path,
        skip: Vec<PathBuf>,
    ) -> Result<(), WatchError> {
        let mut debouncer = lock(&self.debouncer);
        // Route first, so nothing observed between the two is lost.
        let old = lock(&self.demux).insert(workspace, root.to_owned(), skip);
        if let Some(old) = old.filter(|old| old != root) {
            let _ = debouncer.unwatch(&old);
        }
        debouncer
            .watch(root, RecursiveMode::Recursive)
            .map_err(|source| WatchError::Watch {
                path: root.to_owned(),
                source,
            })
            .inspect_err(|error| {
                lock(&self.demux).remove(workspace);
                if error.is_limit() {
                    log_limit_hit(workspace, root);
                }
            })
    }

    fn unwatch(&self, workspace: WorkspaceId) -> Result<(), WatchError> {
        let mut debouncer = lock(&self.debouncer);
        let Some(root) = lock(&self.demux).remove(workspace) else {
            return Ok(());
        };
        match debouncer.unwatch(&root) {
            Ok(()) => Ok(()),
            // The root is gone (released, or deleted under us): the OS
            // already dropped the watch.
            Err(error) if matches!(error.kind, notify::ErrorKind::WatchNotFound) => Ok(()),
            Err(source) => Err(WatchError::Watch { path: root, source }),
        }
    }
}

/// Runs on the debouncer's thread.
struct Handler {
    demux: Arc<Mutex<Demux>>,
    events: mpsc::Sender<WatchEvent>,
    dropped: u64,
    reported_at: Instant,
}

impl DebounceEventHandler for Handler {
    fn handle_event(&mut self, result: DebounceEventResult) {
        match result {
            Ok(batch) => {
                let mut earliest = None::<Instant>;
                let mut paths = Vec::new();
                for event in &batch {
                    // Reads, including the scan's own, are not changes.
                    if matches!(event.event.kind, EventKind::Access(_)) {
                        continue;
                    }
                    earliest = Some(earliest.map_or(event.time, |at| at.min(event.time)));
                    paths.extend(event.event.paths.iter().map(PathBuf::as_path));
                }
                let Some(at) = earliest else { return };
                let grouped = lock(&self.demux).group(paths);
                for (workspace, paths) in grouped {
                    let sent = self.events.try_send(WatchEvent {
                        workspace,
                        paths,
                        at,
                    });
                    if let Err(mpsc::error::TrySendError::Full(_)) = sent {
                        self.dropped += 1;
                    }
                }
            }
            Err(errors) => {
                let demux = lock(&self.demux);
                for error in errors {
                    let workspace = error.paths.iter().find_map(|path| demux.route(path));
                    if let Some(workspace) = workspace {
                        tracing::warn!(
                            workspace_id = %workspace,
                            error = %ErrorChain(&error),
                            "filesystem watcher error"
                        );
                    } else {
                        tracing::warn!(error = %ErrorChain(&error), "filesystem watcher error");
                    }
                }
            }
        }
        if self.dropped > 0 && self.reported_at.elapsed() >= DROP_REPORT_INTERVAL {
            // The fallback timer still snapshots those workspaces.
            tracing::warn!(
                dropped_count = self.dropped,
                "watch events dropped; the scheduler is behind"
            );
            self.dropped = 0;
            self.reported_at = Instant::now();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The inotify limits, or `None` where they cannot be read.
fn inotify_limits() -> Option<(u64, u64)> {
    let read = |name: &str| -> Option<u64> {
        std::fs::read_to_string(Path::new("/proc/sys/fs/inotify").join(name))
            .ok()?
            .trim()
            .parse()
            .ok()
    };
    Some((read("max_user_watches")?, read("max_user_instances")?))
}

#[cfg(target_os = "linux")]
fn log_inotify_limits() {
    match inotify_limits() {
        Some((watches, instances)) => tracing::info!(
            watches_count = watches,
            instances_count = instances,
            "inotify limits"
        ),
        None => tracing::warn!("cannot read the inotify limits"),
    }
}

fn log_limit_hit(workspace: WorkspaceId, root: &Path) {
    let (watches, instances) = inotify_limits().unwrap_or_default();
    tracing::error!(
        workspace_id = %workspace,
        path = %root.display(),
        watches_count = watches,
        instances_count = instances,
        "the watch limit is reached; raise it with `sysctl fs.inotify.max_user_watches=<n>` \
         (and max_user_instances); the workspace is snapshotted on the fallback timer only"
    );
}
