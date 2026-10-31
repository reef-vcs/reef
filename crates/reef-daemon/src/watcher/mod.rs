//! Filesystem watchers that trigger snapshots (R1.13, R1.14). A watcher only
//! says that something under a workspace changed; the scheduler
//! ([`crate::snapshot::Scheduler`]) decides when to snapshot, and the scan
//! itself is jj's.
//!
//! There are two kinds ([`WatcherKind`]), chosen per repository by
//! [`select_watcher`] and recorded per workspace at create:
//!
//! - **notify** ([`notify::NotifyWatcher`]): one debounced watcher for every
//!   notify workspace. Its events are mapped back to workspaces by root
//!   (`Demux`): workspace roots never nest, so the root a path falls under
//!   is unambiguous. A tap on notify's own thread journals the paths before
//!   the debouncer ([`crate::snapshot::journal`], R2.23), and a watcher
//!   snapshot or a read scans only them and everything under them (R1.30,
//!   ADR 0026). Each workspace's sync directory is watched with its root.
//! - **watchman** (`watchman::WatchmanWatcher`, with the `watchman`
//!   feature): a Watchman subscription per workspace, and scans narrowed by
//!   jj's own Watchman query (ADR 0023). For repositories at or above
//!   `watcher.watchman_threshold_files` under `watcher.backend = "auto"`.
//!
//! The daemon holds at most one watcher of each kind.
//!
//! # Attaching off the create path
//!
//! Both watchers make one promise to a create (ADR 0028): a write made
//! after `Create` returns is in the workspace's first watcher snapshot,
//! whether its watch was up at the time or not. A watch that is not up yet
//! is *pending* ([`Attach::Pending`], [`PendingWatch`]); when it comes up,
//! the watcher reports it through the [`HealthHook`] and sends an
//! `everything` event, so the scheduler takes one full scan. Watchman
//! defers a new subscription until creates go quiet (R1.29). On Linux,
//! notify attaches a created workspace's inotify watches on a worker thread,
//! one workspace at a time (R1.32): one `inotify_add_watch` per directory
//! was 985 ms of a 1,006 ms `large` create on the CI runner. On macOS it
//! attaches inline, one `FSEvents` stream restart (9 ms, R1.29).

pub mod notify;
#[cfg(feature = "watchman")]
pub mod watchman;

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use reef_lib::error::{Classify, ErrorKind};
use tokio::sync::mpsc;

use crate::config::{WatcherBackend, WatcherSection};
pub use crate::model::WatcherKind;
use crate::model::WorkspaceId;
use crate::snapshot::Journals;

/// Changes under one workspace, after debouncing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchEvent {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The changed paths, absolute.
    pub paths: Vec<PathBuf>,
    /// Anything under the workspace may have changed, whatever `paths`
    /// holds: events were lost (an inotify queue overflow, `FSEvents`
    /// dropping events), or Watchman's view started over (R1.30).
    pub everything: bool,
    /// When the earliest change was observed.
    pub at: Instant,
}

/// Hears whether a workspace's watch is up (`true`) or down (`false`), from
/// a watcher that sets a watch up in the background (after
/// [`Attach::Pending`]), and from notify when a new directory in a watched
/// workspace cannot be watched (R1.23). The scheduler clears or sets
/// `attention = 'watch-failed'` from it.
pub type HealthHook = Arc<dyn Fn(WorkspaceId, bool) + Send + Sync>;

/// When the latest workspace create started (R1.29). `Manager::create`
/// stamps it; the Watchman watcher holds a just-created workspace's
/// subscription until creates have been quiet for
/// `watcher.watchman_attach_quiet`, so its crawl does not slow the next
/// create's clone.
#[derive(Debug, Default)]
pub struct CreateClock(Mutex<Option<Instant>>);

impl CreateClock {
    /// Records that a create started now.
    pub fn stamp(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
    }

    /// When the latest create started, if any has.
    pub fn last(&self) -> Option<Instant> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What [`Watcher::watch`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attach {
    /// The workspace is watched.
    Attached,
    /// The watch is being set up in the background. The outcome (and any
    /// later loss and recovery) arrives through the watcher's
    /// [`HealthHook`], and a watch that comes up is followed by an
    /// `everything` event, so nothing written meanwhile is lost.
    Pending,
}

/// Why a workspace's watch is not up yet (`watch_state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingWatch {
    /// A Watchman subscription waiting for creates to go quiet (R1.29).
    Waiting,
    /// A notify watch queued or being attached on Linux (R1.32).
    Attaching,
}

impl PendingWatch {
    /// `waiting` or `attaching`, as `watch_state` carries it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Attaching => "attaching",
        }
    }
}

/// Watches workspace roots and reports changes as [`WatchEvent`]s.
pub trait Watcher: Send + Sync {
    /// Which mechanism this is.
    fn kind(&self) -> WatcherKind;
    /// Starts watching `root`, recursively, for `workspace`. Changes under
    /// any of `skip` (absolute prefixes under `root`) are not reported.
    /// Watching a workspace again replaces its root and skip list. `defer`
    /// (a workspace just created) lets a watcher set the watch up after
    /// returning: Watchman once creates go quiet (R1.29), notify on Linux
    /// on its attach thread (R1.32).
    fn watch(
        &self,
        workspace: WorkspaceId,
        root: &Path,
        skip: Vec<PathBuf>,
        defer: bool,
    ) -> Result<Attach, WatchError>;
    /// Stops watching `workspace`. Not watching it is not an error.
    fn unwatch(&self, workspace: WorkspaceId) -> Result<(), WatchError>;
    /// Why `workspace`'s watch is not up yet, if it is pending.
    fn pending(&self, _workspace: WorkspaceId) -> Option<PendingWatch> {
        None
    }
}

/// Shared by the scheduler and notify's attach thread (R1.32): the inotify
/// turn, a test hold on background attaches, and their count. Only the test
/// methods are behind `testing`.
#[derive(Debug)]
pub struct AttachGate {
    /// One background attach or one full scan of a notify workspace at a
    /// time, on Linux ([`AttachGate::turn`]).
    turn: Arc<tokio::sync::Semaphore>,
    /// Open `AttachHold`s (a test hook): while any is, the attach thread
    /// takes no root off its queue.
    holds: AtomicUsize,
    /// Attaches queued or in flight.
    queued: AtomicUsize,
}

impl Default for AttachGate {
    fn default() -> Self {
        Self {
            turn: Arc::new(tokio::sync::Semaphore::new(1)),
            holds: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
        }
    }
}

impl AttachGate {
    /// Waits for the inotify turn: held by a background attach for its
    /// length, and by a full scan of a notify workspace on Linux for the
    /// scan's. A full scan opens every directory, and inotify reports each
    /// open twice (to the directory's watch and its parent's), about 17,000
    /// events for the `large` preset, more than the kernel's queue holds
    /// (`fs.inotify.max_queued_events`, 16,384); while an attach adds
    /// watches, notify's one thread reads none. Scans beside attaches, or
    /// twenty scans at once, overflowed it, which makes every workspace
    /// `everything`, whose full scans overflowed it again, in a loop that
    /// did not end while the workspaces lived. One at a time, notify's
    /// thread keeps up. FIFO, so neither side starves.
    pub(crate) async fn turn(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        // `None` once the notify watcher closed it (`close_turn`).
        Arc::clone(&self.turn).acquire_owned().await.ok()
    }

    /// Closes the inotify turn: waits for it end with `None`, and so does
    /// every later [`AttachGate::turn`]. The notify watcher's drop calls it.
    pub(crate) fn close_turn(&self) {
        self.turn.close();
    }

    /// Whether a hold is open.
    pub(crate) fn is_held(&self) -> bool {
        self.holds.load(Ordering::SeqCst) > 0
    }

    /// Records how many attaches are queued or in flight.
    pub(crate) fn set_queued(&self, count: usize) {
        self.queued.store(count, Ordering::SeqCst);
    }

    /// Test hook: until the guard drops, no background attach starts; one
    /// in flight finishes.
    #[cfg(any(test, feature = "testing"))]
    pub fn hold(self: &Arc<Self>) -> AttachHold {
        self.holds.fetch_add(1, Ordering::SeqCst);
        AttachHold(Arc::clone(self))
    }

    /// Test hook: how many background attaches are queued or in flight.
    #[cfg(any(test, feature = "testing"))]
    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::SeqCst)
    }
}

/// Holds background attaches; see [`AttachGate::hold`].
#[cfg(any(test, feature = "testing"))]
#[must_use = "dropping the guard lets the queued attaches run at once"]
#[derive(Debug)]
pub struct AttachHold(Arc<AttachGate>);

#[cfg(any(test, feature = "testing"))]
impl Drop for AttachHold {
    fn drop(&mut self) {
        self.0.holds.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Failure of a watcher.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WatchError {
    /// The watcher could not be created.
    #[error("start the {kind} watcher")]
    Start {
        /// `notify` or `watchman`.
        kind: &'static str,
        /// The underlying error.
        #[source]
        source: ::notify::Error,
    },
    /// A root could not be watched or unwatched.
    #[error("watch {}", path.display())]
    Watch {
        /// The root.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: ::notify::Error,
    },
    /// The configured watcher cannot run here.
    #[error("the {0} watcher is unavailable")]
    Unavailable(&'static str),
    /// The watcher's executable is not on `PATH`.
    #[error("`{0}` is not on PATH; install it or set watcher.backend to \"notify\" or \"auto\"")]
    BinaryMissing(&'static str),
    /// This build does not have the watcher's cargo feature.
    #[error("this build has no {0} support (the `{0}` cargo feature is off)")]
    FeatureDisabled(&'static str),
    /// The watcher was created outside a tokio runtime.
    #[error("the {0} watcher needs a tokio runtime")]
    NoRuntime(&'static str),
}

impl WatchError {
    /// Whether the operating system's watch limit was hit (inotify's
    /// `max_user_watches` or `max_user_instances`).
    pub fn is_limit(&self) -> bool {
        match self {
            Self::Start { source, .. } | Self::Watch { source, .. } => notify::is_limit(source),
            Self::Unavailable(_)
            | Self::BinaryMissing(_)
            | Self::FeatureDisabled(_)
            | Self::NoRuntime(_) => false,
        }
    }
}

impl Classify for WatchError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Start { .. } | Self::Watch { .. } => ErrorKind::Io,
            Self::Unavailable(_) | Self::BinaryMissing(_) | Self::FeatureDisabled(_) => {
                ErrorKind::Unavailable
            }
            Self::NoRuntime(_) => ErrorKind::Internal,
        }
    }
}

/// The watcher a repository's new workspaces get under `config`, given
/// its tracked file count (`None` if not counted yet):
///
/// - `notify`: notify.
/// - `watchman`: Watchman; [`WatchError::BinaryMissing`] if `watchman` is
///   not on `PATH`, [`WatchError::FeatureDisabled`] in a build without the
///   `watchman` feature.
/// - `auto`: Watchman if it is available and the repository has at least
///   `watcher.watchman_threshold_files` tracked files, notify otherwise (and
///   for an uncounted repository).
pub fn select_watcher(
    config: &WatcherSection,
    tracked_files: Option<u64>,
) -> Result<WatcherKind, WatchError> {
    match config.backend {
        WatcherBackend::Notify => Ok(WatcherKind::Notify),
        WatcherBackend::Watchman => watchman_available().map(|()| WatcherKind::Watchman),
        WatcherBackend::Auto => {
            let large = tracked_files.is_some_and(|count| count >= config.watchman_threshold_files);
            Ok(if large && watchman_available().is_ok() {
                WatcherKind::Watchman
            } else {
                WatcherKind::Notify
            })
        }
    }
}

/// Whether this build and machine can run the Watchman watcher.
fn watchman_available() -> Result<(), WatchError> {
    if !cfg!(feature = "watchman") {
        return Err(WatchError::FeatureDisabled("watchman"));
    }
    if !watchman_on_path() {
        return Err(WatchError::BinaryMissing("watchman"));
    }
    Ok(())
}

/// Whether an executable `watchman` is in a directory on `PATH`.
pub(crate) fn watchman_on_path() -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        std::fs::metadata(dir.join("watchman"))
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

/// A watcher of `kind`, sending to `events`, with `config.debounce`.
/// `health` hears a watch set up in the background come up or fail, and a
/// notify watch that breaks after attaching; `creates` is given to
/// Watchman, which defers new subscriptions by it; `gate` to notify, which
/// attaches created workspaces in the background on Linux unless
/// `watcher.notify_attach_background` is off.
pub fn new_watcher(
    kind: WatcherKind,
    config: &WatcherSection,
    events: mpsc::Sender<WatchEvent>,
    health: HealthHook,
    creates: Arc<CreateClock>,
    gate: Arc<AttachGate>,
    journals: Arc<Journals>,
) -> Result<Box<dyn Watcher>, WatchError> {
    match kind {
        WatcherKind::Notify => {
            drop(creates);
            Ok(Box::new(notify::NotifyWatcher::new(
                config.debounce,
                events,
                health,
                config.notify_attach_background && cfg!(target_os = "linux"),
                gate,
                journals,
            )?))
        }
        #[cfg(feature = "watchman")]
        WatcherKind::Watchman => {
            drop((gate, journals));
            Ok(Box::new(watchman::WatchmanWatcher::new(
                config.debounce,
                events,
                health,
                watchman::Deferral {
                    quiet: config.watchman_attach_quiet,
                    max: config.watchman_attach_defer_max,
                    creates,
                },
            )?))
        }
        #[cfg(not(feature = "watchman"))]
        WatcherKind::Watchman => {
            drop((events, health, creates, gate, journals));
            Err(WatchError::FeatureDisabled("watchman"))
        }
    }
}

/// Stands in when no watcher could be created: every `watch` fails, so each
/// workspace is marked `watch-failed` and lives on the fallback timer.
#[derive(Debug)]
pub struct NoWatcher {
    /// The kind that could not be created.
    pub kind: WatcherKind,
}

impl Watcher for NoWatcher {
    fn kind(&self) -> WatcherKind {
        self.kind
    }

    fn watch(
        &self,
        _: WorkspaceId,
        _: &Path,
        _: Vec<PathBuf>,
        _: bool,
    ) -> Result<Attach, WatchError> {
        Err(WatchError::Unavailable(self.kind.as_str()))
    }

    fn unwatch(&self, _: WorkspaceId) -> Result<(), WatchError> {
        Ok(())
    }
}

/// The absolute prefixes under `root` a watcher can drop events under: the
/// literal entries of `excludes` and every mount. Patterns stay with the
/// scan's ignore rules.
pub fn skip_prefixes(root: &Path, excludes: &[String], mounts: &[String]) -> Vec<PathBuf> {
    reef_lib::jj::ignore::literal_prefixes(excludes.iter().chain(mounts).map(String::as_str))
        .into_iter()
        .map(|prefix| root.join(prefix))
        .collect()
}

/// Maps paths to the workspace whose root they fall under, and cookies
/// to the workspace whose sync directory holds them (R2.23).
#[derive(Debug, Default)]
pub(crate) struct Demux {
    /// Root → (workspace, skipped prefixes).
    roots: BTreeMap<PathBuf, (WorkspaceId, Vec<PathBuf>)>,
    /// Sync directory → workspace.
    syncs: BTreeMap<PathBuf, WorkspaceId>,
}

impl Demux {
    /// Routes paths under `root` to `workspace`, and cookies in `sync` to
    /// it, replacing any root the workspace had. Returns the replaced root.
    pub(crate) fn insert(
        &mut self,
        workspace: WorkspaceId,
        root: PathBuf,
        skip: Vec<PathBuf>,
        sync: Option<PathBuf>,
    ) -> Option<PathBuf> {
        let old = self.remove(workspace);
        self.roots.insert(root, (workspace, skip));
        if let Some(sync) = sync {
            self.syncs.insert(sync, workspace);
        }
        old
    }

    /// The workspace whose sync directory `path` is directly in, with the
    /// cookie's name.
    pub(crate) fn route_cookie<'a>(&self, path: &'a Path) -> Option<(WorkspaceId, &'a str)> {
        let workspace = self.syncs.get(path.parent()?)?;
        Some((*workspace, path.file_name()?.to_str()?))
    }

    /// The sync directory routed with `root`, if any.
    pub(crate) fn sync_of(&self, root: &Path) -> Option<PathBuf> {
        let (workspace, _) = self.roots.get(root)?;
        self.syncs
            .iter()
            .find(|(_, id)| *id == workspace)
            .map(|(sync, _)| sync.clone())
    }

    /// Whether `path` is a sync directory itself.
    pub(crate) fn is_sync_dir(&self, path: &Path) -> bool {
        self.syncs.contains_key(path)
    }

    /// Whether `workspace` is routed.
    pub(crate) fn has(&self, workspace: WorkspaceId) -> bool {
        self.roots.values().any(|(id, _)| *id == workspace)
    }

    /// Stops routing to `workspace`; returns its root.
    pub(crate) fn remove(&mut self, workspace: WorkspaceId) -> Option<PathBuf> {
        self.syncs.retain(|_, id| *id != workspace);
        let root = self
            .roots
            .iter()
            .find(|(_, (id, _))| *id == workspace)
            .map(|(root, _)| root.clone())?;
        self.roots.remove(&root);
        Some(root)
    }

    /// The workspace whose root holds `path`, skipped prefixes included.
    pub(crate) fn route_root(&self, path: &Path) -> Option<WorkspaceId> {
        let (root, (workspace, _)) = self
            .roots
            .range::<Path, _>((Bound::Unbounded, Bound::Included(path)))
            .next_back()?;
        path.starts_with(root).then_some(*workspace)
    }

    /// The workspace `path` belongs to, or `None` if it is under no root or
    /// under one of its root's skipped prefixes.
    pub(crate) fn route(&self, path: &Path) -> Option<WorkspaceId> {
        // Paths order component by component, so the greatest root not after
        // `path` is the only one that can contain it: roots never nest.
        let (root, (workspace, skip)) = self
            .roots
            .range::<Path, _>((Bound::Unbounded, Bound::Included(path)))
            .next_back()?;
        if !path.starts_with(root) || skip.iter().any(|prefix| path.starts_with(prefix)) {
            return None;
        }
        Some(*workspace)
    }

    /// `paths` grouped by workspace, dropping those [`Demux::route`] does
    /// not place.
    pub(crate) fn group<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a Path>,
    ) -> BTreeMap<WorkspaceId, Vec<PathBuf>> {
        let mut grouped: BTreeMap<WorkspaceId, Vec<PathBuf>> = BTreeMap::new();
        for path in paths {
            if let Some(workspace) = self.route(path) {
                grouped.entry(workspace).or_default().push(path.to_owned());
            }
        }
        grouped
    }

    /// The workspaces routed to.
    pub(crate) fn workspaces(&self) -> Vec<WorkspaceId> {
        self.roots.values().map(|(id, _)| *id).collect()
    }

    /// How many roots are routed.
    pub(crate) fn len(&self) -> usize {
        self.roots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> WorkspaceId {
        format!("{n:016x}").parse().unwrap()
    }

    #[test]
    fn routes_to_the_containing_root() {
        let mut demux = Demux::default();
        demux.insert(id(1), PathBuf::from("/s/ws/a1"), vec![], None);
        demux.insert(id(2), PathBuf::from("/s/ws/a10"), vec![], None);
        demux.insert(
            id(3),
            PathBuf::from("/s/ws/b"),
            vec![PathBuf::from("/s/ws/b/target")],
            Some(PathBuf::from("/s/sync/b")),
        );
        assert_eq!(
            demux.route_cookie(Path::new("/s/sync/b/1-2")),
            Some((id(3), "1-2"))
        );
        assert_eq!(demux.route_cookie(Path::new("/s/sync/b/x/1-2")), None);
        assert!(demux.is_sync_dir(Path::new("/s/sync/b")));
        assert_eq!(demux.route(Path::new("/s/ws/a1/x/y.rs")), Some(id(1)));
        assert_eq!(demux.route(Path::new("/s/ws/a1")), Some(id(1)));
        assert_eq!(demux.route(Path::new("/s/ws/a10/z")), Some(id(2)));
        assert_eq!(demux.route(Path::new("/s/ws/a2/z")), None);
        assert_eq!(demux.route(Path::new("/s/ws")), None);
        assert_eq!(demux.route(Path::new("/s/ws/b/src/lib.rs")), Some(id(3)));
        assert_eq!(demux.route(Path::new("/s/ws/b/target/debug/x")), None);
        assert_eq!(demux.route(Path::new("/s/ws/b/target")), None);
        assert_eq!(
            demux.route(Path::new("/s/ws/b/targets/x")),
            Some(id(3)),
            "a skip is a path prefix, not a string prefix"
        );
    }

    #[test]
    fn insert_replaces_and_remove_forgets() {
        let mut demux = Demux::default();
        demux.insert(id(1), PathBuf::from("/a"), vec![], None);
        assert_eq!(
            demux.insert(id(1), PathBuf::from("/b"), vec![], None),
            Some(PathBuf::from("/a"))
        );
        assert_eq!(demux.len(), 1);
        assert_eq!(demux.route(Path::new("/a/x")), None);
        assert_eq!(demux.remove(id(1)), Some(PathBuf::from("/b")));
        assert_eq!(demux.remove(id(1)), None);
        assert_eq!(demux.len(), 0);
    }

    #[test]
    fn group_by_workspace() {
        let mut demux = Demux::default();
        demux.insert(id(1), PathBuf::from("/a"), vec![], None);
        demux.insert(id(2), PathBuf::from("/b"), vec![], None);
        let paths = [
            Path::new("/a/1"),
            Path::new("/b/1"),
            Path::new("/c/1"),
            Path::new("/a/2"),
        ];
        let grouped = demux.group(paths);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[&id(1)].len(), 2);
        assert_eq!(grouped[&id(2)], [PathBuf::from("/b/1")]);
    }

    #[test]
    fn selection_by_backend_and_threshold() {
        let mut config = WatcherSection {
            watchman_threshold_files: 100,
            ..WatcherSection::default()
        };
        config.backend = WatcherBackend::Notify;
        assert_eq!(
            select_watcher(&config, Some(1_000_000)).unwrap(),
            WatcherKind::Notify
        );
        config.backend = WatcherBackend::Auto;
        assert_eq!(
            select_watcher(&config, Some(99)).unwrap(),
            WatcherKind::Notify
        );
        assert_eq!(
            select_watcher(&config, None).unwrap(),
            WatcherKind::Notify,
            "an uncounted repository is below the threshold"
        );
        let available = watchman_available().is_ok();
        let large = if available {
            WatcherKind::Watchman
        } else {
            WatcherKind::Notify
        };
        assert_eq!(select_watcher(&config, Some(100)).unwrap(), large);
        config.backend = WatcherBackend::Watchman;
        assert_eq!(select_watcher(&config, None).is_ok(), available);
    }

    #[test]
    fn skip_prefixes_are_literal_and_absolute() {
        let excludes = ["target/".to_owned(), "*.log".to_owned()];
        let mounts = ["node_modules".to_owned()];
        assert_eq!(
            skip_prefixes(Path::new("/w"), &excludes, &mounts),
            [PathBuf::from("/w/target"), PathBuf::from("/w/node_modules")]
        );
    }
}
