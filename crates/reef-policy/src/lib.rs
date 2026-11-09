//! Reef's repository policy (R3.07, ADR 0061): `reef-policy.toml` at the
//! root of a tree, and its evaluation over one change.
//!
//! - [`format`](mod@format): the file, format version 1: strict TOML tables (owners,
//!   groups, attestor sets, required evidence, limits, forbidden paths,
//!   sign-off areas, conflict tiers) whose conditions are CEL, plus free CEL
//!   rules, loaded into a [`Policy`] with a [`Diagnostic`] per problem.
//! - [`input`]: what a policy sees about one change: its paths and files,
//!   the evidence on its tree with each signer's identity, and its
//!   provenance.
//! - [`eval`]: [`Policy::evaluate`], into an [`Evaluation`]: a
//!   [`Decision`] and a result per rule, with the evidence still missing and
//!   the sign-offs still needed.
//! - [`conflict`]: [`Policy::conflict_tier`], which R3.28 applies.
//! - [`schema`]: the file, the input and the evaluation as JSON Schema.
//!
//! CEL runs on cel-rust's `cel`, pinned exactly, behind Reef's own static
//! check ([`check`]) and a cost bound computed from the real input before
//! anything runs ([`cost`]), since cel-rust has neither. Everything fails
//! closed: a rule that errors blocks, and so does a policy that does not
//! load.
//!
//! Evaluation decides; it never refuses anything. The landing service
//! enforces a decision (R3.13). The crate depends on no Reef crate but
//! `reef-error`, and on no jj-lib, gRPC, SQLite or tokio, so the daemon,
//! the CLI and the phase-four store share it.

use reef_error::{Classify, ErrorKind};

pub mod check;
pub mod conflict;
pub mod cost;
pub mod env;
pub mod eval;
pub mod format;
pub mod glob;
pub mod input;
pub mod schema;

pub use conflict::ConflictDecision;
pub use eval::{
    Decision, Evaluation, InputSummary, Need, Rejected, RuleKind, RuleResult, SignoffNeed, Status,
};
pub use format::{Diagnostic, Policy, Tier, VERSION};
pub use input::{Case, ConflictInput, Input};

/// Where a tree's policy file is: at its root.
pub const POLICY_PATH: &str = "reef-policy.toml";

/// The built-in rules, in the order every evaluation reports them.
pub const BUILTIN_RULES: [&str; 4] = [
    "builtin:input",
    "builtin:conflicts",
    "builtin:policy-file",
    "builtin:owners",
];

/// The most touched paths (and files) a policy judges; a larger change is
/// `builtin:input`'s error, which blocks it.
pub const MAX_PATHS: usize = 100_000;

/// The most evidence objects that count under a policy (that satisfy an
/// attestor set or a sign-off rule) one evaluation reads; more is
/// `builtin:input`'s error.
pub const EVIDENCE_TRUSTED_MAX: usize = 10_000;

/// The most other evidence objects an evaluation keeps, newest first, so
/// junk cannot push trusted evidence out of view (S-94).
pub const EVIDENCE_UNTRUSTED_MAX: usize = 1_000;

/// The most paths a rule result lists; `paths_total` counts them all.
pub const MAX_REPORTED_PATHS: usize = 20;

/// The most paths [`input::Landing::overlap`] lists; `overlap_total`
/// counts them all.
pub const MAX_OVERLAP_PATHS: usize = 1_000;

/// The most rejected objects a missing need lists ([`eval::Need::rejected`]);
/// `rejected_total` counts them all.
pub const MAX_REJECTED: usize = 20;

/// The SHA-256 of `bytes`, as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    digest
        .as_ref()
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Why a policy could not be read: what the CLI reports for a file that
/// does not load.
#[derive(Debug, thiserror::Error)]
#[error("{path}:{first}")]
pub struct LoadError {
    /// The file, as the caller named it.
    pub path: String,
    /// The first problem.
    pub first: Diagnostic,
    /// Every problem.
    pub diagnostics: Vec<Diagnostic>,
}

impl Classify for LoadError {
    fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidArgument
    }
}

/// The starter policy `reef policy init` writes, for `owner`, the default
/// person identity's name: it requires passing `test` evidence from this
/// machine's executor on every change.
pub fn starter(owner: &str) -> String {
    STARTER.replace("{owner}", owner)
}

const STARTER: &str = r#"# Reef repository policy (docs/policy.md). The policy in force is the one on
# the branch a change lands onto, never the change's own, and a change to this
# file or to reef-checks.toml needs the owners' sign-off.
version = 1

# Who signs off a change to reef-policy.toml and reef-checks.toml: sign with
# `reef evidence add --kind signoff --result pass --actor human`.
[owners]
identities = ["{owner}"]

# Whose evidence counts: this machine's executor, signing for {owner}, and never
# the change's own session or agent.
[attestors.local]
identities = ["{owner}"]
actors = ["executor"]

# A team trusts its CI identity instead (`reef identity create ci-main --kind ci`):
# [attestors.ci]
# identities = ["ci-main"]
# identity_kinds = ["ci"]
# when = 'e.artifacts.exists(a, a.state == "stored")'

# Every change needs passing `test` evidence (`reef job submit --kind test`).
[[require]]
name = "tests"
kinds = ["test"]
attestors = ["local"]

# [limits]
# max_files = 400
# max_lines = 4000
# max_file_size = "5MiB"
# except = ["Cargo.lock"]

[[forbid]]
name = "keys"
paths = ["**/*.pem", "**/id_rsa", "**/id_ed25519"]
message = "private keys never enter the tree"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_starter_loads() {
        let policy = Policy::load(starter("alice").as_bytes()).unwrap();
        assert_eq!(
            policy.rule_names(),
            [
                "builtin:input",
                "builtin:conflicts",
                "builtin:policy-file",
                "builtin:owners",
                "tests",
                "keys"
            ]
        );
    }

    #[test]
    fn sha256_is_hex() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
