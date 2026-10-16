//! Embedding of jj-lib. This module is the only place in Reef that talks to
//! jj-lib types directly; everything above it uses Reef's own types.
//!
//! The API is async because jj-lib's is. jj-lib does blocking I/O inside its
//! futures, so callers on a multi-threaded runtime should run these calls on a
//! blocking-friendly thread (ADR 0008).

pub mod oplog;
pub mod repo;
pub mod settings;
pub mod snapshot;
pub mod undo;

pub use repo::{ChangeIdHex, ChangeSummary, CommitIdHex, OpenError, OperationIdHex, Repo};
