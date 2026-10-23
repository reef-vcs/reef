//! [`NotifyWatcher`]: one debounced `notify` watcher for every workspace.
//!
//! The debouncer keeps no file ID cache ([`NoCache`]; on Linux it never
//! did). The cache only stitches the two halves of an `FSEvents` rename into
//! one event, and Reef reads an event's paths only to route it to a
//! workspace by prefix, which either half does. Building it walked and
//! stat'ed every file of a new root (following symlinks, so every store
//! mounted into it too) under the lock that delivers every workspace's
//! events: 1.1 to 1.3 s inside each create of a 100,000-file workspace on
//! APFS (R1.12), and again on every rescan and every new directory (R1.29).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{
    DebounceEventHandler, DebounceEventResult, DebouncedEvent, Debouncer, NoCache,
    new_debouncer_opt,
};
use reef_lib::error::ErrorChain;
use tokio::sync::mpsc;

use super::{Demux, HealthHook, WatchError, WatchEvent, Watcher, WatcherKind};
use crate::model::WorkspaceId;

/// How often dropped events are reported.
const DROP_REPORT_INTERVAL: Duration = Duration::from_mins(1);

/// The `notify` watcher: inotify on Linux, `FSEvents` on macOS, behind
/// `notify-debouncer-full` so a burst of writes arrives as one batch.
pub struct NotifyWatcher {
    debouncer: Mutex<Debouncer<RecommendedWatcher, NoCache>>,
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
    /// `debounce` after the last event in them. A workspace whose new
    /// directory cannot be watched is reported down to `health`. On Linux,
    /// logs the inotify limits first.
    pub fn new(
        debounce: Duration,
        events: mpsc::Sender<WatchEvent>,
        health: HealthHook,
    ) -> Result<Self, WatchError> {
        #[cfg(target_os = "linux")]
        log_inotify_limits();
        let demux = Arc::new(Mutex::new(Demux::default()));
        let handler = Handler {
            demux: Arc::clone(&demux),
            events,
            health,
            dropped: 0,
            reported_at: Instant::now(),
            lost: HashSet::new(),
        };
        // Store mounts (R1.20) are symlinks; following them would watch the
        // shared store from every workspace.
        let config = notify::Config::default().with_follow_symlinks(false);
        let debouncer = new_debouncer_opt::<_, RecommendedWatcher, NoCache>(
            debounce, None, handler, NoCache, config,
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
        // Attaching is one stream restart (inotify: a watch per directory);
        // nothing to wait out.
        _defer: bool,
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
    health: HealthHook,
    dropped: u64,
    reported_at: Instant,
    /// Workspaces an event was dropped for: their next event says anything
    /// may have changed, since a narrowed scan would miss the dropped paths
    /// (R1.30).
    lost: HashSet<WorkspaceId>,
}

impl DebounceEventHandler for Handler {
    fn handle_event(&mut self, result: DebounceEventResult) {
        match result {
            Ok(batch) => {
                let events = route_batch(&lock(&self.demux), &batch);
                for mut event in events {
                    let workspace = event.workspace;
                    event.everything |= self.lost.remove(&workspace);
                    if let Err(mpsc::error::TrySendError::Full(_)) = self.events.try_send(event) {
                        self.dropped += 1;
                        self.lost.insert(workspace);
                    }
                }
            }
            Err(errors) => {
                let mut unwatched = Vec::new();
                let demux = lock(&self.demux);
                for error in errors {
                    let workspace = error.paths.iter().find_map(|path| demux.route(path));
                    if is_limit(&error) {
                        // A new subdirectory could not be watched: its
                        // subtree is covered by the fallback timer only, at
                        // the unwatched interval.
                        unwatched.extend(workspace);
                        let (watches, instances) = inotify_limits().unwrap_or_default();
                        tracing::error!(
                            workspace_id = %workspace.map(|ws| ws.to_string()).unwrap_or_default(),
                            error = %ErrorChain(&error),
                            watches_count = watches,
                            instances_count = instances,
                            "the watch limit is reached; {LIMIT_HINT}"
                        );
                        continue;
                    }
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
                drop(demux);
                for workspace in unwatched {
                    (self.health)(workspace, false);
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

/// One event per workspace a debounced batch touches.
///
/// A rescan event (inotify's queue overflowed, or `FSEvents` coalesced or
/// dropped events) makes the event of every workspace `everything`, even
/// one that also has paths in the batch (R1.30). `FSEvents` names the
/// directory to rescan, but the debouncer keeps only the latest rescan of
/// a window and drops the ones before it, whose directories may lie under
/// other workspaces or above every root; so the directory is not used.
fn route_batch(demux: &Demux, batch: &[DebouncedEvent]) -> Vec<WatchEvent> {
    let mut earliest = None::<Instant>;
    let mut paths = Vec::new();
    let mut rescan = false;
    for event in batch {
        // Reads, including the scan's own, are not changes.
        if !event.event.need_rescan() && matches!(event.event.kind, EventKind::Access(_)) {
            continue;
        }
        earliest = Some(earliest.map_or(event.time, |at| at.min(event.time)));
        if event.event.need_rescan() {
            rescan = true;
        } else {
            paths.extend(event.event.paths.iter().map(PathBuf::as_path));
        }
    }
    let Some(at) = earliest else {
        return Vec::new();
    };
    let mut grouped = demux.group(paths);
    if rescan {
        tracing::warn!("watch events were lost; rescanning every workspace");
        for workspace in demux.workspaces() {
            grouped.entry(workspace).or_default();
        }
    }
    grouped
        .into_iter()
        .map(|(workspace, paths)| WatchEvent {
            workspace,
            paths,
            everything: rescan,
            at,
        })
        .collect()
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
    if let Some((watches, instances)) = inotify_limits() {
        tracing::info!(
            watches_count = watches,
            instances_count = instances,
            "inotify limits"
        );
    } else {
        tracing::warn!("cannot read the inotify limits");
    }
}

/// What to do about a watch limit, by platform.
const LIMIT_HINT: &str = if cfg!(target_os = "linux") {
    "raise it with `sysctl fs.inotify.max_user_watches=<n>` (and max_user_instances); \
     the workspace is snapshotted on the fallback timer meanwhile"
} else {
    "raise the open file limit (`ulimit -n`); the workspace is snapshotted on the fallback \
     timer meanwhile"
};

/// Whether `error` is the operating system's watch limit (inotify's
/// `max_user_watches` or `max_user_instances`, or open files).
pub(super) fn is_limit(error: &notify::Error) -> bool {
    match &error.kind {
        notify::ErrorKind::MaxFilesWatch => true,
        notify::ErrorKind::Io(io) => matches!(
            io.raw_os_error(),
            Some(code) if code == nix::errno::Errno::ENOSPC as i32
                || code == nix::errno::Errno::EMFILE as i32
        ),
        _ => false,
    }
}

fn log_limit_hit(workspace: WorkspaceId, root: &Path) {
    let (watches, instances) = inotify_limits().unwrap_or_default();
    tracing::error!(
        workspace_id = %workspace,
        path = %root.display(),
        watches_count = watches,
        instances_count = instances,
        "the watch limit is reached; {LIMIT_HINT}"
    );
}

#[cfg(test)]
mod routing_tests {
    use notify::event::{CreateKind, Flag};
    use notify::{Event, EventKind};

    use super::*;

    fn id(n: u8) -> WorkspaceId {
        format!("{n:016x}").parse().unwrap()
    }

    fn demux() -> Demux {
        let mut demux = Demux::default();
        demux.insert(id(1), PathBuf::from("/w/a"), vec![]);
        demux.insert(id(2), PathBuf::from("/w/b"), vec![]);
        demux.insert(id(3), PathBuf::from("/w/c"), vec![]);
        demux
    }

    fn write(path: &str, at: Instant) -> DebouncedEvent {
        let event = Event::new(EventKind::Create(CreateKind::File)).add_path(PathBuf::from(path));
        DebouncedEvent::new(event, at)
    }

    fn rescan(path: Option<&str>, at: Instant) -> DebouncedEvent {
        let mut event = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        if let Some(path) = path {
            event = event.add_path(PathBuf::from(path));
        }
        DebouncedEvent::new(event, at)
    }

    /// R1.30, 2: a lost-events rescan in a batch makes every workspace's event
    /// `everything`, including one that also has paths in the batch.
    #[test]
    fn overflow_with_paths_is_everything() {
        let demux = demux();
        let at = Instant::now();
        let events = route_batch(&demux, &[write("/w/a/x.txt", at), rescan(None, at)]);
        assert_eq!(events.len(), 3, "{events:?}");
        let a = events
            .iter()
            .find(|event| event.workspace == id(1))
            .unwrap();
        assert_eq!(a.paths, [PathBuf::from("/w/a/x.txt")]);
        assert!(events.iter().all(|event| event.everything), "{events:?}");

        // FSEvents names a directory, but the debouncer keeps only the
        // latest rescan of a window: the directory is not trusted to be
        // the only one.
        let events = route_batch(&demux, &[rescan(Some("/w/b/sub"), at)]);
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(
            events
                .iter()
                .all(|event| event.everything && event.paths.is_empty())
        );

        let events = route_batch(&demux, &[write("/w/c/y.txt", at)]);
        assert_eq!(
            events,
            [WatchEvent {
                workspace: id(3),
                paths: vec![PathBuf::from("/w/c/y.txt")],
                everything: false,
                at,
            }]
        );
    }
}

#[cfg(test)]
#[cfg(target_os = "macos")]
mod tests {
    use super::*;

    /// The longest a watch may take to attach beyond `FSEvents`' own
    /// registration (R1.29).
    const ATTACH_BOUND: Duration = Duration::from_millis(50);

    fn watcher() -> (NotifyWatcher, mpsc::Receiver<WatchEvent>) {
        let (tx, rx) = mpsc::channel(64);
        (
            NotifyWatcher::new(Duration::from_millis(50), tx, Arc::new(|_, _| {})).unwrap(),
            rx,
        )
    }

    /// How long watching `root` as `id` took.
    fn timed_watch(watcher: &NotifyWatcher, id: WorkspaceId, root: &Path) -> Duration {
        let begun = Instant::now();
        watcher.watch(id, root, Vec::new(), false).unwrap();
        begun.elapsed()
    }

    /// How long a watch of an empty root takes in this process: the
    /// `FSEvents` stream restart alone, the fastest of three. Registering a
    /// stream makes fseventsd do work in proportion to the entries in the
    /// directory of the running executable (about 2 µs each on an M2 Max,
    /// macOS 27): 0.7 s for a test binary in a `target/debug/deps` of
    /// 370,000 files, about a millisecond for `~/.cargo/bin`. The bounds
    /// below are over this.
    fn registration(watcher: &NotifyWatcher, dir: &Path) -> Duration {
        (0..3_u8)
            .map(|n| {
                let empty = dir.join(format!("empty-{n}"));
                std::fs::create_dir(&empty).unwrap();
                let id: WorkspaceId = format!("00000000000000e{n}").parse().unwrap();
                timed_watch(watcher, id, &empty)
            })
            .min()
            .unwrap()
    }

    /// Writes a file under `root` until an event for `id` arrives, failing
    /// after five seconds. (`FSEvents` may not report a write made the
    /// instant a stream starts.)
    async fn delivered(events: &mut mpsc::Receiver<WatchEvent>, id: WorkspaceId, root: &Path) {
        let begun = Instant::now();
        for i in 0.. {
            std::fs::write(root.join(format!("written-{i}.txt")), "x\n").unwrap();
            let wait = tokio::time::timeout(Duration::from_millis(500), async {
                while let Some(event) = events.recv().await {
                    if event.workspace == id {
                        return true;
                    }
                }
                false
            });
            if wait.await == Ok(true) {
                return;
            }
            assert!(
                begun.elapsed() < Duration::from_secs(5),
                "no event for the workspace"
            );
        }
    }

    /// 1. The debouncer does not walk a new root: a fresh APFS clone of the
    /// `large` preset attaches in under 50 ms. Run by hand (and in the PR):
    /// `cargo test -p reef-daemon --lib notify::tests -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "generates the 100,000-file preset; run by hand to record the number"]
    #[allow(clippy::print_stderr, reason = "reports the timing")]
    async fn notify_watch_is_cheap_on_a_large_tree() {
        use reef_testing::synth::{self, Preset, Spec};

        let dir = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(dir.path()).unwrap();
        let tree = dir.join("large");
        let manifest = synth::generate(&tree, &Spec::preset(Preset::Large), 42, false).unwrap();
        let root = dir.join("clone");
        crate::backend::apfs::clonefile(&tree, &root, 0).unwrap();
        let (watcher, mut events) = watcher();
        let registration = registration(&watcher, &dir);
        let id: WorkspaceId = "00000000000000a1".parse().unwrap();
        let took = timed_watch(&watcher, id, &root);
        eprintln!(
            "notify watch of an APFS clone of large ({} files): {took:.2?}, of an empty root {registration:.2?}",
            manifest.files
        );
        assert!(
            took < registration + ATTACH_BOUND,
            "attaching took {took:?}, {registration:?} for an empty root"
        );
        delivered(&mut events, id, &root).await;
    }

    /// 4. Store mounts (symlinks on macOS, R1.20) are not followed when a
    /// watch attaches: a root with a 30,000-file store linked in attaches
    /// in under 50 ms. (The brief's 10,000 files walked in about 65 ms with
    /// the file ID cache, too close to the bound to catch it reliably.)
    #[tokio::test(flavor = "multi_thread")]
    async fn store_mounts_are_not_walked_on_watch() {
        let dir = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(dir.path()).unwrap();
        let store = dir.join("store");
        for d in 0..300 {
            let sub = store.join(format!("pkg-{d}"));
            std::fs::create_dir_all(&sub).unwrap();
            for f in 0..100 {
                std::fs::write(sub.join(format!("{f}.js")), "module.exports = 1;\n").unwrap();
            }
        }
        let root = dir.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("README.md"), "a workspace\n").unwrap();
        std::os::unix::fs::symlink(&store, root.join("node_modules")).unwrap();
        let (watcher, mut events) = watcher();
        let registration = registration(&watcher, &dir);
        let id: WorkspaceId = "00000000000000a2".parse().unwrap();
        let took = timed_watch(&watcher, id, &root);
        assert!(
            took < registration + ATTACH_BOUND,
            "attaching took {took:?}, {registration:?} for an empty root"
        );
        delivered(&mut events, id, &root).await;
    }
}
