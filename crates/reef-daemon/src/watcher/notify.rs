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
//!
//! # Background attaches (R1.32, ADR 0028)
//!
//! On Linux, a deferred watch (a workspace just created) is queued and
//! attached by one worker thread, `reef-notify-attach`, in queue order.
//! inotify adds one watch per directory, on notify's single event-loop
//! thread, which reads no events meanwhile: for the `large` preset's 8,336
//! directories that was 985 ms of a 1,006 ms create on the CI runner. When
//! an attach succeeds the worker reports the workspace up and sends an
//! `everything` event, so the scheduler's next snapshot is a full scan and
//! nothing written before the watch existed is missed. Attaches stay serial:
//! the daemon has one inotify instance, and notify's thread does one at a
//! time whoever asks. On macOS a watch is one `FSEvents` stream restart
//! (9 ms, R1.29) and attaches inline, so a create there takes no
//! post-attach full scan.
//!
//! # The tap and sync directories (R2.23, ADR 0048)
//!
//! [`ChangesOnly`] journals every change's paths on notify's own thread,
//! before the debouncer ([`crate::snapshot::journal`]), and recognises the
//! sync cookies a read writes: each workspace's sync directory, outside its
//! root, is watched with it, non-recursively, so a cookie's event orders
//! after every earlier write's. On macOS each root is its own `FSEvents`
//! stream, over the root and its sync directory: notify's one stream over
//! every root restarted at each watch and unwatch with `since_when = now`,
//! so a create or a release silently dropped other workspaces' events.
//!
//! The worker attaches under the debouncer's lock, as an inline watch and
//! [`Watcher::unwatch`] do, and takes a root off the queue only while it
//! holds that lock: an unwatch of a workspace being attached waits for that
//! one attach, and one still queued is taken off the queue. The queue has
//! its own small lock, so queueing a create's watch never waits behind an
//! attach in flight (each create of a burst would otherwise wait for the
//! previous one's).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode, DataChange, ModifyKind};
use notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{
    DebounceEventHandler, DebounceEventResult, DebouncedEvent, Debouncer, NoCache,
    new_debouncer_opt,
};
use reef_lib::error::ErrorChain;
use tokio::sync::mpsc;

use super::{
    Attach, AttachGate, Demux, HealthHook, PendingWatch, WatchError, WatchEvent, Watcher,
    WatcherKind,
};
use crate::model::WorkspaceId;
use crate::snapshot::Journals;
use crate::snapshot::journal::Change;

/// How long a new macOS stream may take to deliver its first cookie before
/// its watch counts as failed.
#[cfg(target_os = "macos")]
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);

/// How often dropped events are reported.
const DROP_REPORT_INTERVAL: Duration = Duration::from_mins(1);

/// How often the attach thread looks again while its gate is held, and
/// while the events channel is full.
const WORKER_POLL: Duration = Duration::from_millis(10);

/// The `notify` watcher: inotify on Linux, `FSEvents` on macOS, behind
/// `notify-debouncer-full` so a burst of writes arrives as one batch.
pub struct NotifyWatcher {
    shared: Arc<Shared>,
    /// Deferred watches go to the attach thread.
    background: bool,
    worker: Option<JoinHandle<()>>,
}

/// What the watcher and its attach thread share.
struct Shared {
    /// Taken for every attach and unwatch, and held across the attach.
    /// Taken before `queue`, never after.
    debouncer: Mutex<Debouncer<ChangesOnly, NoCache>>,
    /// Deferred watches: never held for long, so a create queueing its
    /// watch and `pending` never wait behind an attach.
    queue: Mutex<Queue>,
    demux: Arc<Mutex<Demux>>,
    /// Wakes the attach thread (with `queue`): a root queued, or the
    /// watcher dropped.
    wake: Condvar,
    stop: AtomicBool,
    events: mpsc::Sender<WatchEvent>,
    health: HealthHook,
    gate: Arc<AttachGate>,
    journals: Arc<Journals>,
}

#[derive(Default)]
struct Queue {
    /// Deferred watches not yet taken by the attach thread, oldest first.
    waiting: VecDeque<Queued>,
    /// Workspaces queued or being attached.
    pending: HashSet<WorkspaceId>,
    /// Workspaces the attach thread attached and has not reported yet. An
    /// inline watch or an unwatch in between takes a workspace out, and its
    /// report is then dropped: it would undo what that call did.
    unreported: HashSet<WorkspaceId>,
    /// `everything` events of attached workspaces, held until the queue is
    /// empty (see [`Shared::release_scans`]).
    scans: Vec<WatchEvent>,
}

impl Queue {
    /// Drops `workspace`'s queued watch, if any, its pending mark, and any
    /// report not yet made: an inline watch or an unwatch supersedes them.
    fn cancel(&mut self, workspace: WorkspaceId, gate: &AttachGate) {
        self.waiting.retain(|queued| queued.workspace != workspace);
        self.pending.remove(&workspace);
        self.unreported.remove(&workspace);
        gate.set_queued(self.pending.len());
    }
}

/// A deferred watch.
struct Queued {
    workspace: WorkspaceId,
    root: PathBuf,
    skip: Vec<PathBuf>,
    /// When it was queued.
    at: Instant,
}

/// An attach the thread made.
struct Attached {
    queued: Queued,
    /// From queueing to the attach's start.
    waited: Duration,
    /// The attach itself.
    took: Duration,
    /// Attaches still queued or in flight after it.
    left: usize,
    outcome: Result<(), WatchError>,
}

impl std::fmt::Debug for NotifyWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotifyWatcher")
            .field("roots", &lock(&self.shared.demux).len())
            .field("pending", &lock(&self.shared.queue).pending.len())
            .field("background", &self.background)
            .finish_non_exhaustive()
    }
}

impl NotifyWatcher {
    /// Starts the watcher, with nothing watched yet. Batches arrive
    /// `debounce` after the last event in them. A workspace whose new
    /// directory cannot be watched is reported down to `health`. With
    /// `background`, deferred watches are attached on the attach thread
    /// (held while `gate` is) and reported to `health`. On Linux, logs the
    /// inotify limits first. Every change is journaled in `journals` before
    /// the debouncer (see the module docs).
    pub fn new(
        debounce: Duration,
        events: mpsc::Sender<WatchEvent>,
        health: HealthHook,
        background: bool,
        gate: Arc<AttachGate>,
        journals: Arc<Journals>,
    ) -> Result<Self, WatchError> {
        #[cfg(target_os = "linux")]
        log_inotify_limits();
        let demux = Arc::new(Mutex::new(Demux::default()));
        let handler = Handler {
            demux: Arc::clone(&demux),
            events: events.clone(),
            health: Arc::clone(&health),
            dropped: 0,
            reported_at: Instant::now(),
        };
        // Store mounts (R1.20) are symlinks; following them would watch the
        // shared store from every workspace.
        let config = notify::Config::default().with_follow_symlinks(false);
        // The debouncer builds the watcher itself, on this thread.
        NEXT_TAP.with(|next| {
            *next.borrow_mut() = Some(Tap {
                demux: Arc::clone(&demux),
                journals: Arc::clone(&journals),
            });
        });
        let debouncer =
            new_debouncer_opt::<_, ChangesOnly, NoCache>(debounce, None, handler, NoCache, config)
                .map_err(|source| WatchError::Start {
                    kind: "notify",
                    source,
                });
        NEXT_TAP.with(|next| next.borrow_mut().take());
        let debouncer = debouncer?;
        let shared = Arc::new(Shared {
            debouncer: Mutex::new(debouncer),
            queue: Mutex::new(Queue::default()),
            demux,
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            events,
            health,
            gate,
            journals,
        });
        let worker = if background {
            let shared = Arc::clone(&shared);
            let spawned = std::thread::Builder::new()
                .name("reef-notify-attach".to_owned())
                .spawn(move || shared.run_attaches());
            match spawned {
                Ok(worker) => Some(worker),
                Err(error) => {
                    tracing::warn!(
                        error = %ErrorChain(&error),
                        "cannot start the watch attach thread; new workspaces attach inline"
                    );
                    None
                }
            }
        } else {
            None
        };
        Ok(Self {
            shared,
            background: worker.is_some(),
            worker,
        })
    }
}

impl Drop for NotifyWatcher {
    fn drop(&mut self) {
        {
            // Under the lock, so the thread cannot miss the wake-up.
            let _queue = lock(&self.shared.queue);
            self.shared.stop.store(true, Ordering::SeqCst);
            self.shared.wake.notify_all();
        }
        // Ends a wait for the inotify turn; scans that ask for it from now
        // on (the daemon is stopping) run without it.
        self.shared.gate.close_turn();
        // Waits for an attach in flight.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
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
        defer: bool,
    ) -> Result<Attach, WatchError> {
        if self.background && defer {
            let mut queue = lock(&self.shared.queue);
            queue.waiting.retain(|queued| queued.workspace != workspace);
            queue.waiting.push_back(Queued {
                workspace,
                root: root.to_owned(),
                skip,
                at: Instant::now(),
            });
            queue.pending.insert(workspace);
            self.shared.gate.set_queued(queue.pending.len());
            self.shared.wake.notify_one();
            return Ok(Attach::Pending);
        }
        // Not under the inotify turn: inline attaches are rare (daemon start,
        // a move's rewatch, creates with `notify_attach_background` off), and
        // a move's rewatch would otherwise wait behind every full scan
        // queued for the turn.
        let mut debouncer = lock(&self.shared.debouncer);
        lock(&self.shared.queue).cancel(workspace, &self.shared.gate);
        self.shared.wake.notify_one();
        attach(
            &mut debouncer,
            &self.shared.demux,
            &self.shared.journals,
            workspace,
            root,
            skip,
        )?;
        // A stream that failed to start says nothing (notify drops the
        // error): see one cookie through it first.
        #[cfg(target_os = "macos")]
        if !self.shared.journals.confirm(workspace, CONFIRM_TIMEOUT) {
            if let Some(root) = lock(&self.shared.demux).remove(workspace) {
                let _ = debouncer.unwatch(&root);
            }
            return Err(WatchError::Watch {
                path: root.to_owned(),
                source: notify::Error::generic("the FSEvents stream delivered no event"),
            });
        }
        Ok(Attach::Attached)
    }

    fn unwatch(&self, workspace: WorkspaceId) -> Result<(), WatchError> {
        {
            // Still queued, and never attached: the attach thread pops only
            // under the queue's lock, so it cannot attach it after this, and
            // nothing waits for another workspace's attach.
            let mut queue = lock(&self.shared.queue);
            if queue
                .waiting
                .iter()
                .any(|queued| queued.workspace == workspace)
                && !lock(&self.shared.demux).has(workspace)
            {
                queue.cancel(workspace, &self.shared.gate);
                // An emptied queue releases the held attach scans.
                self.shared.wake.notify_one();
                return Ok(());
            }
        }
        // Waits for an attach in flight.
        let mut debouncer = lock(&self.shared.debouncer);
        lock(&self.shared.queue).cancel(workspace, &self.shared.gate);
        self.shared.wake.notify_one();
        let Some(root) = lock(&self.shared.demux).remove(workspace) else {
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

    fn pending(&self, workspace: WorkspaceId) -> Option<PendingWatch> {
        lock(&self.shared.queue)
            .pending
            .contains(&workspace)
            .then_some(PendingWatch::Attaching)
    }
}

impl Shared {
    /// The attach thread: attaches queued roots in order until the
    /// watcher is dropped.
    fn run_attaches(&self) {
        while let Some(attached) = self.attach_next() {
            self.report(attached);
            self.release_scans();
        }
    }

    /// Waits for a queued root and attaches it; `None` once the watcher is
    /// dropped. The root is taken off the queue under the debouncer's lock,
    /// which the attach holds to the end, so an unwatch either finds it
    /// still queued or waits for its attach.
    fn attach_next(&self) -> Option<Attached> {
        loop {
            self.wait_for_queued()?;
            // Not beside a full scan (see `AttachGate::turn`); held to the
            // end of the attach.
            // FIFO with the scans waiting for it; the watcher's drop closes
            // it, which ends the wait.
            let _turn = pollster::block_on(self.gate.turn())?;
            let mut debouncer = lock(&self.debouncer);
            // An unwatch may have emptied the queue meanwhile.
            let Some(mut queued) = lock(&self.queue).waiting.pop_front() else {
                continue;
            };
            let begun = Instant::now();
            let outcome = attach(
                &mut debouncer,
                &self.demux,
                &self.journals,
                queued.workspace,
                &queued.root,
                std::mem::take(&mut queued.skip),
            );
            let took = begun.elapsed();
            let left = {
                let mut queue = lock(&self.queue);
                queue.pending.remove(&queued.workspace);
                queue.unreported.insert(queued.workspace);
                self.gate.set_queued(queue.pending.len());
                queue.pending.len()
            };
            drop(debouncer);
            return Some(Attached {
                waited: begun.saturating_duration_since(queued.at),
                took,
                left,
                queued,
                outcome,
            });
        }
    }

    /// Waits until a root is queued and the gate is open; `None` once the
    /// watcher is dropped. Sends the held attach scans whenever the queue is
    /// empty, also when an unwatch emptied it.
    fn wait_for_queued(&self) -> Option<()> {
        let mut queue = lock(&self.queue);
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return None;
            }
            let held = self.gate.is_held();
            if !held && !queue.waiting.is_empty() {
                return Some(());
            }
            if queue.waiting.is_empty() && !queue.scans.is_empty() {
                let scans = std::mem::take(&mut queue.scans);
                drop(queue);
                for event in scans {
                    self.send(event);
                }
                queue = lock(&self.queue);
                continue;
            }
            queue = if held {
                // A hold's drop does not wake the thread.
                self.wake
                    .wait_timeout(queue, WORKER_POLL)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            } else {
                self.wake
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner)
            };
        }
    }

    /// Reports an attach the thread made, outside the debouncer's lock: a
    /// full scan follows one that came up.
    fn report(&self, attached: Attached) {
        let Attached {
            queued:
                Queued {
                    workspace,
                    root,
                    at,
                    ..
                },
            waited,
            took,
            left,
            outcome,
        } = attached;
        match outcome {
            Ok(()) => {
                tracing::info!(
                    workspace_id = %workspace,
                    path = %root.display(),
                    queued_ms = millis(waited),
                    attach_ms = millis(took),
                    queued_count = left,
                    "workspace watch attached"
                );
                if self.report_health(workspace, true) {
                    // Strictly after the watch exists: a scan that ran before
                    // it cannot vouch for a write made after. `at` is the
                    // queue time, so the snapshot's latency shows the wait.
                    lock(&self.queue).scans.push(WatchEvent {
                        workspace,
                        paths: Vec::new(),
                        everything: true,
                        at,
                    });
                }
            }
            Err(error) => {
                tracing::warn!(
                    workspace_id = %workspace,
                    path = %root.display(),
                    watcher = "notify",
                    error = %ErrorChain(&error),
                    "cannot watch the workspace; it is snapshotted on the fallback timer only"
                );
                self.report_health(workspace, false);
            }
        }
    }

    /// Sends the held `everything` events once no attach is queued. A full
    /// scan of a `large` workspace opens its 8,336 directories, and inotify
    /// reports each open; while notify's thread adds the next root's
    /// watches it reads no events, so two such scans overflowed the kernel's
    /// 16,384-event queue, which makes every workspace `everything`, whose
    /// scans overflowed it again: about 30 full scans of each workspace
    /// during a burst of twenty in a Docker VM, against one. Until its scan
    /// the workspace is `behind`, so any scan of it is full, and the
    /// fallback timer still runs.
    fn release_scans(&self) {
        let scans = {
            let mut queue = lock(&self.queue);
            if !queue.waiting.is_empty() {
                return;
            }
            std::mem::take(&mut queue.scans)
        };
        for event in scans {
            self.send(event);
        }
    }

    /// Reports `workspace` up or down unless an inline watch or an unwatch
    /// came after its attach (see [`Queue::unreported`]); whether it did.
    /// Under the queue's lock, so neither can slip in between the check and
    /// the report.
    fn report_health(&self, workspace: WorkspaceId, up: bool) -> bool {
        let mut queue = lock(&self.queue);
        if !queue.unreported.remove(&workspace) {
            return false;
        }
        (self.health)(workspace, up);
        true
    }

    /// Sends `event`, waiting while the channel is full: never dropped
    /// while the watcher lives, unlike the debouncer's events. Gives up
    /// once the scheduler is gone or the watcher is dropped.
    fn send(&self, mut event: WatchEvent) {
        loop {
            match self.events.try_send(event) {
                Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => return,
                Err(mpsc::error::TrySendError::Full(back)) => {
                    if self.stop.load(Ordering::SeqCst) {
                        return;
                    }
                    event = back;
                    std::thread::sleep(WORKER_POLL);
                }
            }
        }
    }
}

/// Watches `root` for `workspace`, with its sync directory, for an inline
/// watch and the attach thread alike, so the two cannot drift. Routes
/// first, so nothing observed between the two is lost.
fn attach(
    debouncer: &mut Debouncer<ChangesOnly, NoCache>,
    demux: &Mutex<Demux>,
    journals: &Journals,
    workspace: WorkspaceId,
    root: &Path,
    skip: Vec<PathBuf>,
) -> Result<(), WatchError> {
    let sync = journals.sync_dir(workspace);
    let old = lock(demux).insert(workspace, root.to_owned(), skip, sync);
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
            lock(demux).remove(workspace);
            if error.is_limit() {
                log_limit_hit(workspace, root);
            }
        })
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The platform's watcher with read events dropped on notify's own thread,
/// before the debouncer sees them (R1.32), and every change journaled there
/// (`Tap`, R2.23). inotify reports every open of a
/// watched directory or file, so a full scan of the `large` preset is about
/// 17,000 of them. The debouncer keeps each in a map under the lock notify's
/// thread needs to hand it the next event, and rebuilds the map on every
/// tick: during a few concurrent scans that thread fell behind, the kernel's
/// queue overflowed, and every workspace was rescanned in full, which
/// overflowed it again, in a loop that did not end while the workspaces
/// lived. Reads are not changes (`route_batch` drops them too), and a
/// dropped read costs only its parsing. A close after writing is kept, as a
/// modify: it is the only event a write through a memory mapping produces
/// on Linux.
///
/// Each watched root comes with its sync directory, watched non-recursively
/// beside it: in the one inotify instance on Linux, and on macOS in the
/// root's own `FSEvents` stream, one stream per root (see the module docs).
pub struct ChangesOnly {
    #[cfg(not(target_os = "macos"))]
    watcher: RecommendedWatcher,
    /// One stream per root.
    #[cfg(target_os = "macos")]
    streams: HashMap<PathBuf, notify::FsEventWatcher>,
    /// What every stream calls.
    #[cfg(target_os = "macos")]
    handler: Arc<Mutex<dyn notify::EventHandler>>,
    #[cfg(target_os = "macos")]
    config: notify::Config,
    tap: Option<Tap>,
    /// Root → the sync directory watched with it.
    syncs: HashMap<PathBuf, PathBuf>,
}

thread_local! {
    /// The tap [`NotifyWatcher::new`] hands the [`ChangesOnly`] the
    /// debouncer builds, on the same thread.
    static NEXT_TAP: RefCell<Option<Tap>> = const { RefCell::new(None) };
}

/// What notify's thread tells the journal, before the debouncer (R2.23).
#[derive(Clone)]
struct Tap {
    demux: Arc<Mutex<Demux>>,
    journals: Arc<Journals>,
}

impl Tap {
    /// Journals `event` and marks the cookies it names seen; whether to
    /// forward it to the debouncer: not when it names only cookies, sync
    /// directories, or paths a test hook drops.
    fn see(&self, event: &notify::Event) -> bool {
        if event.need_rescan() {
            self.rescan(event);
            return true;
        }
        let change = change_of(event);
        let mut routed = Vec::with_capacity(event.paths.len());
        let mut cookies = Vec::new();
        let mut forward = false;
        {
            let demux = lock(&self.demux);
            for path in &event.paths {
                if let Some((workspace, name)) = demux.route_cookie(path) {
                    cookies.push((workspace, name.to_owned()));
                } else if !demux.is_sync_dir(path) && !self.journals.drops(path) {
                    forward = true;
                    routed.extend(demux.route(path).map(|workspace| (workspace, path)));
                }
            }
        }
        // Changes first: a cookie in the same event orders after them.
        for (workspace, path) in routed {
            let change = if change == Change::NewDir && !path.is_dir() {
                Change::Any
            } else {
                change
            };
            self.journals.record(workspace, path, change);
        }
        for (workspace, name) in cookies {
            self.journals.seen_cookie(workspace, &name);
        }
        forward
    }

    /// Lost events: on Linux every notify workspace's (one queue serves
    /// them all); on macOS the workspace whose root or sync directory holds
    /// the rescan's path, since each stream is one workspace's, or every
    /// one's when it is under none.
    fn rescan(&self, event: &notify::Event) {
        let workspace = if cfg!(target_os = "macos") {
            let demux = lock(&self.demux);
            // By root, skipped prefixes included: a rescan in one
            // workspace's build directory is that workspace's.
            event.paths.first().and_then(|path| {
                demux
                    .route_root(path)
                    .or_else(|| demux.route_cookie(path).map(|(workspace, _)| workspace))
            })
        } else {
            None
        };
        match workspace {
            Some(workspace) => self.journals.lost(workspace),
            None => self.journals.lost_all_notify(),
        }
    }

    /// The sync directory routed with `root`, if it has one.
    fn sync_of(&self, root: &Path) -> Option<PathBuf> {
        lock(&self.demux).sync_of(root)
    }
}

/// What `event` changed, for the journal. A directory created or moved in
/// matters on Linux only, where notify watches it after the fact; the
/// caller checks it is one.
fn change_of(event: &notify::Event) -> Change {
    use notify::event::{CreateKind, RenameMode};
    match event.kind {
        EventKind::Modify(ModifyKind::Metadata(_)) => Change::Metadata,
        EventKind::Create(CreateKind::Folder | CreateKind::Any)
        | EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Both))
            if cfg!(target_os = "linux") =>
        {
            Change::NewDir
        }
        _ => Change::Any,
    }
}

/// Forwards every event but reads, after the tap; see [`ChangesOnly`].
struct DropReads<F> {
    handler: F,
    tap: Option<Tap>,
}

impl<F: notify::EventHandler> notify::EventHandler for DropReads<F> {
    fn handle_event(&mut self, mut event: notify::Result<notify::Event>) {
        if let (Err(error), Some(tap)) = (&event, &self.tap)
            && is_limit(error)
        {
            // notify stops adding the batch's new watches at the first
            // limit error, whichever workspaces they were in.
            tap.journals.lost_all_notify();
        }
        if let Ok(event) = &mut event {
            let close_write = matches!(
                event.kind,
                EventKind::Access(AccessKind::Close(AccessMode::Write))
            );
            if matches!(event.kind, EventKind::Access(_)) && !close_write && !event.need_rescan() {
                return;
            }
            if let Some(tap) = &self.tap
                && !tap.see(event)
            {
                return;
            }
            if close_write && !event.need_rescan() {
                event.kind = EventKind::Modify(ModifyKind::Data(DataChange::Any));
            }
        }
        self.handler.handle_event(event);
    }
}

/// One handler behind every macOS stream.
#[cfg(target_os = "macos")]
struct SharedHandler(Arc<Mutex<dyn notify::EventHandler>>);

#[cfg(target_os = "macos")]
impl notify::EventHandler for SharedHandler {
    fn handle_event(&mut self, event: notify::Result<notify::Event>) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .handle_event(event);
    }
}

impl ChangesOnly {
    /// The sync directory to watch with `root`.
    fn sync_of(&self, root: &Path) -> Option<PathBuf> {
        self.tap.as_ref().and_then(|tap| tap.sync_of(root))
    }
}

impl notify::Watcher for ChangesOnly {
    fn new<F: notify::EventHandler>(handler: F, config: notify::Config) -> notify::Result<Self> {
        let tap = NEXT_TAP.with(|next| next.borrow_mut().take());
        let handler = DropReads {
            handler,
            tap: tap.clone(),
        };
        #[cfg(target_os = "macos")]
        let watcher = Self {
            streams: HashMap::new(),
            handler: Arc::new(Mutex::new(handler)),
            config,
            tap,
            syncs: HashMap::new(),
        };
        #[cfg(not(target_os = "macos"))]
        let watcher = Self {
            watcher: RecommendedWatcher::new(handler, config)?,
            tap,
            syncs: HashMap::new(),
        };
        Ok(watcher)
    }

    #[cfg(not(target_os = "macos"))]
    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> notify::Result<()> {
        self.watcher.watch(path, recursive_mode)?;
        if let Some(sync) = self.sync_of(path) {
            self.watcher.watch(&sync, RecursiveMode::NonRecursive)?;
            self.syncs.insert(path.to_owned(), sync);
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> notify::Result<()> {
        // A root watched again gets a new stream; the old one stops first.
        self.streams.remove(path);
        let handler = SharedHandler(Arc::clone(&self.handler));
        let mut stream = notify::FsEventWatcher::new(handler, self.config)?;
        let sync = self.sync_of(path);
        {
            // Both paths before the stream starts: it starts once.
            let mut paths = stream.paths_mut();
            paths.add(path, recursive_mode)?;
            if let Some(sync) = &sync {
                paths.add(sync, RecursiveMode::NonRecursive)?;
            }
            paths.commit()?;
        }
        self.streams.insert(path.to_owned(), stream);
        if let Some(sync) = sync {
            self.syncs.insert(path.to_owned(), sync);
        }
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        let unwatched = self.watcher.unwatch(path);
        if let Some(sync) = self.syncs.remove(path) {
            let _ = self.watcher.unwatch(&sync);
        }
        unwatched
    }

    #[cfg(target_os = "macos")]
    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.syncs.remove(path);
        // Dropping a stream stops it.
        self.streams
            .remove(path)
            .map(drop)
            .ok_or_else(notify::Error::watch_not_found)
    }

    #[cfg(not(target_os = "macos"))]
    fn configure(&mut self, option: notify::Config) -> notify::Result<bool> {
        self.watcher.configure(option)
    }

    #[cfg(target_os = "macos")]
    fn configure(&mut self, option: notify::Config) -> notify::Result<bool> {
        // Applies to streams started from now on.
        self.config = option;
        Ok(false)
    }

    fn kind() -> notify::WatcherKind {
        RecommendedWatcher::kind()
    }
}

/// Runs on the debouncer's thread.
struct Handler {
    demux: Arc<Mutex<Demux>>,
    events: mpsc::Sender<WatchEvent>,
    health: HealthHook,
    dropped: u64,
    reported_at: Instant,
}

impl DebounceEventHandler for Handler {
    fn handle_event(&mut self, result: DebounceEventResult) {
        match result {
            Ok(batch) => {
                let events = route_batch(&lock(&self.demux), &batch);
                for event in events {
                    // The journal has its paths already (the tap): only the
                    // watcher snapshot waits for the next event or the timer.
                    if let Err(mpsc::error::TrySendError::Full(_)) = self.events.try_send(event) {
                        self.dropped += 1;
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
    use crate::snapshot::WalkReason;

    fn id(n: u8) -> WorkspaceId {
        format!("{n:016x}").parse().unwrap()
    }

    fn demux() -> Demux {
        let mut demux = Demux::default();
        demux.insert(id(1), PathBuf::from("/w/a"), vec![], None);
        demux.insert(id(2), PathBuf::from("/w/b"), vec![], None);
        demux.insert(id(3), PathBuf::from("/w/c"), vec![], None);
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

    /// R1.32: reads never reach the debouncer; changes and a rescan (even
    /// one carrying an access kind) do.
    #[test]
    fn reads_are_dropped_before_the_debouncer() {
        use notify::EventHandler as _;
        use notify::event::{AccessKind, AccessMode, ModifyKind};

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let mut handler = DropReads {
            handler: move |event: notify::Result<Event>| {
                lock(&sink).push(event.unwrap().kind);
            },
            tap: None,
        };
        let event = |kind| Event::new(kind).add_path(PathBuf::from("/w/a/x"));
        handler.handle_event(Ok(event(EventKind::Access(AccessKind::Open(
            AccessMode::Any,
        )))));
        handler.handle_event(Ok(event(EventKind::Access(AccessKind::Read))));
        handler.handle_event(Ok(event(EventKind::Modify(ModifyKind::Any))));
        handler.handle_event(Ok(
            event(EventKind::Access(AccessKind::Any)).set_flag(Flag::Rescan)
        ));
        assert_eq!(
            *lock(&seen),
            [
                EventKind::Modify(ModifyKind::Any),
                EventKind::Access(AccessKind::Any)
            ]
        );
    }

    /// R2.23, 20: a close after writing reaches the debouncer as a modify;
    /// the tap journals changes, takes cookies for itself, and marks
    /// rescans lost per platform.
    #[test]
    fn the_tap_journals_before_the_debouncer() {
        use notify::EventHandler as _;
        use notify::event::{AccessKind, AccessMode, ModifyKind};

        let journals = Arc::new(Journals::new(100));
        let mut demux = Demux::default();
        demux.insert(
            id(1),
            PathBuf::from("/w/a"),
            vec![],
            Some(PathBuf::from("/s/a")),
        );
        demux.insert(
            id(2),
            PathBuf::from("/w/b"),
            vec![],
            Some(PathBuf::from("/s/b")),
        );
        for (n, root) in [(1, "/w/a"), (2, "/w/b")] {
            let sync = root.replace("/w/", "/s/");
            journals.track(
                id(n),
                PathBuf::from(root),
                true,
                Some(PathBuf::from(sync)),
                None,
            );
            journals.set_watch(id(n), true);
        }
        let tap = Tap {
            demux: Arc::new(Mutex::new(demux)),
            journals: Arc::clone(&journals),
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let mut handler = DropReads {
            handler: move |event: notify::Result<Event>| {
                lock(&sink).push(event.unwrap().kind);
            },
            tap: Some(tap),
        };
        let event = |kind, path: &str| Event::new(kind).add_path(PathBuf::from(path));
        handler.handle_event(Ok(event(
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            "/w/a/mapped.bin",
        )));
        handler.handle_event(Ok(event(
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            "/w/a/read.txt",
        )));
        let cookie = format!("/s/a/{}-1", std::process::id());
        handler.handle_event(Ok(event(EventKind::Create(CreateKind::File), &cookie)));
        handler.handle_event(Ok(event(
            EventKind::Modify(ModifyKind::Metadata(notify::event::MetadataKind::Any)),
            "/w/b",
        )));
        assert_eq!(
            *lock(&seen),
            [
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                EventKind::Modify(ModifyKind::Metadata(notify::event::MetadataKind::Any)),
            ],
            "the cookie and the read never reach the debouncer"
        );
        assert_eq!(
            journals.journal_of(id(1)),
            Some((vec![PathBuf::from("mapped.bin")], None))
        );
        assert_eq!(journals.take(id(1)).sync, 1, "the cookie was seen");
        assert_eq!(
            journals.journal_of(id(2)),
            Some((vec![], None)),
            "the root's metadata is not a change"
        );

        handler.handle_event(Ok(rescan(Some("/w/b/sub"), Instant::now()).event));
        assert_eq!(journals.peek(id(2)).stale, Some(WalkReason::EventsLost));
        let other = journals.peek(id(1)).stale;
        if cfg!(target_os = "macos") {
            assert_eq!(other, None, "a stream's rescan is its workspace's");
        } else {
            assert_eq!(
                other,
                Some(WalkReason::EventsLost),
                "one queue serves them all"
            );
        }
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
mod attach_tests {
    //! R1.32's background attaches, against the real watcher on both
    //! platforms (`background` is forced on). Bounds are loose: on macOS a
    //! debug test binary's `FSEvents` stream restart alone can take seconds
    //! (fseventsd works in proportion to the executable's directory).

    use std::sync::mpsc as std_mpsc;

    use super::*;

    /// The longest any one wait here takes.
    const BOUND: Duration = Duration::from_mins(1);

    fn id(n: u8) -> WorkspaceId {
        format!("00000000000000b{n:x}").parse().unwrap()
    }

    struct Fixture {
        watcher: Arc<NotifyWatcher>,
        gate: Arc<AttachGate>,
        events: mpsc::Receiver<WatchEvent>,
        health: std_mpsc::Receiver<(WorkspaceId, bool)>,
        dir: tempfile::TempDir,
        root: PathBuf,
    }

    fn fixture() -> Fixture {
        let (tx, events) = mpsc::channel(256);
        let (health_tx, health) = std_mpsc::channel();
        let health_tx = Mutex::new(health_tx);
        let hook: HealthHook = Arc::new(move |ws, up| {
            let _ = lock(&health_tx).send((ws, up));
        });
        let gate = Arc::new(AttachGate::default());
        let watcher = Arc::new(
            NotifyWatcher::new(
                Duration::from_millis(50),
                tx,
                hook,
                true,
                Arc::clone(&gate),
                Arc::new(Journals::new(1_000)),
            )
            .unwrap(),
        );
        let dir = tempfile::tempdir().unwrap();
        // Events carry real paths (`/private/var` on macOS).
        let root = std::fs::canonicalize(dir.path()).unwrap();
        Fixture {
            watcher,
            gate,
            events,
            health,
            dir,
            root,
        }
    }

    impl Fixture {
        fn mkroot(&self, name: &str) -> PathBuf {
            let root = self.root.join(name);
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
            root
        }

        /// The next health report, failing after [`BOUND`].
        fn next_health(&self) -> (WorkspaceId, bool) {
            self.health.recv_timeout(BOUND).expect("a health report")
        }

        /// The next event satisfying `wanted`, failing after `within`.
        fn next_event(
            &mut self,
            within: Duration,
            wanted: impl Fn(&WatchEvent) -> bool,
        ) -> Option<WatchEvent> {
            let deadline = Instant::now() + within;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return None;
                }
                match self.events.try_recv() {
                    Ok(event) if wanted(&event) => return Some(event),
                    Ok(_) => {}
                    Err(mpsc::error::TryRecvError::Empty) => {
                        std::thread::sleep(Duration::from_millis(5).min(left));
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => return None,
                }
            }
        }

        /// Writes files under `root` until a path event for `ws` arrives.
        /// (`FSEvents` may not report a write made the instant a stream
        /// starts.)
        fn delivered(&mut self, ws: WorkspaceId, root: &Path) {
            let begun = Instant::now();
            for i in 0.. {
                std::fs::write(root.join(format!("written-{i}.txt")), "x\n").unwrap();
                let event = self.next_event(Duration::from_millis(500), |event| {
                    event.workspace == ws && !event.everything
                });
                if event.is_some() {
                    return;
                }
                assert!(begun.elapsed() < BOUND, "no event for {ws}");
            }
        }
    }

    /// R1.32, 1: deferred watches queue at once and attach in order once the gate
    /// opens; each is reported up and followed by one `everything` event
    /// dated no later than its `watch`; an inline watch sends none.
    #[test]
    fn deferred_watch_is_queued_and_attaches_in_order() {
        let mut fx = fixture();
        let hold = fx.gate.hold();
        let roots: Vec<_> = (1..=3).map(|n| fx.mkroot(&format!("ws-{n}"))).collect();
        let mut returned = Vec::new();
        for (n, root) in (1..=3).zip(&roots) {
            let begun = Instant::now();
            let attach = fx.watcher.watch(id(n), root, Vec::new(), true).unwrap();
            let took = begun.elapsed();
            returned.push(Instant::now());
            assert_eq!(attach, Attach::Pending);
            assert!(took < Duration::from_millis(10), "queueing took {took:?}");
            assert_eq!(fx.watcher.pending(id(n)), Some(PendingWatch::Attaching));
        }
        assert_eq!(fx.gate.queued(), 3);
        // Before the third watch exists: only the attach's event covers it.
        std::fs::write(roots[2].join("early.txt"), "early\n").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(fx.health.try_recv().is_err(), "nothing attaches while held");
        drop(hold);

        for n in 1..=3 {
            assert_eq!(fx.next_health(), (id(n), true), "reported in queue order");
            let event = fx
                .next_event(BOUND, |event| event.everything)
                .expect("an everything event");
            assert_eq!(event.workspace, id(n), "{event:?}");
            assert_eq!(event.paths, Vec::<PathBuf>::new());
            assert!(event.at <= returned[usize::from(n - 1)]);
        }
        assert_eq!(fx.gate.queued(), 0);
        for n in 1..=3 {
            assert_eq!(fx.watcher.pending(id(n)), None);
        }
        fx.delivered(id(3), &roots[2]);

        let fourth = fx.mkroot("ws-4");
        let attach = fx.watcher.watch(id(4), &fourth, Vec::new(), false).unwrap();
        assert_eq!(attach, Attach::Attached);
        assert_eq!(fx.watcher.pending(id(4)), None);
        assert!(
            fx.next_event(Duration::from_millis(300), |event| event.everything)
                .is_none(),
            "an inline watch sends no everything event"
        );
        assert!(fx.health.try_recv().is_err(), "nor reports health");
        fx.delivered(id(4), &fourth);
    }

    /// R1.32, 2: an unwatch takes a queued workspace off the queue at once, and
    /// waits for one being attached; no event follows it.
    #[test]
    fn unwatch_cancels_a_queued_attach_and_waits_for_one_in_flight() {
        let mut fx = fixture();
        let hold = fx.gate.hold();
        let (a, b) = (fx.mkroot("a"), fx.mkroot("b"));
        fx.watcher.watch(id(1), &a, Vec::new(), true).unwrap();
        fx.watcher.watch(id(2), &b, Vec::new(), true).unwrap();
        let begun = Instant::now();
        fx.watcher.unwatch(id(2)).unwrap();
        assert!(begun.elapsed() < Duration::from_millis(100));
        assert_eq!(fx.watcher.pending(id(2)), None);
        assert_eq!(fx.gate.queued(), 1);
        drop(hold);
        assert_eq!(fx.next_health(), (id(1), true));
        let event = fx.next_event(BOUND, |event| event.everything).unwrap();
        assert_eq!(event.workspace, id(1));
        std::fs::write(b.join("ignored.txt"), "x\n").unwrap();
        assert!(
            fx.next_event(Duration::from_millis(500), |event| event.workspace == id(2))
                .is_none(),
            "B was never watched"
        );
        assert!(fx.health.try_recv().is_err(), "B was never attached");
        assert_eq!(fx.gate.queued(), 0);

        // An attach in flight: held inside `attach` by the routing lock,
        // with the debouncer's lock taken.
        let c = fx.root.join("c");
        for d in 0..2_000 {
            std::fs::create_dir_all(c.join(format!("d{}/e{d}", d % 40))).unwrap();
        }
        let watcher = Arc::clone(&fx.watcher);
        let routing = lock(&watcher.shared.demux);
        watcher.watch(id(3), &c, Vec::new(), true).unwrap();
        let begun = Instant::now();
        while watcher.shared.debouncer.try_lock().is_ok() {
            assert!(begun.elapsed() < BOUND, "the attach never started");
            std::thread::sleep(Duration::from_millis(1));
        }
        let unwatching = {
            let watcher = Arc::clone(&watcher);
            std::thread::spawn(move || {
                watcher.unwatch(id(3)).unwrap();
                watcher.pending(id(3))
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !unwatching.is_finished(),
            "the unwatch waits for the attach"
        );
        drop(routing);
        assert_eq!(
            unwatching.join().unwrap(),
            None,
            "the attach was done when the unwatch returned"
        );
        // An unwatch that lands before the attach's report supersedes it,
        // so the report may or may not come; nothing else may.
        if let Ok(report) = fx.health.recv_timeout(Duration::from_millis(500)) {
            assert_eq!(report, (id(3), true));
        }
        std::fs::write(c.join("after.txt"), "x\n").unwrap();
        std::fs::write(c.join("d0/after.txt"), "x\n").unwrap();
        assert!(
            fx.next_event(Duration::from_secs(1), |event| event.workspace == id(3)
                && !event.everything)
                .is_none(),
            "no event after the unwatch"
        );
        drop(fx.dir);
    }

    /// R1.32: an attach waits for the inotify turn, which full scans hold.
    #[test]
    fn attaches_wait_for_the_inotify_turn() {
        let fx = fixture();
        let turn = pollster::block_on(fx.gate.turn()).unwrap();
        let a = fx.mkroot("a");
        fx.watcher.watch(id(1), &a, Vec::new(), true).unwrap();
        assert!(
            fx.health.recv_timeout(Duration::from_millis(300)).is_err(),
            "attached while a scan held the turn"
        );
        drop(turn);
        assert_eq!(fx.next_health(), (id(1), true));
    }

    /// R1.32: the attach scans of a burst are held until no attach is
    /// queued, and an unwatch that empties the queue releases them.
    #[test]
    fn attach_scans_wait_for_the_queue() {
        let mut fx = fixture();
        let turn = pollster::block_on(fx.gate.turn()).unwrap();
        let (a, b) = (fx.mkroot("a"), fx.mkroot("b"));
        fx.watcher.watch(id(1), &a, Vec::new(), true).unwrap();
        fx.watcher.watch(id(2), &b, Vec::new(), true).unwrap();
        // The attach thread waits for the turn; a second holder queues behind
        // it, so after A the thread waits again with B still queued.
        std::thread::sleep(Duration::from_millis(100));
        let gate = Arc::clone(&fx.gate);
        let (release, released) = std_mpsc::channel::<()>();
        let (taken, took) = std_mpsc::channel();
        let holder = std::thread::spawn(move || {
            let turn = pollster::block_on(gate.turn()).unwrap();
            taken.send(()).unwrap();
            let _ = released.recv();
            drop(turn);
        });
        std::thread::sleep(Duration::from_millis(100));
        drop(turn);
        assert_eq!(fx.next_health(), (id(1), true));
        took.recv_timeout(BOUND).unwrap();
        assert!(
            fx.next_event(Duration::from_millis(300), |event| event.everything)
                .is_none(),
            "A's scan was released with B still queued"
        );
        // B goes before its attach: the queue is empty, so A's scan goes.
        fx.watcher.unwatch(id(2)).unwrap();
        release.send(()).unwrap();
        holder.join().unwrap();
        let event = fx.next_event(BOUND, |event| event.everything).unwrap();
        assert_eq!(event.workspace, id(1));
        assert!(fx.health.try_recv().is_err(), "B was never attached");
    }

    /// R1.32, 5: a root gone by the time its attach runs is reported down, with
    /// the fallback-timer warning, and sends no event.
    #[test]
    fn failed_attach_reports_down() {
        let logs = capture_logs();
        let mut fx = fixture();
        let hold = fx.gate.hold();
        let gone = fx.mkroot("gone");
        fx.watcher.watch(id(5), &gone, Vec::new(), true).unwrap();
        std::fs::remove_dir_all(&gone).unwrap();
        drop(hold);
        assert_eq!(fx.next_health(), (id(5), false));
        assert_eq!(fx.watcher.pending(id(5)), None);
        assert_eq!(fx.gate.queued(), 0);
        assert!(
            fx.next_event(Duration::from_millis(300), |event| event.workspace == id(5))
                .is_none()
        );
        let text = String::from_utf8_lossy(&lock(&logs)).into_owned();
        assert!(
            text.lines()
                .any(|line| line.contains("cannot watch the workspace")
                    && line.contains(&id(5).to_string())),
            "{text}"
        );
    }

    /// The process's log lines (every thread's), from a global subscriber
    /// this module installs on first use.
    fn capture_logs() -> Arc<Mutex<Vec<u8>>> {
        static LOGS: std::sync::OnceLock<Arc<Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
        Arc::clone(LOGS.get_or_init(|| {
            let logs = Arc::new(Mutex::new(Vec::new()));
            let writer = Capture(Arc::clone(&logs));
            let _ = tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::new("reef_daemon=info"))
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .try_init();
            logs
        }))
    }

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
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
            NotifyWatcher::new(
                Duration::from_millis(50),
                tx,
                Arc::new(|_, _| {}),
                false,
                Arc::default(),
                Arc::new(Journals::new(1_000)),
            )
            .unwrap(),
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
