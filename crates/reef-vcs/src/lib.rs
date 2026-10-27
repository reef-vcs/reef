//! Umbrella crate for Reef. Re-exports the API and its thin client, so a
//! Rust program that talks to the daemon can depend on one crate; the
//! daemon itself is `reef-daemon` (R2.05, ADR 0033).

pub use reef_api;
pub use reef_client;
