//! The status index's journal (R2.23, ADR 0048): per tracked workspace, the
//! paths the watcher reported since the last scan took them, and whether
//! they can be trusted.
//!
//! The index is jj's working-copy state, which records each tracked file's
//! type, mtime, size and content, plus this journal: a path is *dirty* when
//! it, or a directory above it, is in the journal, and every other path is
//! as jj's state records it, unless the journal is *stale*. A read answers
//! from it: nothing dirty means no snapshot, otherwise a snapshot narrowed
//! to the dirty paths (ADR 0026), and a stale journal means a full scan.
//!
//! notify's thread feeds it directly, before the debouncer
//! (`watcher::notify`'s tap), so a path is here as soon as its event is
//! read rather than half a second later. Watchman workspaces keep only the
//! stale reason: jj narrows their scans through its own synced query.
//!
//! # The sync point
//!
//! A read trusts the journal only once it knows the watcher has delivered
//! every write that finished before the read arrived. `Journals::sync`
//! creates a cookie file in the workspace's sync directory, outside its
//! root (`repos/<id>/sync/<ws>/`), and waits for the tap to see it. The
//! sync directory is watched with the root: by the daemon's one inotify
//! instance on Linux, whose one queue orders every event, and on macOS as a
//! second path of the workspace's own `FSEvents` stream, which delivers in
//! event-id order. A cookie seen therefore orders after every earlier
//! write's event. Concurrent reads share cookies: a read waits for one
//! created after it arrived, so each costs at most two round trips.
//!
//! Cookie numbers come from one counter for the daemon's life, so a
//! journal tracked again (a restore, a rewatch) never reuses one that a
//! scan's mark still holds.
//!
//! On Linux notify adds a new directory's watch only once it has read the
//! rest of its buffer, so a write under a directory just created can go
//! unreported. A new directory therefore stays dirty across takes until an
//! event from under it shows its watch is up, or `STICKY` has passed.
//!
//! # Locks
//!
//! The journal's lock is never held across I/O or an `.await`. Lock order:
//! `Demux`, then the scheduler's entries, then [`Journals`]; nothing takes
//! either of the others while holding it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::model::WorkspaceId;
use crate::workspace::REEFIGNORE;

/// The longest a new directory stays dirty waiting for its watch.
const STICKY: Duration = Duration::from_secs(10);

/// Why a status walked the tree instead of reading the index; the first
/// five are journal states, the rest found by one read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WalkReason {
    /// The workspace's watch is not up: it failed, is still attaching, hit
    /// the watch limit, or its Watchman subscription is down.
    NotWatched,
    /// It was watched again after a time unwatched: writes in between were
    /// never reported.
    WatchRestarted,
    /// The watcher lost events: an inotify overflow, an `FSEvents` rescan,
    /// a Watchman fresh instance.
    EventsLost,
    /// More than `watcher.narrowed_max_paths` paths, the root itself, or
    /// `.reefignore`.
    TooBroad,
    /// A snapshot that took the journal failed.
    SnapshotFailed,
    /// The read's cookie was not seen within `sync_timeout`.
    NotSynced,
    /// The working copy is stale against the view, or has no working-copy
    /// commit in it.
    StaleWorkingCopy,
    /// A journal path cannot be narrowed to.
    PathNotNarrowable,
    /// A full scan the read did not start answered it.
    CoveredByFullScan,
}

impl WalkReason {
    /// The name the daemon's log uses: `not-watched`, `events-lost`, ...
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotWatched => "not-watched",
            Self::WatchRestarted => "watch-restarted",
            Self::EventsLost => "events-lost",
            Self::TooBroad => "too-broad",
            Self::SnapshotFailed => "snapshot-failed",
            Self::NotSynced => "not-synced",
            Self::StaleWorkingCopy => "stale-working-copy",
            Self::PathNotNarrowable => "path-not-narrowable",
            Self::CoveredByFullScan => "covered-by-full-scan",
        }
    }
}

/// What a snapshot took from a journal ([`Journals::take`]).
#[derive(Clone, Debug, Default)]
pub(crate) struct Taken {
    /// The dirty paths, relative to the root.
    pub(crate) paths: Vec<PathBuf>,
    /// Why they cannot be trusted, if they cannot.
    pub(crate) stale: Option<WalkReason>,
    /// The latest cookie the tap had seen: every write before it is in
    /// `paths`, unless `stale`.
    pub(crate) sync: u64,
}

/// What a read sees without taking ([`Journals::peek`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Peek {
    /// Why the journal cannot be trusted, if it cannot.
    pub(crate) stale: Option<WalkReason>,
    /// No path is dirty.
    pub(crate) empty: bool,
}

/// Every tracked workspace's journal; one per daemon, shared by the
/// watchers and the scheduler.
#[derive(Debug)]
pub struct Journals {
    by_workspace: Mutex<HashMap<WorkspaceId, Journal>>,
    /// `watcher.narrowed_max_paths`.
    max_paths: usize,
    /// This process's id, in cookie names, so a cookie another daemon left
    /// is never taken for one of ours.
    pid: u32,
    /// The last cookie number handed out, for every workspace.
    next_cookie: AtomicU64,
    /// Changes full scans found that no event had named (R2.23).
    misses: AtomicU64,
    /// Test hook: cookies are written but never seen.
    swallow_cookies: AtomicBool,
    /// Test hook: raw events under these absolute paths are dropped.
    dropped: Mutex<Vec<PathBuf>>,
}

#[derive(Debug)]
struct Journal {
    /// The root, canonical as the watchers report paths.
    root: PathBuf,
    /// A notify workspace's: Reef narrows its scans.
    notify: bool,
    /// The sync directory, canonical; `None` for Watchman workspaces, and
    /// when it could not be made.
    sync_dir: Option<PathBuf>,
    paths: BTreeSet<PathBuf>,
    /// New directories whose watch may not be up yet, until when.
    sticky: HashMap<PathBuf, Instant>,
    stale: Option<WalkReason>,
    /// The watch is up; until it is, `stale` cannot be cleared.
    watch_up: bool,
    /// The latest cookie the tap saw, and how often the journal went
    /// stale: a read waiting for a cookie stops at either.
    signal: tokio::sync::watch::Sender<Signal>,
    /// The cookie written and not yet seen, and when it was created.
    in_flight: Option<(u64, Instant)>,
}

/// What a read waiting in [`Journals::sync`] watches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Signal {
    seen: u64,
    lost: u64,
}

impl Journal {
    fn mark(&mut self, reason: WalkReason) {
        self.paths.clear();
        self.sticky.clear();
        self.stale = Some(reason);
        self.signal.send_modify(|signal| signal.lost += 1);
    }

    fn seen(&self) -> u64 {
        self.signal.borrow().seen
    }
}

impl Journals {
    /// Journals that turn `TooBroad` past `max_paths` paths; `0` never
    /// trusts a path.
    pub fn new(max_paths: usize) -> Self {
        Self {
            by_workspace: Mutex::new(HashMap::new()),
            max_paths,
            pid: std::process::id(),
            next_cookie: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            swallow_cookies: AtomicBool::new(false),
            dropped: Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<WorkspaceId, Journal>> {
        self.by_workspace
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts or restarts `workspace`'s journal: `root` and `sync_dir`
    /// canonical, the watch not up yet, and `stale` if writes may have gone
    /// unreported (a restart, a rewatch).
    pub(crate) fn track(
        &self,
        workspace: WorkspaceId,
        root: PathBuf,
        notify: bool,
        sync_dir: Option<PathBuf>,
        stale: Option<WalkReason>,
    ) {
        let mut journals = self.lock();
        let journal = journals.entry(workspace).or_insert_with(|| Journal {
            root: root.clone(),
            notify,
            sync_dir: sync_dir.clone(),
            paths: BTreeSet::new(),
            sticky: HashMap::new(),
            stale: None,
            watch_up: false,
            signal: tokio::sync::watch::Sender::new(Signal::default()),
            in_flight: None,
        });
        journal.root = root;
        journal.notify = notify;
        journal.sync_dir = sync_dir;
        journal.watch_up = false;
        if let Some(reason) = stale {
            journal.mark(reason);
        }
    }

    /// Forgets `workspace`; a read waiting on its cookie gives up.
    pub(crate) fn untrack(&self, workspace: WorkspaceId) {
        self.lock().remove(&workspace);
    }

    /// `workspace`'s sync directory, if it has one.
    pub(crate) fn sync_dir(&self, workspace: WorkspaceId) -> Option<PathBuf> {
        self.lock()
            .get(&workspace)
            .and_then(|journal| journal.sync_dir.clone())
    }

    /// Marks `workspace`'s journal `NotWatched` for a watch that is being
    /// set up in the background, leaving whether it is up to the watcher's
    /// report, which may already have come.
    pub(crate) fn pending(&self, workspace: WorkspaceId) {
        if let Some(journal) = self.lock().get_mut(&workspace) {
            journal.mark(WalkReason::NotWatched);
        }
    }

    /// Records whether `workspace`'s watch is up. Down marks the journal
    /// `NotWatched`; up only lets the next full scan clear it.
    pub(crate) fn set_watch(&self, workspace: WorkspaceId, up: bool) {
        if let Some(journal) = self.lock().get_mut(&workspace) {
            journal.watch_up = up;
            if !up {
                journal.mark(WalkReason::NotWatched);
            }
        }
    }

    /// Records a change at `path` (absolute) in `workspace`: the root
    /// itself or `.reefignore` is `TooBroad`, and so is one path past the
    /// cap; a metadata-only change of the root is dropped (jj records
    /// nothing about it). `new_dir`: a directory just created or moved in,
    /// whose watch may not be up yet (see the module docs).
    pub(crate) fn record(&self, workspace: WorkspaceId, path: &Path, change: Change) {
        let mut journals = self.lock();
        let Some(journal) = journals.get_mut(&workspace) else {
            return;
        };
        if journal.stale.is_some() {
            return;
        }
        let Ok(relative) = path.strip_prefix(&journal.root) else {
            return;
        };
        if !journal.sticky.is_empty() {
            // An event from under a new directory: its watch is up.
            journal
                .sticky
                .retain(|dir, _| !(relative.starts_with(dir) && relative != dir));
        }
        if change == Change::NewDir && !relative.as_os_str().is_empty() {
            journal
                .sticky
                .insert(relative.to_owned(), Instant::now() + STICKY);
        }
        let metadata_only = change == Change::Metadata;
        if relative.as_os_str().is_empty() {
            if !metadata_only {
                journal.mark(WalkReason::TooBroad);
            }
            return;
        }
        // jj rescans under a changed `.gitignore` itself, not under Reef's
        // own ignore file.
        if relative == Path::new(REEFIGNORE)
            || (journal.paths.insert(relative.to_owned()) && journal.paths.len() > self.max_paths)
        {
            journal.mark(WalkReason::TooBroad);
        }
    }

    /// Marks `workspace`'s journal `EventsLost`.
    pub(crate) fn lost(&self, workspace: WorkspaceId) {
        if let Some(journal) = self.lock().get_mut(&workspace) {
            journal.mark(WalkReason::EventsLost);
        }
    }

    /// Marks every notify workspace's journal `EventsLost`: an inotify
    /// overflow, a watch limit hit, or a rescan under no root.
    pub(crate) fn lost_all_notify(&self) {
        for journal in self.lock().values_mut() {
            if journal.notify {
                journal.mark(WalkReason::EventsLost);
            }
        }
    }

    /// Records that the tap saw cookie `name` in `workspace`'s sync
    /// directory; names that are not this process's cookies are ignored.
    pub(crate) fn seen_cookie(&self, workspace: WorkspaceId, name: &str) {
        if self.swallow_cookies.load(Ordering::Relaxed) {
            return;
        }
        let Some(n) = name
            .split_once('-')
            .filter(|(pid, _)| pid.parse::<u32>().ok() == Some(self.pid))
            .and_then(|(_, n)| n.parse::<u64>().ok())
        else {
            return;
        };
        if let Some(journal) = self.lock().get(&workspace) {
            journal.signal.send_if_modified(|signal| {
                let newer = n > signal.seen;
                if newer {
                    signal.seen = n;
                }
                newer
            });
        }
    }

    /// Moves `workspace`'s dirty paths and stale reason out, for a snapshot
    /// about to scan; events from now on go to the next one. A journal
    /// whose watch is not up stays `NotWatched`.
    pub(crate) fn take(&self, workspace: WorkspaceId) -> Taken {
        let mut journals = self.lock();
        let Some(journal) = journals.get_mut(&workspace) else {
            return Taken {
                stale: Some(WalkReason::NotWatched),
                ..Taken::default()
            };
        };
        let mut paths = std::mem::take(&mut journal.paths);
        let now = Instant::now();
        journal.sticky.retain(|_, until| *until > now);
        for dir in journal.sticky.keys() {
            // Scanned now, and dirty again for the next scan.
            paths.insert(dir.clone());
            journal.paths.insert(dir.clone());
        }
        let taken = Taken {
            paths: paths.into_iter().collect(),
            stale: journal.stale.take(),
            sync: journal.seen(),
        };
        if !journal.watch_up {
            journal.stale = Some(WalkReason::NotWatched);
        }
        taken
    }

    /// Marks `workspace`'s journal `SnapshotFailed` after a snapshot that
    /// took it failed, unless it is already stale for another reason.
    pub(crate) fn failed(&self, workspace: WorkspaceId) {
        if let Some(journal) = self.lock().get_mut(&workspace) {
            let reason = journal.stale.unwrap_or(WalkReason::SnapshotFailed);
            journal.mark(reason);
        }
    }

    /// `workspace`'s journal as a read sees it, without taking it.
    pub(crate) fn peek(&self, workspace: WorkspaceId) -> Peek {
        match self.lock().get(&workspace) {
            Some(journal) => Peek {
                stale: journal.stale,
                empty: journal.paths.is_empty(),
            },
            None => Peek {
                stale: Some(WalkReason::NotWatched),
                empty: true,
            },
        }
    }

    /// The tracked workspaces whose journal is stale.
    pub(crate) fn stale_set(&self) -> HashSet<WorkspaceId> {
        self.lock()
            .iter()
            .filter(|(_, journal)| journal.stale.is_some())
            .map(|(ws, _)| *ws)
            .collect()
    }

    /// Whether `workspace`'s journal now holds a path covering `path`
    /// (relative): the path or a directory above it.
    pub(crate) fn covers(&self, workspace: WorkspaceId, path: &Path) -> bool {
        self.lock()
            .get(&workspace)
            .is_some_and(|journal| covered(journal.paths.iter(), path))
    }

    /// Waits until the tap has seen a cookie of `workspace` created after
    /// `arrived`, writing one if none is in flight; returns its number, or
    /// `None` when none was seen within `timeout`, one could not be
    /// written, or the journal went stale meanwhile (see the module docs).
    pub(crate) async fn sync(
        &self,
        workspace: WorkspaceId,
        arrived: Instant,
        timeout: Duration,
    ) -> Option<u64> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let (action, mut seen, lost) = {
                let mut journals = self.lock();
                let journal = journals.get_mut(&workspace)?;
                let dir = journal.sync_dir.clone()?;
                if journal.stale.is_some() {
                    return None;
                }
                let seen = journal.signal.subscribe();
                let Signal {
                    seen: current,
                    lost,
                } = *seen.borrow();
                let action = match journal.in_flight {
                    Some((n, created)) if n > current && created > arrived => Sync::Wait(n),
                    Some((n, _)) if n > current => Sync::WaitThenWrite(n),
                    // None in flight, or the one in flight was already seen.
                    _ => {
                        let n = self.next_cookie.fetch_add(1, Ordering::Relaxed) + 1;
                        journal.in_flight = Some((n, Instant::now()));
                        Sync::Write(n, dir.join(format!("{}-{n}", self.pid)))
                    }
                };
                (action, seen, lost)
            };
            match action {
                Sync::Write(n, path) => {
                    let written = write_cookie(&path);
                    let synced = match &written {
                        Ok(()) => wait_seen(&mut seen, n, lost, deadline).await,
                        Err(error) => {
                            tracing::warn!(
                                workspace_id = %workspace,
                                path = %path.display(),
                                %error,
                                "cannot write a sync cookie; the read walks the tree"
                            );
                            false
                        }
                    };
                    if written.is_ok() {
                        let _ = std::fs::remove_file(&path);
                    }
                    if let Some(journal) = self.lock().get_mut(&workspace)
                        && journal.in_flight.is_some_and(|(m, _)| m == n)
                    {
                        journal.in_flight = None;
                    }
                    return synced.then_some(n);
                }
                Sync::Wait(n) => return wait_seen(&mut seen, n, lost, deadline).await.then_some(n),
                Sync::WaitThenWrite(n) => {
                    if !wait_seen(&mut seen, n, lost, deadline).await {
                        return None;
                    }
                }
            }
        }
    }

    /// Whether `workspace`'s watch delivers: writes a cookie and waits up to
    /// `timeout`, blocking, for the tap to see it; true when it has no sync
    /// directory. A macOS stream reports nothing when it fails to start, so
    /// a new one is confirmed this way before it counts as attached.
    #[cfg(target_os = "macos")]
    pub(crate) fn confirm(&self, workspace: WorkspaceId, timeout: Duration) -> bool {
        let Some(dir) = self.sync_dir(workspace) else {
            return true;
        };
        let n = self.next_cookie.fetch_add(1, Ordering::Relaxed) + 1;
        let path = dir.join(format!("{}-{n}", self.pid));
        if write_cookie(&path).is_err() {
            return false;
        }
        let deadline = Instant::now() + timeout;
        let seen = loop {
            let seen = self
                .lock()
                .get(&workspace)
                .is_some_and(|journal| journal.seen() >= n);
            if seen || Instant::now() >= deadline {
                break seen;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let _ = std::fs::remove_file(&path);
        seen
    }

    /// Counts `count` changes a full scan found that no event had named.
    pub(crate) fn add_misses(&self, count: u64) {
        self.misses.fetch_add(count, Ordering::Relaxed);
    }

    /// Changes full scans found that no event had named, since start.
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Whether a raw event at `path` is to be dropped (a test hook).
    pub(crate) fn drops(&self, path: &Path) -> bool {
        let dropped = self.dropped.lock().unwrap_or_else(PoisonError::into_inner);
        !dropped.is_empty() && dropped.iter().any(|prefix| path.starts_with(prefix))
    }

    /// Test hook: from now on cookies are written but never seen, so every
    /// read's sync times out.
    #[cfg(any(test, feature = "testing"))]
    pub fn swallow_cookies(&self, swallow: bool) {
        self.swallow_cookies.store(swallow, Ordering::Relaxed);
    }

    /// Test hook: from now on raw events under `prefix` (absolute,
    /// canonical) are dropped before the journal and the debouncer, as if
    /// no event had named them.
    #[cfg(any(test, feature = "testing"))]
    pub fn drop_events_under(&self, prefix: PathBuf) {
        self.dropped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(prefix);
    }

    /// Test hook: `workspace`'s dirty paths and stale reason.
    #[cfg(any(test, feature = "testing"))]
    pub fn journal_of(&self, workspace: WorkspaceId) -> Option<(Vec<PathBuf>, Option<WalkReason>)> {
        self.lock()
            .get(&workspace)
            .map(|journal| (journal.paths.iter().cloned().collect(), journal.stale))
    }
}

/// What changed at a path the tap journals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    /// Contents, names, anything.
    Any,
    /// Metadata alone: dropped for the root itself.
    Metadata,
    /// A directory created or moved in, whose watch may not be up yet.
    NewDir,
}

enum Sync {
    /// Write cookie `n` at the path and wait for it.
    Write(u64, PathBuf),
    /// Wait for cookie `n`, created after the read arrived.
    Wait(u64),
    /// Wait for cookie `n`, created before the read arrived, then write.
    WaitThenWrite(u64),
}

/// Whether `paths` holds `path` or a directory above it.
pub(crate) fn covered<'a>(mut paths: impl Iterator<Item = &'a PathBuf>, path: &Path) -> bool {
    paths.any(|prefix| {
        if cfg!(target_os = "macos") {
            // A case-insensitive volume reports the spelling a writer used.
            let (prefix, path) = (
                prefix.to_string_lossy().to_lowercase(),
                path.to_string_lossy().to_lowercase(),
            );
            Path::new(&path).starts_with(Path::new(&prefix))
        } else {
            path.starts_with(prefix)
        }
    })
}

fn write_cookie(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map(drop)
}

/// Whether cookie `n` was seen by `deadline`; false once the journal goes
/// stale after `lost`.
async fn wait_seen(
    signal: &mut tokio::sync::watch::Receiver<Signal>,
    n: u64,
    lost: u64,
    deadline: tokio::time::Instant,
) -> bool {
    let waited = tokio::time::timeout_at(
        deadline,
        signal.wait_for(|signal| signal.seen >= n || signal.lost > lost),
    )
    .await;
    matches!(waited, Ok(Ok(signal)) if signal.lost == lost)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> WorkspaceId {
        format!("{n:016x}").parse().unwrap()
    }

    fn journals(max: usize) -> Journals {
        let journals = Journals::new(max);
        journals.track(
            id(1),
            PathBuf::from("/w/a"),
            true,
            Some(PathBuf::from("/s/a")),
            None,
        );
        journals.set_watch(id(1), true);
        journals
    }

    #[test]
    fn records_paths_and_the_root_rules() {
        let journals = journals(3);
        journals.record(id(1), Path::new("/w/a/x.rs"), Change::Any);
        journals.record(id(1), Path::new("/w/a/src"), Change::Any);
        journals.record(id(1), Path::new("/w/b/y.rs"), Change::Any);
        journals.record(id(1), Path::new("/w/a"), Change::Metadata);
        assert_eq!(
            journals.journal_of(id(1)),
            Some((vec![PathBuf::from("src"), PathBuf::from("x.rs")], None)),
            "outside the root and the root's metadata are dropped"
        );
        journals.record(id(1), Path::new("/w/a"), Change::Any);
        assert_eq!(
            journals.journal_of(id(1)),
            Some((vec![], Some(WalkReason::TooBroad)))
        );
        let taken = journals.take(id(1));
        assert_eq!(taken.stale, Some(WalkReason::TooBroad));
        assert_eq!(
            journals.peek(id(1)),
            Peek {
                stale: None,
                empty: true
            }
        );

        journals.record(id(1), Path::new("/w/a/.reefignore"), Change::Any);
        assert_eq!(journals.peek(id(1)).stale, Some(WalkReason::TooBroad));
        journals.take(id(1));
        for n in 0..4 {
            journals.record(id(1), &PathBuf::from(format!("/w/a/{n}")), Change::Any);
        }
        assert_eq!(
            journals.peek(id(1)).stale,
            Some(WalkReason::TooBroad),
            "past the cap"
        );
    }

    #[test]
    fn take_moves_out_and_an_unwatched_journal_stays_stale() {
        let journals = journals(10);
        journals.record(id(1), Path::new("/w/a/x"), Change::Any);
        let taken = journals.take(id(1));
        assert_eq!(taken.paths, [PathBuf::from("x")]);
        assert_eq!(taken.stale, None);
        assert!(journals.peek(id(1)).empty);

        journals.set_watch(id(1), false);
        journals.record(id(1), Path::new("/w/a/y"), Change::Any);
        assert_eq!(journals.take(id(1)).stale, Some(WalkReason::NotWatched));
        assert_eq!(
            journals.peek(id(1)).stale,
            Some(WalkReason::NotWatched),
            "only a scan once the watch is up clears it"
        );
        journals.set_watch(id(1), true);
        journals.take(id(1));
        assert_eq!(journals.peek(id(1)).stale, None);

        journals.failed(id(1));
        assert_eq!(journals.peek(id(1)).stale, Some(WalkReason::SnapshotFailed));
        journals.lost(id(1));
        assert_eq!(journals.peek(id(1)).stale, Some(WalkReason::EventsLost));
    }

    /// A new directory stays dirty across takes until an event from under
    /// it shows its watch is up.
    #[test]
    fn new_directories_stay_dirty_until_their_watch_is_up() {
        let journals = journals(10);
        journals.record(id(1), Path::new("/w/a/new"), Change::NewDir);
        assert_eq!(journals.take(id(1)).paths, [PathBuf::from("new")]);
        assert_eq!(
            journals.take(id(1)).paths,
            [PathBuf::from("new")],
            "still dirty: a write under it may have gone unreported"
        );
        journals.record(id(1), Path::new("/w/a/new/f.txt"), Change::Any);
        assert_eq!(
            journals.take(id(1)).paths,
            [PathBuf::from("new"), PathBuf::from("new/f.txt")],
            "an event from under it: its watch is up, after one more scan"
        );
        assert_eq!(journals.take(id(1)).paths, Vec::<PathBuf>::new());
    }

    /// Cookie numbers never repeat, even for a journal tracked again.
    #[test]
    fn cookie_numbers_outlive_a_journal() {
        let journals = journals(10);
        let pid = std::process::id();
        journals.seen_cookie(id(1), &format!("{pid}-7"));
        assert_eq!(journals.take(id(1)).sync, 7);
        journals.untrack(id(1));
        journals.track(id(1), PathBuf::from("/w/a"), true, None, None);
        assert_eq!(journals.take(id(1)).sync, 0);
        let first = journals.next_cookie.fetch_add(1, Ordering::Relaxed) + 1;
        assert!(first > 0);
    }

    #[test]
    fn prefixes_cover_paths_under_them() {
        let paths = [PathBuf::from("src"), PathBuf::from("a.txt")];
        assert!(covered(paths.iter(), Path::new("src/lib.rs")));
        assert!(covered(paths.iter(), Path::new("a.txt")));
        assert!(!covered(paths.iter(), Path::new("srcs/x")));
        assert!(!covered(paths.iter(), Path::new("b.txt")));
    }

    #[test]
    fn cookie_names_are_this_process_s() {
        let journals = journals(10);
        let pid = std::process::id();
        let seen = |journals: &Journals| journals.lock()[&id(1)].seen();
        journals.seen_cookie(id(1), &format!("{}-3", pid.wrapping_add(1)));
        journals.seen_cookie(id(1), "junk");
        assert_eq!(seen(&journals), 0);
        journals.seen_cookie(id(1), &format!("{pid}-3"));
        journals.seen_cookie(id(1), &format!("{pid}-2"));
        assert_eq!(seen(&journals), 3, "never goes back");
        assert_eq!(journals.take(id(1)).sync, 3);
    }

    #[tokio::test]
    async fn sync_waits_for_the_cookie() {
        let dir = tempfile::tempdir().unwrap();
        let journals = std::sync::Arc::new(Journals::new(10));
        journals.track(
            id(1),
            PathBuf::from("/w/a"),
            true,
            Some(dir.path().to_owned()),
            None,
        );
        journals.set_watch(id(1), true);
        let arrived = Instant::now();
        let syncing = {
            let journals = std::sync::Arc::clone(&journals);
            tokio::spawn(
                async move { journals.sync(id(1), arrived, Duration::from_secs(10)).await },
            )
        };
        // Plays the tap.
        let name = loop {
            if let Some(entry) = std::fs::read_dir(dir.path()).unwrap().next() {
                break entry.unwrap().file_name().into_string().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        };
        journals.seen_cookie(id(1), &name);
        assert_eq!(syncing.await.unwrap(), Some(1));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "removed");

        journals.swallow_cookies(true);
        let synced = journals
            .sync(id(1), Instant::now(), Duration::from_millis(50))
            .await;
        assert_eq!(synced, None);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "removed on timeout"
        );
    }
}
