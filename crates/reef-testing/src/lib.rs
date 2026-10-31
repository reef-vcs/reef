//! Test fixtures and dev tooling for Reef: the synthetic repository
//! generator, fixture builders, test helpers, and the agent benchmarks
//! (onboarding, and token efficiency).
//!
//! This crate is not published and depends on no other Reef crate, so any
//! Reef crate can take it as a dev-dependency without creating a cycle.

pub mod agent;
pub mod git;
pub mod onboarding;
pub mod synth;
pub mod tokens;
