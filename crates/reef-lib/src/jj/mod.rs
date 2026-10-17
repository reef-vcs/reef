//! Embedding of jj-lib. This module is the only place in Reef that talks to
//! jj-lib types directly; everything above it uses Reef's own types.
//!
//! The API is async because jj-lib's is, but the futures are not `Send` and
//! [`Repo`] is `Send` but not `Sync`: jj-lib holds trait objects without those
//! bounds across awaits. The futures also do blocking I/O. A tokio daemon
//! therefore cannot `tokio::spawn` them; it drives them on a dedicated thread,
//! for example `spawn_blocking(move || pollster::block_on(repo.snapshot(..)))`,
//! and moves a `Repo` between threads by ownership or behind a `Mutex`, never
//! shares one through an `Arc` alone (ADR 0008, R1.03).

pub mod gc;
pub mod git;
pub mod ignore;
pub mod materialize;
pub mod oplog;
pub mod repo;
pub mod settings;
pub mod snapshot;
pub mod undo;
pub mod workspace;

pub use gc::{GcError, GcOutcome, WorkingCopyCheckError};
pub use git::{ImportRefsError, ImportRefsOutcome};
pub use ignore::IgnoreSources;
pub use materialize::{MaterializeError, MaterializeStats, TreeStateEntry, TreeStateReadError};
pub use repo::{
    ChangeIdHex, ChangeSummary, CommitIdHex, InitError, OpenError, OperationIdHex, ParseHexIdError,
    Repo,
};
pub use snapshot::{
    Behind, PendingSnapshot, SnapshotCommit, SnapshotError, SnapshotPlan, commit_snapshot,
};

// Watchman (R1.14): jj-lib's monitor, and the client crate at the version
// jj-lib uses, for the daemon's trigger subscriptions.
#[cfg(feature = "watchman")]
pub use jj_lib::fsmonitor::watchman;
#[cfg(feature = "watchman")]
pub use watchman_client;

// jj-lib types the daemon names without depending on jj-lib: the repository
// thread's state (R1.03) and the cross-thread `Send + Sync` assertion on them.
pub use jj_lib::merged_tree::MergedTree;
pub use jj_lib::repo::ReadonlyRepo;
pub use jj_lib::settings::UserSettings;
pub use jj_lib::working_copy::WorkingCopyStateError;
pub use workspace::{
    CreateWorkspaceError, ForgetWorkspaceError, ResolveError, Resolved, ResolvedVia, StartPoint,
};
