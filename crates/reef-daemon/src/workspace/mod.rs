//! Workspaces (R1.05): the [`Manager`] that creates, snapshots, inspects,
//! and releases them, joining jj workspaces (`reef_lib::jj`), the base
//! cache and backends ([`crate::backend`]), the repository threads
//! ([`crate::repo`]), and the metadata database ([`crate::db`]).
//!
//! The manager's signatures use the daemon's own types; `service/`
//! converts to and from the generated messages.
//!
//! # Threads
//!
//! Each step runs where ADR 0012 puts it: jj transactions, reloads, ref
//! imports, and base materialization on the repository's thread
//! ([`RepoHandle`]); scans of one workspace and backend calls on tokio's
//! blocking pool, holding that workspace's jj handle; database writes on
//! the async workers (they are short). The repository thread is blocked for
//! the duration of a cold base materialization; other repositories and
//! `Status` are not. A create waiting for another create of the same tree
//! waits on the base cache's per-tree lock, not on the repository thread.

mod create;
mod manager;
mod release;
mod snapshot;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use reef_lib::error::{Classify, ErrorKind};
use reef_lib::jj::repo::ReadError;
use reef_lib::jj::snapshot::SnapshotError;
use reef_lib::jj::workspace::{CommitLookupError, ResetError};
use reef_lib::jj::{
    CreateWorkspaceError, ForgetWorkspaceError, ImportRefsError, OpenError, Repo, ResolveError,
};

pub use manager::Manager;

use crate::backend::{
    Backend, BackendChoice, BackendError, BaseCacheError, DiskUsage, WorkspacePaths,
};
use crate::db::DbError;
use crate::model::{LeaseRecord, RepoId, WorkspaceId, WorkspaceRecord, WorkspaceState};
use crate::repo::{RegisterError, RepoHandle, RepoThreadError};

/// One loaded workspace: what the manager holds for every `active` row.
pub struct WorkspaceEntry {
    /// The row as last written by the manager.
    pub record: Mutex<WorkspaceRecord>,
    /// Where its files are.
    pub paths: WorkspacePaths,
    /// The backend that materialized it.
    pub backend: Box<dyn Backend>,
    /// The workspace's own jj handle, `None` if it could not be reopened
    /// after a restart or once it is released. Locked by scans on the
    /// blocking pool; never held across an `.await`.
    pub jj: Arc<Mutex<Option<Repo>>>,
    /// Its repository's thread.
    pub repo: RepoHandle,
    /// Serializes snapshots and release of this workspace.
    pub scan_lock: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for WorkspaceEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceEntry")
            .field("paths", &self.paths)
            .field("backend", &self.backend.kind())
            .finish_non_exhaustive()
    }
}

impl WorkspaceEntry {
    /// A copy of the row as last written.
    pub fn record(&self) -> WorkspaceRecord {
        self.record
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn update_record(&self, f: impl FnOnce(&mut WorkspaceRecord)) {
        f(&mut self.record.lock().unwrap_or_else(PoisonError::into_inner));
    }
}

/// Where a new workspace's working-copy commit starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    /// Edit the commit this revision names.
    Change(String),
    /// A new change on top of the commit this revision names.
    NewOn(String),
    /// A new change on the `main` bookmark if it exists, else on the
    /// `default` workspace's working-copy commit.
    Default,
}

/// What [`Manager::create`] is asked for.
#[derive(Clone, Debug)]
pub struct CreateSpec {
    /// The repository.
    pub repo: RepoId,
    /// Where the working-copy commit starts.
    pub start: Start,
    /// Who holds it; must not be empty.
    pub owner: String,
    /// Free text.
    pub purpose: String,
    /// The backend; `None` for `workspace.backend`.
    pub backend: Option<BackendChoice>,
    /// The directory to create it at (absolute, must not exist); `None` for
    /// `<workspace.root>/<id>`.
    pub path: Option<PathBuf>,
    /// The lease length; `None` for `lease.default`. Above `lease.max` is
    /// [`ManagerError::LeaseTooLong`].
    pub lease: Option<Duration>,
}

/// What [`Manager::create`] made.
#[derive(Clone, Debug)]
pub struct Created {
    /// The new row.
    pub record: WorkspaceRecord,
    /// Its lease.
    pub lease: LeaseRecord,
    /// The operation that created the jj workspace.
    pub op_id: String,
    /// Another Reef workspace's working-copy commit was already the edited
    /// commit.
    pub shared_change: bool,
    /// Wall time of ensuring the base and the backend's materialization.
    pub materialize: Duration,
    /// Whether the base cache already held the tree.
    pub base_was_cached: bool,
}

/// How [`Manager::release`] releases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseSpec {
    /// Release even if the final snapshot or forgetting the jj workspace
    /// fails.
    pub force: bool,
    /// Take a final snapshot first (the default).
    pub final_snapshot: bool,
}

impl Default for ReleaseSpec {
    fn default() -> Self {
        Self {
            force: false,
            final_snapshot: true,
        }
    }
}

/// What [`Manager::release`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Released {
    /// The operation holding the final snapshot, if it recorded one.
    pub final_op_id: Option<String>,
    /// Whether a final snapshot ran (it may have found no changes).
    pub snapshot_taken: bool,
    /// Bytes the workspace had allocated, estimated before tear-down.
    pub freed_bytes: u64,
}

/// What [`Manager::snapshot`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshotted {
    /// The snapshot's operation; `None` when nothing changed.
    pub op_id: Option<String>,
    /// Whether the tree changed.
    pub changed: bool,
    /// The working-copy commit afterwards.
    pub commit_id: String,
}

/// What [`Manager::inspect`] found.
#[derive(Clone, Debug)]
pub struct Inspected {
    /// The row (after the snapshot, if one was taken).
    pub record: WorkspaceRecord,
    /// Its unended lease.
    pub lease: Option<LeaseRecord>,
    /// Its disk usage beyond its base, for a loaded workspace whose base is
    /// still cached.
    pub disk_usage: Option<DiskUsage>,
    /// The other active workspaces on the same change.
    pub other_workspaces_on_change: Vec<WorkspaceId>,
    /// The operation `snapshot_first` recorded, if it recorded one.
    pub op_id: Option<String>,
}

/// Failure of a workspace operation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManagerError {
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// A backend failed.
    #[error(transparent)]
    Backend(#[from] BackendError),
    /// The base cache failed.
    #[error(transparent)]
    BaseCache(#[from] BaseCacheError),
    /// jj could not create the workspace.
    #[error(transparent)]
    CreateWorkspace(#[from] CreateWorkspaceError),
    /// A snapshot failed.
    #[error("snapshot failed")]
    Snapshot(#[from] SnapshotError),
    /// The repository could not be registered.
    #[error(transparent)]
    Register(#[from] RegisterError),
    /// A job on the repository thread failed.
    #[error(transparent)]
    RepoThread(#[from] RepoThreadError),
    /// The start revision did not resolve.
    #[error("resolve the start revision")]
    Resolve(#[from] ResolveError),
    /// Importing a colocated repository's git refs failed.
    #[error(transparent)]
    ImportRefs(#[from] ImportRefsError),
    /// A repository or workspace could not be opened or reloaded.
    #[error(transparent)]
    Open(#[from] OpenError),
    /// The jj workspace could not be forgotten.
    #[error(transparent)]
    Forget(#[from] ForgetWorkspaceError),
    /// A working-copy commit could not be read.
    #[error(transparent)]
    Read(#[from] ReadError),
    /// A commit could not be read.
    #[error(transparent)]
    CommitLookup(#[from] CommitLookupError),
    /// The working-copy state could not be reset.
    #[error(transparent)]
    Reset(#[from] ResetError),
    /// The workspace's path already exists.
    #[error("{} already exists", .0.display())]
    PathExists(PathBuf),
    /// The workspace's path is not absolute.
    #[error("workspace path {} is not absolute", .0.display())]
    PathNotAbsolute(PathBuf),
    /// The request named no owner.
    #[error("owner must not be empty")]
    EmptyOwner,
    /// The requested lease is longer than `lease.max`.
    #[error("a lease of {requested:?} is longer than the maximum {max:?}")]
    LeaseTooLong {
        /// What was asked for.
        requested: Duration,
        /// `lease.max`.
        max: Duration,
    },
    /// The request named no repository where one is required.
    #[error("a repository must be named")]
    RepoRequired,
    /// The request named no workspace.
    #[error("a workspace must be named")]
    WorkspaceRequired,
    /// The repository's thread is not running (it failed to open at start,
    /// or was forgotten).
    #[error("repository {0} is not open")]
    RepoNotOpen(RepoId),
    /// The workspace is not `active`.
    #[error("workspace {id} is {}", state.as_str())]
    NotActive {
        /// The workspace.
        id: WorkspaceId,
        /// Its state.
        state: WorkspaceState,
    },
    /// The workspace's jj handle could not be reopened at start; only a
    /// forced release without a final snapshot can remove it.
    #[error("workspace {0} is not loaded")]
    NotLoaded(WorkspaceId),
    /// The final snapshot before a release failed; the workspace is kept,
    /// marked `final-snapshot-failed`.
    #[error("final snapshot of workspace {id} failed; release with force to discard it")]
    FinalSnapshotFailed {
        /// The workspace.
        id: WorkspaceId,
        /// Why.
        #[source]
        source: Box<ManagerError>,
    },
    /// A blocking task panicked or was cancelled.
    #[error("a workspace task failed")]
    Task(#[from] tokio::task::JoinError),
}

impl Classify for ManagerError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Db(error) => error.kind(),
            Self::Backend(error) => error.kind(),
            Self::BaseCache(error) => error.kind(),
            Self::CreateWorkspace(error) => error.kind(),
            Self::Snapshot(error) => error.kind(),
            Self::Register(error) => error.kind(),
            Self::RepoThread(error) => error.kind(),
            Self::Resolve(error) => error.kind(),
            Self::ImportRefs(error) => error.kind(),
            Self::Open(error) => error.kind(),
            Self::Forget(error) => error.kind(),
            Self::Read(error) => error.kind(),
            Self::CommitLookup(error) => error.kind(),
            Self::Reset(error) => error.kind(),
            Self::FinalSnapshotFailed { source, .. } => source.kind(),
            Self::PathExists(_) => ErrorKind::AlreadyExists,
            Self::PathNotAbsolute(_)
            | Self::EmptyOwner
            | Self::LeaseTooLong { .. }
            | Self::RepoRequired
            | Self::WorkspaceRequired => ErrorKind::InvalidArgument,
            Self::RepoNotOpen(_) | Self::NotLoaded(_) => ErrorKind::Unavailable,
            Self::NotActive { .. } => ErrorKind::Conflict,
            Self::Task(_) => ErrorKind::Internal,
        }
    }
}

/// `duration` in whole milliseconds, saturating.
fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
