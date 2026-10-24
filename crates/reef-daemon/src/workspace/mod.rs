//! Workspaces (R1.05): the [`Manager`] that creates, snapshots, inspects,
//! and releases or reclaims them (R1.15), joining jj workspaces (`reef_lib::jj`), the base
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

mod abandoned;
mod create;
mod gc;
mod manager;
mod move_;
mod reclaim;
mod rematerialize;
mod snapshot;
mod stores;

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use reef_lib::error::{Classify, ErrorKind, classify_by_source};
use reef_lib::jj::repo::ReadError;
use reef_lib::jj::snapshot::SnapshotError;
use reef_lib::jj::workspace::{CommitLookupError, ResetError};
use reef_lib::jj::{
    CheckoutSummary, CheckoutTreeError, CreateWorkspaceError, ForgetWorkspaceError,
    ImportRefsError, MovePointerError, OpenError, Repo, ResolveError,
};
use reef_lib::spec::SpecError;

pub use create::ROOT_CHECKOUT;
pub use gc::JjStep;
pub(crate) use gc::measure;
pub use manager::Manager;
pub use snapshot::REEFIGNORE;
pub(crate) use stores::StoreTarget;

use crate::backend::{
    Backend, BackendChoice, BackendError, BaseCacheError, DiskUsage, WorkspacePaths,
};
use crate::db::DbError;
use crate::error::{Advice, Advise};
use crate::lease::LeaseError;
use crate::model::{LeaseRecord, RepoId, WorkspaceId, WorkspaceRecord, WorkspaceState};
use crate::repo::{RegisterError, RepoHandle, RepoThreadError};
use crate::snapshot::index::SnapshotEntry;
use crate::store::{MountedStore, StoreError};

/// One loaded workspace: what the manager holds for every `active` row.
pub struct WorkspaceEntry {
    /// The row as last written by the manager.
    pub record: Mutex<WorkspaceRecord>,
    /// Where its files are. `store_mounts` is empty: the mounts change
    /// with a move (R1.06), so disk usage takes them from `stores`.
    pub paths: WorkspacePaths,
    /// The backend that materialized it.
    pub backend: Box<dyn Backend>,
    /// The workspace's own jj handle, `None` if it could not be reopened
    /// after a restart or once it is released. Locked by scans on the
    /// blocking pool; never held across an `.await`.
    pub jj: Arc<Mutex<Option<Repo>>>,
    /// The [`RepoHandle::generation`] read before `jj` was opened: when the
    /// repository's is later, `Manager::workspace_head` reopens `jj` on
    /// the repository's new store.
    pub jj_generation: AtomicU64,
    /// Its repository's thread.
    pub repo: RepoHandle,
    /// Serializes snapshots and release of this workspace.
    pub scan_lock: tokio::sync::Mutex<()>,
    /// What its snapshots exclude beyond git's ignore rules.
    pub spec: Mutex<SnapshotSpec>,
    /// The shared stores mounted in it (R1.20).
    pub stores: Mutex<Vec<MountedStore>>,
}

/// The parts of a workspace's spec its snapshots and watcher apply, from
/// the `reef.toml` of the tree it was created on (`crate::spec::resolve`);
/// empty when that tree has none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapshotSpec {
    /// `[snapshot] exclude`, in gitignore syntax, repository-root relative.
    pub excludes: Vec<String>,
    /// Shared store mount points (`[[store]] mount`), repository-root
    /// relative.
    pub mounts: Vec<String>,
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

    /// A copy of its snapshot spec.
    pub fn spec(&self) -> SnapshotSpec {
        self.spec
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Sets `attention` on the loaded row (the database is the caller's).
    pub(crate) fn set_attention(&self, attention: Option<crate::model::Attention>) {
        self.update_record(|record| record.attention = attention);
    }

    /// A copy of its mounted stores.
    pub fn stores(&self) -> Vec<MountedStore> {
        self.stores
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// What [`Manager::create`] does when a store the spec declares has no
/// sealed build for the lockfile's hash (R1.20).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MissingStores {
    /// Fail the create with [`StoreError::Missing`].
    #[default]
    Fail,
    /// Create the workspace without the store, marked `store-missing`.
    Skip,
}

/// Where a new workspace's working-copy commit starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    /// Edit the commit this revision names.
    Change(String),
    /// A new change on top of the commit this revision names.
    NewOn(String),
    /// A new change on the default start (R1.31): the first bookmark of
    /// `main`, origin's HEAD, `master`, `trunk`, then the same on `origin`;
    /// else the `default` workspace's working-copy commit.
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
    /// [`LeaseError::TooLong`].
    pub lease: Option<Duration>,
    /// What to do about a store with no sealed build (R1.20).
    pub missing_stores: MissingStores,
    /// A store left out without counting as missing: the one a store
    /// build's scratch workspace is building (R1.20).
    pub skip_store: Option<String>,
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
    /// The bookmark a create without a start revision chose, or
    /// [`ROOT_CHECKOUT`]; `None` when the request named one (R1.31).
    pub default_start: Option<String>,
    /// How long each step took.
    pub timings: CreateTimings,
    /// Whether the base cache already held the tree.
    pub base_was_cached: bool,
    /// The shared stores mounted in it (R1.20).
    pub stores: Vec<MountedStore>,
}

/// How long each step of [`Manager::create`] took, in milliseconds (R1.29).
/// The steps are those of `workspace/create.rs`; step 1 counts only in the
/// total.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CreateTimings {
    /// Step 2: resolving the start revision, reading `reef.toml`, and
    /// looking the stores up.
    pub resolve_ms: u64,
    /// Step 3: ensuring the base.
    pub base_ms: u64,
    /// Step 4: the backend's materialization alone.
    pub materialize_ms: u64,
    /// Step 4: mounting the stores.
    pub mount_ms: u64,
    /// Step 5: the jj workspace.
    pub jj_ms: u64,
    /// Step 6: seeding the working copy's `tree_state`.
    pub seed_ms: u64,
    /// Step 7: the row and the lease.
    pub insert_ms: u64,
    /// Loading the workspace and attaching its watcher.
    pub watch_ms: u64,
    /// The whole create.
    pub total_ms: u64,
}

/// What [`Manager::move_to`] did (R1.06).
#[derive(Clone, Debug)]
pub struct Moved {
    /// The row afterwards: same id, path, lease, and backend; the new
    /// change, commit, and base tree.
    pub record: WorkspaceRecord,
    /// Its lease, untouched.
    pub lease: Option<LeaseRecord>,
    /// The operation that moved the pointer; `None` when the target was the
    /// workspace's working-copy commit already.
    pub op_id: Option<String>,
    /// The operation holding the final snapshot of the change it left;
    /// `None` when that snapshot found nothing to record.
    pub final_op_id: Option<String>,
    /// The files a check-out wrote (clone and copy backends); zero when the
    /// backend replaced the tree.
    pub checkout: CheckoutSummary,
    /// The backend replaced the whole tree (overlay backends).
    pub tree_replaced: bool,
    /// The target's shared stores, mounted (R1.20).
    pub stores: Vec<MountedStore>,
}

/// How [`Manager::reclaim`] releases or reclaims. The expiry loop uses the
/// default; `Release` passes its request's flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseSpec {
    /// Release even if the final snapshot or forgetting the jj workspace
    /// fails.
    pub force: bool,
    /// Take a final snapshot first (the default).
    pub final_snapshot: bool,
    /// Recorded in the `reclaims` row's detail, such as `store build` for a
    /// store build's scratch workspace (R1.20).
    pub detail: Option<&'static str>,
}

impl Default for ReleaseSpec {
    fn default() -> Self {
        Self {
            force: false,
            final_snapshot: true,
            detail: None,
        }
    }
}

/// What [`Manager::reclaim`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimOutcome {
    /// Whether a final snapshot ran (it may have found no changes).
    pub snapshot_taken: bool,
    /// The operation holding the final snapshot, if it recorded one.
    pub final_op_id: Option<String>,
    /// Bytes the workspace had allocated, estimated before tear-down;
    /// `None` when the backend could not measure them.
    pub freed_bytes: Option<u64>,
    /// The operation that forgot the jj workspace, if one was recorded.
    pub release_op_id: Option<String>,
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
    /// Its indexed snapshot operations, and how many are local (R1.16).
    pub snapshot_count: u64,
    /// See `snapshot_count`.
    pub local_snapshot_count: u64,
    /// The newest [`crate::snapshot::index::RECENT_LIMIT`] of them, newest
    /// first.
    pub recent_snapshots: Vec<SnapshotEntry>,
    /// Its mounted stores, for a loaded workspace (R1.20).
    pub stores: Vec<MountedStore>,
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
    /// The commit's tree has an entry jj cannot read (RX.01).
    #[error(transparent)]
    TreeName(#[from] reef_lib::jj::TreeNameError),
    /// The target commit's `reef.toml` is invalid or could not be read.
    #[error("read the workspace spec")]
    Spec(#[from] SpecError),
    /// The working-copy state could not be reset.
    #[error(transparent)]
    Reset(#[from] ResetError),
    /// The workspace's path already exists.
    #[error("{} already exists", .0.display())]
    PathExists(PathBuf),
    /// Another workspace's row records this path. A released or reclaimed
    /// workspace keeps its path until garbage collection deletes the row
    /// (R1.17), because `workspaces.path` is unique.
    #[error(
        "{} is recorded for workspace {id} ({}); a released workspace keeps its path until garbage collection",
        path.display(),
        state.as_str()
    )]
    PathRecorded {
        /// The path.
        path: PathBuf,
        /// The workspace whose row holds it.
        id: WorkspaceId,
        /// That workspace's state.
        state: WorkspaceState,
    },
    /// The workspace's path is not absolute.
    #[error("workspace path {} is not absolute", .0.display())]
    PathNotAbsolute(PathBuf),
    /// The request named no owner.
    #[error("owner must not be empty")]
    EmptyOwner,
    /// The lease could not be issued.
    #[error(transparent)]
    Lease(#[from] LeaseError),
    /// An expiry reclaim found the workspace's lease not past its grace (a
    /// heartbeat or the restart grace renewed it after the tick read it, or
    /// it has no lease); nothing was done.
    #[error("the lease of workspace {0} is not past its grace; not reclaimed")]
    LeaseNotExpired(WorkspaceId),
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
        /// Its repository, for the advice to create another.
        repo: RepoId,
        /// Its state.
        state: WorkspaceState,
    },
    /// No registered repository contains an absolute path a request named.
    /// The message is the one R1.25's implicit registration recognizes.
    #[error("repository containing {} not found", .0.display())]
    NotRegistered(PathBuf),
    /// The workspace's jj handle could not be reopened at start, or the
    /// workspace is not loaded yet. A release without a final snapshot
    /// (`skip_final_snapshot`, or `force`) can still remove it.
    #[error("workspace {0} is not loaded")]
    NotLoaded(WorkspaceId),
    /// The final snapshot before a release failed; the workspace is kept,
    /// marked `final-snapshot-failed`.
    #[error("final snapshot of workspace {id} failed")]
    FinalSnapshotFailed {
        /// The workspace.
        id: WorkspaceId,
        /// Why.
        #[source]
        source: Box<ManagerError>,
    },
    /// A stored workspace or commit value does not parse (a row this
    /// binary did not write, R1.18's rematerialization).
    #[error("workspace {id} has an invalid {what} {value:?}")]
    InvalidRecord {
        /// The workspace.
        id: WorkspaceId,
        /// Which field.
        what: &'static str,
        /// What it holds.
        value: String,
    },
    /// A blocking task panicked or was cancelled.
    #[error("a workspace task failed")]
    Task(#[from] tokio::task::JoinError),
    /// The configured watcher cannot run (R1.14: `watcher.backend =
    /// "watchman"` without `watchman` on `PATH`).
    #[error("no watcher for the workspace")]
    Watcher(#[from] crate::watcher::WatchError),
    /// The jj settings for the workspace's watcher could not be built.
    #[error("jj settings for the workspace")]
    Settings(#[from] reef_lib::jj::settings::SettingsError),
    /// GC could not compare a workspace's working copy with its commit
    /// (R1.17).
    #[error("check workspace {id}'s working copy")]
    WorkingCopyCheck {
        /// The workspace.
        id: WorkspaceId,
        /// Why.
        #[source]
        source: reef_lib::jj::WorkingCopyCheckError,
    },
    /// GC could not move a workspace's working copy to the new head
    /// (R1.17).
    #[error("move workspace {id}'s working copy to the new head")]
    Rebind {
        /// The workspace.
        id: WorkspaceId,
        /// Why.
        #[source]
        source: reef_lib::jj::WorkingCopyStateError,
    },
    /// A shared store could not be found, mounted, or built (R1.20).
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A lockfile could not be read from the tree (R1.20).
    #[error("read a store's lockfile")]
    ReadFile(#[from] reef_lib::jj::spec::ReadFileError),
    /// A move's final snapshot failed, so the move was refused and nothing
    /// changed (R1.06).
    #[error("workspace {id} has an unsnapshotted write in flight")]
    WriteInFlight {
        /// The workspace.
        id: WorkspaceId,
        /// Why the snapshot failed.
        #[source]
        source: SnapshotError,
    },
    /// A move could not move the working-copy pointer (R1.06).
    #[error(transparent)]
    MovePointer(#[from] MovePointerError),
    /// A move could not check the target's tree out (R1.06).
    #[error(transparent)]
    CheckoutTree(#[from] CheckoutTreeError),
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
            Self::TreeName(error) => error.kind(),
            Self::Reset(error) => error.kind(),
            Self::Spec(error) => error.kind(),
            Self::FinalSnapshotFailed { source, .. } => source.kind(),
            Self::Lease(error) => error.kind(),
            Self::PathExists(_) | Self::PathRecorded { .. } => ErrorKind::AlreadyExists,
            Self::PathNotAbsolute(_)
            | Self::EmptyOwner
            | Self::RepoRequired
            | Self::WorkspaceRequired => ErrorKind::InvalidArgument,
            Self::RepoNotOpen(_) | Self::NotLoaded(_) => ErrorKind::Unavailable,
            Self::NotActive { .. } | Self::LeaseNotExpired(_) | Self::WriteInFlight { .. } => {
                ErrorKind::Conflict
            }
            Self::NotRegistered(_) => ErrorKind::NotFound,
            Self::Task(_) | Self::InvalidRecord { .. } => ErrorKind::Internal,
            Self::Watcher(error) => error.kind(),
            Self::Settings(error) => error.kind(),
            Self::WorkingCopyCheck { source, .. } => source.kind(),
            Self::Rebind { source, .. } => classify_by_source(source),
            Self::Store(error) => error.kind(),
            Self::ReadFile(error) => error.kind(),
            Self::MovePointer(error) => error.kind(),
            Self::CheckoutTree(error) => error.kind(),
        }
    }
}

impl Advise for ManagerError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Db(error) => error.advice(),
            Self::BaseCache(error) => error.advice(),
            Self::Lease(error) => error.advice(),
            Self::Register(error) => error.advice(),
            Self::Store(error) => error.advice(),
            Self::NotRegistered(path) => Some(unregistered_advice(path)),
            Self::NotActive { id, repo, state } => match state {
                WorkspaceState::Released | WorkspaceState::Reclaimed => {
                    Some(gone_advice(*id, *state, *repo))
                }
                WorkspaceState::Releasing => Some(releasing_advice()),
                WorkspaceState::Active => None,
            },
            Self::FinalSnapshotFailed { id, .. } => Some(Advice::run(
                "The workspace is kept; `reef ws release --no-snapshot` discards its unsnapshotted edits.",
                ["reef", "ws", "inspect", &id.as_hex()],
            )),
            Self::TreeName(_) => Some(Advice::hint(
                "The commit has a tree entry git itself never writes, which jj cannot read; start from another revision.",
            )),
            Self::WriteInFlight { id, .. } => Some(Advice::run(
                "Nothing moved; the workspace's files could not be snapshotted. Inspect it, then retry the move.",
                ["reef", "ws", "inspect", &id.as_hex()],
            )),
            _ => None,
        }
    }
}

/// The advice for a path in no registered repository: register it if it
/// exists, else list what is registered.
fn unregistered_advice(path: &std::path::Path) -> Advice {
    if path.exists() {
        let path = path.display().to_string();
        return Advice::run(
            "Register the repository (a plain git checkout gets jj alongside git).",
            ["reef", "repo", "register", &path],
        );
    }
    Advice::run(
        format!(
            "{} does not exist; name a directory inside a git or jj checkout, or a registered repository's id.",
            path.display()
        ),
        ["reef", "repo", "list"],
    )
}

/// The advice for a workspace being released: wait, since a release whose
/// final snapshot fails puts it back to `active`.
pub(crate) fn releasing_advice() -> Advice {
    Advice::hint("It is being released; retry in a moment (a release that fails leaves it active).")
}

/// The advice for a workspace that was released or reclaimed: its
/// snapshots are kept, its directory is gone, so make another.
pub(crate) fn gone_advice(id: WorkspaceId, state: WorkspaceState, repo: RepoId) -> Advice {
    Advice::run(
        format!(
            "Workspace {id} was {}; its snapshots are kept but its directory is gone. Create a new workspace.",
            state.as_str()
        ),
        ["reef", "ws", "create", "--repo", &repo.as_hex()],
    )
}
