//! R3.42 test 4: real bundles of binding statements, recorded from the
//! staging and public-good instances (an ed25519 key certified by Fulcio,
//! logged in Rekor v1 as `dsse` 0.0.1), verify offline against their trusted
//! roots, and fail with one byte changed.
#![cfg(feature = "sigstore")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    reason = "tests: a panic is the failure report, and a refusal's text is printed"
)]

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use reef_evidence::session::{Actor, Custody, Presence};
use reef_evidence::sigstore::{Step, TrustedRoot, check_sigstore_bundle};

const PRODUCTION: &str = include_str!("fixtures/sigstore/production.bundle.json");
const STAGING: &str = include_str!("fixtures/sigstore/staging.bundle.json");
const STAGING_ROOT: &str = include_str!("fixtures/sigstore/staging-trusted-root.json");
const PROBE_SUBJECT: &str = "https://github.com/reef-vcs/reef/.github/workflows/sigstore-probe.yml@refs/heads/r3.42-staging-probe";

#[test]
fn recorded_bundles_verify_offline() {
    for (bundle, root) in [
        (PRODUCTION, TrustedRoot::embedded().unwrap()),
        (
            STAGING,
            TrustedRoot::from_json(STAGING_ROOT.as_bytes(), "staging").unwrap(),
        ),
    ] {
        let certified = check_sigstore_bundle(bundle.as_bytes(), &root).unwrap();
        assert_eq!(
            certified.oidc_issuer,
            "https://token.actions.githubusercontent.com"
        );
        assert_eq!(certified.oidc_subject, PROBE_SUBJECT);
        assert_eq!(certified.actor, Actor::Executor);
        assert_eq!(certified.agent.as_deref(), Some("r3.42-probe"));
        assert_eq!(certified.presence, Presence::None);
        assert_eq!(certified.custody, Custody::Client);
        assert_eq!(certified.not_after_ms - certified.not_before_ms, 600_000);
        assert!(certified.not_before_ms <= certified.integrated_time_ms);
        assert!(certified.integrated_time_ms <= certified.not_after_ms);
        assert!(certified.ca.host.starts_with("fulcio.sigs"));
        assert_eq!(
            certified
                .ci_claims
                .get("source_repository_uri")
                .map(String::as_str),
            Some("https://github.com/reef-vcs/reef")
        );
        assert!(certified.log_index > 0);
    }
}

#[test]
fn recorded_bundles_fail_against_the_other_root() {
    let staging = TrustedRoot::from_json(STAGING_ROOT.as_bytes(), "staging").unwrap();
    let error = check_sigstore_bundle(PRODUCTION.as_bytes(), &staging).unwrap_err();
    assert!(
        matches!(error.step, Step::Chain | Step::Tlog | Step::Sct),
        "{error}"
    );
}

#[test]
fn a_changed_byte_fails() {
    let root = TrustedRoot::embedded().unwrap();
    let value: serde_json::Value = serde_json::from_str(PRODUCTION).unwrap();
    // The payload: one byte of the statement changed, still JSON.
    let mut payload_changed = value.clone();
    let payload = payload_changed["dsseEnvelope"]["payload"].as_str().unwrap();
    let statement = String::from_utf8(B64.decode(payload).unwrap()).unwrap();
    let changed = statement.replace("r3.42-probe", "r3.42-probf");
    assert_ne!(changed, statement);
    payload_changed["dsseEnvelope"]["payload"] = B64.encode(changed).into();
    let error = check_sigstore_bundle(payload_changed.to_string().as_bytes(), &root).unwrap_err();
    assert!(
        matches!(error.step, Step::Signature | Step::Tlog),
        "{error}"
    );
    // The integrated time, a second earlier: the SET no longer verifies.
    let mut time_changed = value;
    let entry = &mut time_changed["verificationMaterial"]["tlogEntries"][0];
    let time: i64 = entry["integratedTime"].as_str().unwrap().parse().unwrap();
    entry["integratedTime"] = (time - 1).to_string().into();
    let error = check_sigstore_bundle(time_changed.to_string().as_bytes(), &root).unwrap_err();
    assert_eq!(error.step, Step::Tlog, "{error}");
    // Too large.
    let big = format!("{PRODUCTION}{}", " ".repeat(64 * 1024));
    assert_eq!(
        check_sigstore_bundle(big.as_bytes(), &root)
            .unwrap_err()
            .step,
        Step::Size
    );
}

/// The cost of one `check_sigstore_bundle` of a recorded public-good bundle,
/// for the PR and docs/benchmarks.md: `cargo test --release -p
/// reef-evidence --features sigstore --test sigstore_recorded -- --ignored
/// --nocapture`.
#[test]
#[ignore = "a measurement, run by hand in release"]
fn check_cost() {
    let root = TrustedRoot::embedded().unwrap();
    let runs = 200u32;
    let start = std::time::Instant::now();
    for _ in 0..runs {
        check_sigstore_bundle(PRODUCTION.as_bytes(), &root).unwrap();
    }
    eprintln!("check_sigstore_bundle: {:?} each", start.elapsed() / runs);
}
