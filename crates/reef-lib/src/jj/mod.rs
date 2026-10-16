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

pub mod materialize;
pub mod oplog;
pub mod repo;
pub mod settings;
pub mod snapshot;
pub mod undo;
pub mod workspace;

pub use materialize::{MaterializeError, MaterializeStats, TreeStateEntry, TreeStateReadError};
pub use repo::{
    ChangeIdHex, ChangeSummary, CommitIdHex, InitError, OpenError, OperationIdHex, ParseHexIdError,
    Repo,
};

// jj-lib types the daemon names without depending on jj-lib: the repository
// thread's state (R1.03) and the cross-thread `Send + Sync` assertion on them.
pub use jj_lib::merged_tree::MergedTree;
pub use jj_lib::repo::ReadonlyRepo;
pub use jj_lib::settings::UserSettings;
pub use workspace::{
    CreateWorkspaceError, ForgetWorkspaceError, ResolveError, Resolved, ResolvedVia, StartPoint,
};
