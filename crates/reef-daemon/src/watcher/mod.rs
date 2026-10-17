//! Filesystem watchers that trigger snapshots (R1.13). A watcher only says
//! that something under a workspace changed; the scheduler
//! ([`crate::snapshot::Scheduler`]) decides when to snapshot, and the scan
//! itself is jj's, over every tracked file (ADR 0008). Watchman, which can
//! also narrow the scan, is R1.14.
//!
//! The daemon runs one watcher for every workspace. Events are mapped back
//! to workspaces by root (`Demux`): workspace roots never nest, so the
//! root a path falls under is unambiguous.

pub mod notify;

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::time::Instant;

use reef_lib::error::{Classify, ErrorKind};
use tokio::sync::mpsc;

use crate::config::{WatcherBackend, WatcherSection};
use crate::model::WorkspaceId;

/// Changes under one workspace, after debouncing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchEvent {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The changed paths, absolute. Empty means "anything may have changed".
    pub paths: Vec<PathBuf>,
    /// When the earliest of them was observed.
    pub at: Instant,
}

/// Which mechanism a watcher uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatcherKind {
    /// The `notify` crate: inotify on Linux, `FSEvents` on macOS.
    Notify,
    /// A Watchman subscription (R1.14).
    Watchman,
}

/// Watches workspace roots and reports changes as [`WatchEvent`]s.
pub trait Watcher: Send + Sync {
    /// Which mechanism this is.
    fn kind(&self) -> WatcherKind;
    /// Starts watching `root`, recursively, for `workspace`. Changes under
    /// any of `skip` (absolute prefixes under `root`) are not reported.
    /// Watching a workspace again replaces its root and skip list.
    fn watch(
        &self,
        workspace: WorkspaceId,
        root: &Path,
        skip: Vec<PathBuf>,
    ) -> Result<(), WatchError>;
    /// Stops watching `workspace`. Not watching it is not an error.
    fn unwatch(&self, workspace: WorkspaceId) -> Result<(), WatchError>;
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
}

impl WatchError {
    /// Whether the operating system's watch limit was hit (inotify's
    /// `max_user_watches` or `max_user_instances`).
    pub fn is_limit(&self) -> bool {
        match self {
            Self::Start { source, .. } | Self::Watch { source, .. } => notify::is_limit(source),
            Self::Unavailable(_) => false,
        }
    }
}

impl Classify for WatchError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Start { .. } | Self::Watch { .. } => ErrorKind::Io,
            Self::Unavailable(_) => ErrorKind::Unavailable,
        }
    }
}

/// The daemon's watcher for `config.backend`, sending to `events`: notify
/// for `notify` and `auto`; `watchman` is [`WatchError::Unavailable`] until
/// R1.14.
pub fn new_watcher(
    config: &WatcherSection,
    events: mpsc::Sender<WatchEvent>,
) -> Result<Box<dyn Watcher>, WatchError> {
    match config.backend {
        WatcherBackend::Watchman => Err(WatchError::Unavailable("watchman")),
        WatcherBackend::Auto | WatcherBackend::Notify => Ok(Box::new(notify::NotifyWatcher::new(
            config.debounce,
            events,
        )?)),
    }
}

/// Stands in when no watcher could be created: every `watch` fails, so each
/// workspace is marked `watch-failed` and lives on the fallback timer.
#[derive(Debug)]
pub struct NoWatcher {
    /// Why there is no watcher, as `watch` reports it.
    pub reason: &'static str,
}

impl Watcher for NoWatcher {
    fn kind(&self) -> WatcherKind {
        WatcherKind::Notify
    }

    fn watch(&self, _: WorkspaceId, _: &Path, _: Vec<PathBuf>) -> Result<(), WatchError> {
        Err(WatchError::Unavailable(self.reason))
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

/// Maps paths to the workspace whose root they fall under.
#[derive(Debug, Default)]
pub(crate) struct Demux {
    /// Root → (workspace, skipped prefixes).
    roots: BTreeMap<PathBuf, (WorkspaceId, Vec<PathBuf>)>,
}

impl Demux {
    /// Routes paths under `root` to `workspace`, replacing any root the
    /// workspace had. Returns the replaced root.
    pub(crate) fn insert(
        &mut self,
        workspace: WorkspaceId,
        root: PathBuf,
        skip: Vec<PathBuf>,
    ) -> Option<PathBuf> {
        let old = self.remove(workspace);
        self.roots.insert(root, (workspace, skip));
        old
    }

    /// Stops routing to `workspace`; returns its root.
    pub(crate) fn remove(&mut self, workspace: WorkspaceId) -> Option<PathBuf> {
        let root = self
            .roots
            .iter()
            .find(|(_, (id, _))| *id == workspace)
            .map(|(root, _)| root.clone())?;
        self.roots.remove(&root);
        Some(root)
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
        demux.insert(id(1), PathBuf::from("/s/ws/a1"), vec![]);
        demux.insert(id(2), PathBuf::from("/s/ws/a10"), vec![]);
        demux.insert(
            id(3),
            PathBuf::from("/s/ws/b"),
            vec![PathBuf::from("/s/ws/b/target")],
        );
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
        demux.insert(id(1), PathBuf::from("/a"), vec![]);
        assert_eq!(
            demux.insert(id(1), PathBuf::from("/b"), vec![]),
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
        demux.insert(id(1), PathBuf::from("/a"), vec![]);
        demux.insert(id(2), PathBuf::from("/b"), vec![]);
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
    fn skip_prefixes_are_literal_and_absolute() {
        let excludes = ["target/".to_owned(), "*.log".to_owned()];
        let mounts = ["node_modules".to_owned()];
        assert_eq!(
            skip_prefixes(Path::new("/w"), &excludes, &mounts),
            [PathBuf::from("/w/target"), PathBuf::from("/w/node_modules")]
        );
    }
}
