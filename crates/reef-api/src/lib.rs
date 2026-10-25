//! The Reef daemon API (ADR 0029): the `reef.daemon.v1` protobuf schema, the
//! code generated from it, the Rust [`client`], the JSON form of its
//! responses ([`json`]), and the error kinds every Reef crate classifies its
//! errors by ([`error`], [`advice`]).
//!
//! This is the bottom crate: `reef-lib`, `reef-daemon`, and `reef-vcs`
//! depend on it, and it depends on no other Reef crate and never on jj-lib,
//! so an SDK, the MCP server, or any other client can reach the daemon
//! without linking it.
//!
//! See <https://github.com/reef-vcs/reef> for the design document and roadmap.

use std::sync::LazyLock;

pub mod advice;
pub mod client;
pub mod error;
pub mod json;
pub mod page;
pub mod proto;

/// The `FileDescriptorSet` of every `reef/daemon/v1` file, as `protox`
/// compiled it: with source info (every comment) and the imported
/// well-known types. Served by the daemon's reflection service.
pub const DESCRIPTOR_SET: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/reef.daemon.v1.bin"));

/// BLAKE3 of [`DESCRIPTOR_SET`], 64 lowercase hex characters: which build of
/// the schema a daemon was compiled with (`Daemon.Status.schema_hash`).
pub static SCHEMA_HASH: LazyLock<String> =
    LazyLock::new(|| blake3::hash(DESCRIPTOR_SET).to_hex().to_string());
