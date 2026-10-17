//! The Reef daemon: the long-running local service that owns snapshots,
//! workspaces, merge, and queries on behalf of agents.
//!
//! It hosts the typed API, workspace materialization backends, the lease
//! manager, filesystem watchers, and logging initialization. The data model
//! and merge logic live in `reef-lib`; this crate wires them into a service.
//! The `reef` binary (`reef-vcs`) depends on this crate; this crate never
//! depends on it.
//!
//! See <https://github.com/reef-vcs/reef> for the design document and roadmap.

pub mod backend;
pub mod client;
pub mod config;
pub mod daemon;
pub mod db;
pub mod lock;
pub mod logging;
pub mod model;
pub mod proto;
pub mod repo;
mod run;
pub mod server;
pub mod service;
#[cfg(feature = "testing")]
pub mod testing;

pub use daemon::{AppState, Daemon, DaemonHandle, StartError};
pub use run::{DaemonArgs, exit_code, run_main};
