//! The backend conformance suite (R1.11) against every backend this
//! platform has: one test per [`BackendKind`], which probes the kind and
//! runs the suite or prints `skipped: <kind>: <reason>`, and a test that
//! runs the suite against a deliberately broken backend to show it notices.
//!
//! Each test builds its own fixture (about a second), so its temporary
//! directory is removed when the test ends; a fixture in a `static` would
//! never be dropped, and nextest runs every test in its own process anyway.
//!
//! Each test prints a Markdown timing row for the kind it ran (nextest
//! shows it on success too; see `.config/nextest.toml`).
//!
//! The kernel overlayfs test runs only with `--features privileged-tests`
//! as root (CI's privileged Linux job); reflink only with
//! `REEF_TEST_REFLINK_DIR` naming a directory on btrfs or XFS.

#![allow(
    clippy::print_stderr,
    reason = "skips and timings are reported on stderr"
)]

#[path = "common/backends.rs"]
mod backends;

use backends::{BaseFixture, BrokenCopy, SuiteOptions, run_conformance};
use reef_daemon::backend::{Availability, Backend, BackendKind, OverlayConfig, backend_for, probe};

/// Runs the suite against `backend` and fails with every failed case.
fn conform(backend: &dyn Backend, fx: &BaseFixture) {
    let report = run_conformance(
        backend,
        fx,
        SuiteOptions {
            skip_case_sensitivity: false,
            label: backend.kind().as_str(),
        },
    );
    eprintln!("{}", report.timing_table());
    assert!(report.failed.is_empty(), "{report}");
}

/// Probes `kind` for the fixture's workspace directory and base cache, and
/// runs the suite if it is available.
fn conform_if_available(kind: BackendKind, fx: &BaseFixture) {
    let overlay = OverlayConfig::default();
    let probes = probe(&fx.repo_dir().join("ws"), Some(fx.bases_dir()), &overlay);
    let availability = probes
        .into_iter()
        .find(|probe| probe.kind == kind)
        .map(|probe| probe.availability);
    match availability {
        Some(Availability::Available) => conform(backend_for(kind, &overlay).as_ref(), fx),
        Some(Availability::Unavailable(reason)) => eprintln!("skipped: {kind}: {reason}"),
        None => panic!("probe reported nothing for {kind}"),
    }
}

/// The copy backend works everywhere, so it never skips.
#[test]
fn conformance_copy() {
    conform(
        backend_for(BackendKind::Copy, &OverlayConfig::default()).as_ref(),
        &BaseFixture::small(),
    );
}

#[test]
#[cfg(target_os = "macos")]
fn conformance_apfs() {
    conform_if_available(BackendKind::ApfsClone, &BaseFixture::small());
}

#[test]
#[cfg(target_os = "linux")]
fn conformance_reflink() {
    let Some(dir) = std::env::var_os("REEF_TEST_REFLINK_DIR") else {
        eprintln!(
            "skipped: reflink: REEF_TEST_REFLINK_DIR is not set to a directory on btrfs or XFS"
        );
        return;
    };
    let spec = reef_testing::synth::Spec::preset(reef_testing::synth::Preset::Small);
    let fx = BaseFixture::in_dir(dir.as_ref(), &spec, backends::SEED, "small");
    // The variable promises a filesystem that can reflink, so an
    // unavailable probe is a failure here, not a skip.
    let overlay = OverlayConfig::default();
    let availability = probe(&fx.repo_dir().join("ws"), Some(fx.bases_dir()), &overlay)
        .into_iter()
        .find(|probe| probe.kind == BackendKind::Reflink)
        .map(|probe| probe.availability);
    assert_eq!(
        availability,
        Some(Availability::Available),
        "REEF_TEST_REFLINK_DIR is set"
    );
    conform(backend_for(BackendKind::Reflink, &overlay).as_ref(), &fx);
}

#[test]
#[cfg(target_os = "linux")]
fn conformance_fuse_overlayfs() {
    conform_if_available(BackendKind::FuseOverlayfs, &BaseFixture::small());
}

#[test]
#[cfg(target_os = "linux")]
fn conformance_overlayfs() {
    if !cfg!(feature = "privileged-tests") {
        eprintln!("skipped: overlayfs: needs --features privileged-tests, run as root");
        return;
    }
    conform_if_available(BackendKind::Overlayfs, &BaseFixture::small());
}

/// A copy that does not restore mtimes fails exactly the cases that
/// depend on them: the mtime check itself, the first snapshot (jj has to
/// rehash every file), and disk usage (every file differs from the base).
/// Every other case passes, so a failure points at its cause.
#[test]
fn broken_copy_fails_only_the_mtime_cases() {
    let report = run_conformance(
        &BrokenCopy,
        &BaseFixture::small(),
        SuiteOptions {
            skip_case_sensitivity: false,
            label: "broken-copy",
        },
    );
    assert_eq!(
        report.failed_cases(),
        [
            "mtime_and_size_preserved",
            "first_snapshot_is_noop",
            "disk_usage_tracks_changes"
        ],
        "{report}"
    );
}
