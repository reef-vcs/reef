//! Reef's reference executor (R3.06, ADR 0060): `reef executor`.
//!
//! An executor claims jobs from a daemon's `ExecutorService` and returns
//! signed evidence, following `docs/executor-protocol.md`. This one:
//!
//! 1. claims a job ([`serve`]), and issues a session key for it, actor
//!    `executor`;
//! 2. reads the job's tree through `FileService` into a private directory,
//!    and checks the listing, every blob and the laid-out disk against git
//!    object ids ([`materialize`], [`githash`]);
//! 3. matches each requested kind and target to a declared check, from the
//!    executor's own `executor.toml` or the tree's `reef-checks.toml`
//!    ([`config`], [`declare`]);
//! 4. runs each check in a sandbox: Seatbelt on macOS, bubblewrap on Linux
//!    ([`sandbox`], [`run`]);
//! 5. uploads the logs and declared files, signs a summary per pair, and
//!    completes the job ([`result`], [`job`]).
//!
//! It links no jj-lib and no daemon: everything goes through
//! [`reef_client::Client`], over the socket or the remote listener.
//! `docs/executor.md` is the operator's guide.

pub mod config;
pub mod declare;
mod error;
pub mod githash;
pub mod job;
pub mod materialize;
pub mod probe;
pub mod report;
pub mod result;
pub mod run;
pub mod sandbox;
pub mod serve;

pub use error::Error;

/// The executor's name in `ExecutorInfo.version` and in its annotations.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The executor protocol version this executor speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The repository file that declares checks at a tree.
pub const CHECKS_FILE: &str = "reef-checks.toml";

/// The executor's configuration file's name, in the client's state
/// directory.
pub const CONFIG_FILE: &str = "executor.toml";

/// The variable that names the configuration file in place of the
/// default.
pub const CONFIG_ENV: &str = "REEF_EXECUTOR_CONFIG";
