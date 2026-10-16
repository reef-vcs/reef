//! Reef core library: the data model, jj-lib embedding, workspace model,
//! merge engine, and query layer.
//!
//! This crate contains no server and no CLI code. Everything here must be
//! usable by a process that is not the daemon: tests, benchmarks, and future
//! tooling. The daemon (`reef-daemon`) and the `reef` binary (`reef-vcs`)
//! depend on this crate; it depends on neither.
//!
//! See <https://github.com/reef-vcs/reef> for the design document and roadmap.

pub mod error;
pub mod jj;
pub mod telemetry;
