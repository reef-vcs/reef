//! Workspace materialization backends (R1.02, ADR 0015): the seam between
//! "the daemon wants a workspace at this path on this tree" and "this
//! platform does it with clonefile, FICLONE, an overlay mount, or a copy".
//!
//! Every backend starts from a [`BaseEntry`]: the tree materialized once per
//! machine through jj-lib by the [`BaseCache`], with jj's `tree_state` for it
//! beside it. A backend makes a workspace directory hold the base's files
//! with their mtimes and sizes preserved, so the base's `tree_state` can seed
//! the workspace's working copy and its first snapshot is a no-op scan
//! (R1.05). Backends do not capture the writable layer as a tree: jj-lib's
//! working copy scans the workspace directory and builds the tree itself
//! (R1.13). They materialize, measure, swap the base, and tear down.
//!
//! [`select`] picks a backend for a workspace root from what [`probe()`]
//! reports. `auto` takes the first available in [`BackendKind::AUTO_ORDER`];
//! a named backend that is unavailable is an error naming the reason, never
//! a silent fallback.
//!
//! The trait's methods are synchronous and block; the daemon calls them from
//! `spawn_blocking` (R1.03).

pub mod base_cache;
pub mod copy;
pub mod fs_util;
pub mod probe;

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{LazyLock, Mutex, PoisonError};

use reef_lib::error::{Classify, ErrorKind, classify_by_source};
use reef_lib::jj::TreeStateReadError;
use serde::{Deserialize, Serialize};

pub use base_cache::{BaseCache, BaseCacheError, BaseEntry};
pub use probe::{Availability, Probe, probe};

use crate::model::WorkspaceId;

/// A way of materializing workspaces. Stored as [`BackendKind::as_str`] in
/// `workspaces.backend`; serde uses the same names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    /// A plain copy of every file with mtimes restored (R1.07). Works
    /// everywhere; the backend of last resort.
    Copy,
    /// `clonefile(2)` of the base directory on APFS (R1.08).
    #[serde(rename = "apfs")]
    ApfsClone,
    /// Per-file `FICLONE` on btrfs or XFS (R1.10).
    Reflink,
    /// A kernel overlayfs mount over the base, when the daemon has
    /// `CAP_SYS_ADMIN` (R1.09).
    Overlayfs,
    /// An overlay mount through the `fuse-overlayfs` binary, unprivileged
    /// (R1.09).
    FuseOverlayfs,
}

impl BackendKind {
    /// The order `auto` tries backends in: overlays first (creation is a
    /// mount), then clones, then the copy.
    pub const AUTO_ORDER: [BackendKind; 5] = [
        BackendKind::Overlayfs,
        BackendKind::FuseOverlayfs,
        BackendKind::ApfsClone,
        BackendKind::Reflink,
        BackendKind::Copy,
    ];

    /// The stored and configured name: `copy`, `apfs`, `reflink`,
    /// `overlayfs`, or `fuse-overlayfs`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::ApfsClone => "apfs",
            Self::Reflink => "reflink",
            Self::Overlayfs => "overlayfs",
            Self::FuseOverlayfs => "fuse-overlayfs",
        }
    }
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Text that names no backend.
#[derive(Debug, thiserror::Error)]
#[error(
    "unknown workspace backend {0:?}; expected auto, copy, apfs, reflink, overlayfs, or fuse-overlayfs"
)]
pub struct ParseBackendError(pub String);

impl Classify for ParseBackendError {
    fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidArgument
    }
}

impl FromStr for BackendKind {
    type Err = ParseBackendError;

    /// The inverse of [`BackendKind::as_str`].
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::AUTO_ORDER
            .into_iter()
            .find(|kind| kind.as_str() == text)
            .ok_or_else(|| ParseBackendError(text.to_owned()))
    }
}

/// What the configuration (`workspace.backend`) or a request asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackendChoice {
    /// The first available backend in [`BackendKind::AUTO_ORDER`].
    Auto,
    /// This backend, or an error if it is unavailable.
    Named(BackendKind),
}

impl fmt::Display for BackendChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Named(kind) => kind.fmt(f),
        }
    }
}

impl FromStr for BackendChoice {
    type Err = ParseBackendError;

    /// `auto` or a [`BackendKind`] name.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text == "auto" {
            Ok(Self::Auto)
        } else {
            text.parse().map(Self::Named)
        }
    }
}

/// As its [`fmt::Display`] text, the form `workspace.backend` takes.
impl Serialize for BackendChoice {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for BackendChoice {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Where one workspace's files live on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspacePaths {
    /// The workspace.
    pub id: WorkspaceId,
    /// The directory the agent works in: the tree and nothing else.
    pub root: PathBuf,
    /// An overlay backend's upper (writable) directory, a sibling of the
    /// root. Unused by clone and copy backends.
    pub upper: PathBuf,
    /// An overlay backend's work directory, a sibling of the root. Unused by
    /// clone and copy backends.
    pub work: PathBuf,
    /// jj's working-copy state for the workspace, outside the tree.
    pub state_dir: PathBuf,
}

impl WorkspacePaths {
    /// The default layout under `repo_dir` (`<state>/repos/<repo-id>`):
    /// `ws/<id>`, `ws/<id>.upper`, `ws/<id>.work`, and `wc/<id>`.
    pub fn under(repo_dir: &Path, id: WorkspaceId) -> Self {
        let hex = id.as_hex();
        Self::at(
            id,
            repo_dir.join("ws").join(&hex),
            repo_dir.join("wc").join(hex),
        )
    }

    /// A workspace whose root is `root`, anywhere (a `--path` workspace, or
    /// a configured `workspace.root`), with the overlay directories as its
    /// siblings `<root>.upper` and `<root>.work`, and its working-copy state
    /// at `state_dir`.
    pub fn at(id: WorkspaceId, root: PathBuf, state_dir: PathBuf) -> Self {
        let sibling = |suffix: &str| {
            let mut name = root.as_os_str().to_owned();
            name.push(suffix);
            PathBuf::from(name)
        };
        Self {
            id,
            upper: sibling(".upper"),
            work: sibling(".work"),
            root,
            state_dir,
        }
    }
}

/// How much disk a workspace uses beyond its base. An estimate, documented
/// as one: phase targets are measured by free-space deltas (R1.12, R1.23),
/// not by this.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskUsage {
    /// Allocated bytes (`st_blocks * 512`): of the upper directory for
    /// overlay backends; of files that differ from the base for clone and
    /// copy backends.
    pub allocated_bytes: u64,
    /// Files added, modified, or deleted relative to the base.
    pub files_changed: u64,
}

/// What [`Backend::swap_base`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapOutcome {
    /// The backend replaced the whole tree (overlays swap the lower layer
    /// and empty the upper). The daemon then resets the working copy to the
    /// new commit (`locked_wc.reset`).
    TreeReplaced,
    /// The directory is untouched; the daemon applies the tree diff with
    /// `locked_wc.check_out` (clone and copy backends).
    NeedsCheckout,
}

/// Failure of a backend operation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BackendError {
    /// The backend cannot serve this directory, or is not implemented yet.
    #[error("backend {kind} is not available here: {reason}")]
    Unavailable {
        /// The backend.
        kind: BackendKind,
        /// Why not, for the user.
        reason: String,
    },
    /// `materialize` was asked for a root that already exists.
    #[error("workspace root {} already exists", .0.display())]
    RootExists(PathBuf),
    /// `tear_down` was asked for a root that is already gone.
    #[error("workspace root {} does not exist", .0.display())]
    RootMissing(PathBuf),
    /// A filesystem operation failed.
    #[error("{op} {}", path.display())]
    Io {
        /// What was being done, such as `walk`.
        op: &'static str,
        /// The path it was done to.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// Mounting or unmounting an overlay failed (R1.09).
    #[error("mount {}", path.display())]
    Mount {
        /// The mount point.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// An external tool such as `fuse-overlayfs` failed (R1.09).
    #[error("{tool} exited with {status}: {stderr}")]
    Tool {
        /// The program.
        tool: String,
        /// Its exit status, as displayed.
        status: String,
        /// Its standard error.
        stderr: String,
    },
    /// The base's `tree_state` could not be read (disk usage).
    #[error(transparent)]
    TreeState(#[from] TreeStateReadError),
}

impl Classify for BackendError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Unavailable { .. } => ErrorKind::Unavailable,
            Self::RootExists(_) => ErrorKind::AlreadyExists,
            Self::RootMissing(_) => ErrorKind::NotFound,
            Self::Io { source, .. } | Self::Mount { source, .. } => classify_by_source(source),
            Self::Tool { .. } => ErrorKind::Internal,
            Self::TreeState(error) => error.kind(),
        }
    }
}

/// A way of materializing workspaces from base cache entries.
pub trait Backend: Send + Sync {
    /// Which backend this is.
    fn kind(&self) -> BackendKind;

    /// Makes `ws.root` (which must not exist: [`BackendError::RootExists`])
    /// hold `base`'s files, with every file's mtime and size preserved, so
    /// the base's `tree_state` describes it. Never writes under the base.
    fn materialize(&self, base: &BaseEntry, ws: &WorkspacePaths) -> Result<(), BackendError>;

    /// Estimates the disk the workspace uses beyond `base`.
    fn disk_usage(&self, base: &BaseEntry, ws: &WorkspacePaths) -> Result<DiskUsage, BackendError>;

    /// Moves the workspace from base `from` to base `to` (a workspace
    /// move, R1.06). See [`SwapOutcome`] for what the daemon does next.
    fn swap_base(
        &self,
        ws: &WorkspacePaths,
        from: &BaseEntry,
        to: &BaseEntry,
    ) -> Result<SwapOutcome, BackendError>;

    /// Removes `ws.root`, `ws.upper`, and `ws.work`, never following a
    /// symlink out of them. [`BackendError::RootMissing`] if the root does
    /// not exist.
    fn tear_down(&self, ws: &WorkspacePaths) -> Result<(), BackendError>;
}

/// Constructs the backend for `kind` without probing. Each backend ticket
/// replaces its placeholder arm; a placeholder fails every operation with
/// [`BackendError::Unavailable`].
pub fn backend_for(kind: BackendKind) -> Box<dyn Backend> {
    match kind {
        BackendKind::Copy => Box::new(copy::CopyBackend),
        BackendKind::ApfsClone => Box::new(Pending {
            kind,
            ticket: "R1.08",
        }),
        BackendKind::Reflink => Box::new(Pending {
            kind,
            ticket: "R1.10",
        }),
        BackendKind::Overlayfs | BackendKind::FuseOverlayfs => Box::new(Pending {
            kind,
            ticket: "R1.09",
        }),
    }
}

/// Picks the backend for a workspace root under `dir` (which need not exist
/// yet; its nearest existing ancestor is probed): the named one if its probe
/// says it is available, or for [`BackendChoice::Auto`] the first available
/// in [`BackendKind::AUTO_ORDER`].
///
/// When `auto` falls through to the copy backend, logs one `warn` per `dir`
/// for the daemon's lifetime naming why every other backend was
/// unavailable, so a user who expected copy-on-write finds out.
pub fn select(choice: BackendChoice, dir: &Path) -> Result<Box<dyn Backend>, BackendError> {
    let probes = probe(dir);
    let backend = select_from(choice, &probes)?;
    if choice == BackendChoice::Auto && backend.kind() == BackendKind::Copy {
        warn_copy_fallback_once(dir, &probes);
    }
    Ok(backend)
}

/// The directories [`select`] has warned about.
static COPY_FALLBACK_WARNED: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn warn_copy_fallback_once(dir: &Path, probes: &[Probe]) {
    let first = COPY_FALLBACK_WARNED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(dir.to_owned());
    if !first {
        return;
    }
    let reasons: Vec<String> = probes
        .iter()
        .filter_map(|probe| match &probe.availability {
            Availability::Unavailable(reason) => Some(format!("{}: {reason}", probe.kind)),
            Availability::Available => None,
        })
        .collect();
    tracing::warn!(
        path = %dir.display(),
        unavailable = %reasons.join("; "),
        "no copy-on-write backend is available; workspaces are full copies"
    );
}

/// [`select`] over probes the caller already has, such as the ones
/// `Daemon.Status` reports, or stubs in tests.
pub fn select_from(
    choice: BackendChoice,
    probes: &[Probe],
) -> Result<Box<dyn Backend>, BackendError> {
    let availability = |kind: BackendKind| {
        probes.iter().find(|probe| probe.kind == kind).map_or_else(
            || Availability::Unavailable("not probed".to_owned()),
            |probe| probe.availability.clone(),
        )
    };
    let kind = match choice {
        BackendChoice::Named(kind) => match availability(kind) {
            Availability::Available => kind,
            Availability::Unavailable(reason) => {
                return Err(BackendError::Unavailable { kind, reason });
            }
        },
        BackendChoice::Auto => BackendKind::AUTO_ORDER
            .into_iter()
            .find(|kind| availability(*kind).is_available())
            // Copy is always available, so this needs stubbed probes; the
            // error names it as the last resort that also failed.
            .ok_or_else(|| BackendError::Unavailable {
                kind: BackendKind::Copy,
                reason: "no backend is available".to_owned(),
            })?,
    };
    tracing::debug!(backend = kind.as_str(), "backend selected");
    Ok(backend_for(kind))
}

/// Placeholder for a backend whose ticket has not landed.
struct Pending {
    kind: BackendKind,
    ticket: &'static str,
}

impl Pending {
    fn unavailable<T>(&self) -> Result<T, BackendError> {
        Err(not_implemented(self.kind, self.ticket))
    }
}

/// [`BackendError::Unavailable`] for a backend implemented by `ticket`.
pub(crate) fn not_implemented(kind: BackendKind, ticket: &str) -> BackendError {
    BackendError::Unavailable {
        kind,
        reason: format!("not implemented until {ticket}"),
    }
}

impl Backend for Pending {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn materialize(&self, _: &BaseEntry, _: &WorkspacePaths) -> Result<(), BackendError> {
        self.unavailable()
    }

    fn disk_usage(&self, _: &BaseEntry, _: &WorkspacePaths) -> Result<DiskUsage, BackendError> {
        self.unavailable()
    }

    fn swap_base(
        &self,
        _: &WorkspacePaths,
        _: &BaseEntry,
        _: &BaseEntry,
    ) -> Result<SwapOutcome, BackendError> {
        self.unavailable()
    }

    fn tear_down(&self, _: &WorkspacePaths) -> Result<(), BackendError> {
        self.unavailable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_through_their_names() {
        for kind in BackendKind::AUTO_ORDER {
            assert_eq!(kind.as_str().parse::<BackendKind>().unwrap(), kind);
            assert_eq!(kind.to_string(), kind.as_str());
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            assert_eq!(serde_json::from_str::<BackendKind>(&json).unwrap(), kind);
            assert_eq!(
                kind.as_str().parse::<BackendChoice>().unwrap(),
                BackendChoice::Named(kind)
            );
        }
        assert_eq!(
            "apfs".parse::<BackendKind>().unwrap(),
            BackendKind::ApfsClone
        );
        assert_eq!(
            "auto".parse::<BackendChoice>().unwrap(),
            BackendChoice::Auto
        );
        let error = "zfs".parse::<BackendKind>().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidArgument);
        assert!("auto".parse::<BackendKind>().is_err());
        assert!("Copy".parse::<BackendChoice>().is_err());
    }

    #[test]
    fn workspace_paths_follow_the_layout() {
        let id: WorkspaceId = "00000000000000ab".parse().unwrap();
        let paths = WorkspacePaths::under(Path::new("/state/repos/r"), id);
        assert_eq!(paths.root, Path::new("/state/repos/r/ws/00000000000000ab"));
        assert_eq!(
            paths.upper,
            Path::new("/state/repos/r/ws/00000000000000ab.upper")
        );
        assert_eq!(
            paths.work,
            Path::new("/state/repos/r/ws/00000000000000ab.work")
        );
        assert_eq!(
            paths.state_dir,
            Path::new("/state/repos/r/wc/00000000000000ab")
        );
    }

    #[test]
    fn errors_classify() {
        let io = |kind| BackendError::Io {
            op: "walk",
            path: "/x".into(),
            source: io::Error::from(kind),
        };
        assert_eq!(
            io(io::ErrorKind::PermissionDenied).kind(),
            ErrorKind::PermissionDenied
        );
        assert_eq!(io(io::ErrorKind::Other).kind(), ErrorKind::Io);
        assert_eq!(
            BackendError::RootExists("/x".into()).kind(),
            ErrorKind::AlreadyExists
        );
        assert_eq!(
            BackendError::RootMissing("/x".into()).kind(),
            ErrorKind::NotFound
        );
        assert_eq!(
            not_implemented(BackendKind::Reflink, "R1.10").kind(),
            ErrorKind::Unavailable
        );
        let tool = BackendError::Tool {
            tool: "fuse-overlayfs".into(),
            status: "exit status: 1".into(),
            stderr: "fuse: device not found".into(),
        };
        assert_eq!(tool.kind(), ErrorKind::Internal);
    }

    #[test]
    fn placeholders_are_unavailable() {
        for kind in BackendKind::AUTO_ORDER {
            let backend = backend_for(kind);
            assert_eq!(backend.kind(), kind);
        }
        let id: WorkspaceId = "00000000000000ab".parse().unwrap();
        let ws = WorkspacePaths::under(Path::new("/nonexistent"), id);
        let error = backend_for(BackendKind::Overlayfs)
            .tear_down(&ws)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "backend overlayfs is not available here: not implemented until R1.09"
        );
    }
}
