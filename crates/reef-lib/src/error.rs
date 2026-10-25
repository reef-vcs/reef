//! Error conventions shared by every Reef crate (ADR 0009): `ErrorKind`,
//! `Classify`, `classify_by_source`, and `ErrorChain`, defined in
//! `reef_api::error` since R2.01 (ADR 0029) so clients classify errors
//! without linking this crate, and re-exported here so every crate keeps
//! importing them from `reef_lib::error`.

pub use reef_api::error::*;
