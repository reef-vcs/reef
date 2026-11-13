//! The fleet benchmark's harness, end to end on its synthetic repository
//! (#264, test 4): five tickets at once against a scratch daemon, offline,
//! in well under a minute. Here rather than in reef-testing because only
//! this package's tests can name the `reef` binary.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic is the failure report"
)]

mod common;

use std::path::Path;
use std::time::Duration;

use reef_testing::fleet::report::{self, Verdict};
use reef_testing::fleet::run::{self, RunConfig};
use reef_testing::fleet::{LimitMode, Profile, SeedMode, Toggle, Variant, repos};

fn config(root: &Path, seed: SeedMode, tickets: u32) -> RunConfig {
    // `reef` and `reef-daemon` side by side, as an install has them.
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_reef"), bin.join("reef")).unwrap();
    std::os::unix::fs::symlink(common::daemon_bin(), bin.join("reef-daemon")).unwrap();
    RunConfig {
        reef_bin_dir: bin,
        repos: vec![repos::synthetic()],
        patches_root: root.join("patches"),
        reef_checkout: root.to_path_buf(),
        work_root: root.join("w"),
        cache: root.join("repos"),
        out: root.join("out"),
        profile: Profile::Dev,
        variant: Variant {
            limit: Toggle::On,
            seed,
            stores: Toggle::Off,
        },
        tickets,
        solo_tickets: tickets,
        solo_runs: 1,
        limits: LimitMode::On,
        idle: Duration::from_secs(1),
        stagger: Duration::from_millis(300),
        keep_state: false,
    }
}

fn tools_present() -> bool {
    repos::which("cargo").is_some() && repos::which("git").is_some()
}

#[test]
fn five_synthetic_tickets_at_once() {
    if !tools_present() {
        return;
    }
    // A short root: the daemon's socket lives under it.
    let root = tempfile::Builder::new()
        .prefix("fl")
        .tempdir_in("/tmp")
        .unwrap();
    // #271 test 22: off, then on, in one run; both halves reported.
    let mut config = config(root.path(), SeedMode::Off, 5);
    config.limits = LimitMode::Both;
    let report = run::run(&config).unwrap();
    let repo = &report.repos[0];
    assert_eq!(repo.limit, Some(Toggle::On));
    let off = repo.off.as_ref().expect("the off run");
    assert_eq!(off.limit, Some(Toggle::Off));
    assert_eq!(off.error, None, "{}", report::render(&report));
    assert_eq!(off.five.len(), 10);
    assert!(
        !repo.build_samples.is_empty(),
        "the build limit was sampled: off {}, phases {:?}, error {:?}",
        off.build_samples.len(),
        repo.phases,
        repo.error
    );
    let halves = report::limit_halves(repo).expect("both halves");
    assert!(
        halves.makespan.is_some() && halves.solo.is_some(),
        "{halves:?}"
    );
    assert_eq!(repo.error, None, "{}", report::render(&report));
    assert_eq!(repo.solo.len(), 5);
    assert_eq!(repo.five.len(), 10, "{}", report::render(&report));
    for t in repo.solo.iter().chain(&repo.five) {
        assert_eq!(t.exit_code, Some(0), "{t:?}");
        assert!(
            t.outcomes.keys().any(|k| k.contains("value_is_patched")),
            "{t:?}"
        );
    }
    // Round 2 ran in round 1's workspace, with the review round's test.
    for t in repo.five.iter().filter(|t| t.round == 2) {
        let first = repo
            .five
            .iter()
            .find(|f| f.round == 1 && f.ticket == t.ticket)
            .unwrap();
        assert_eq!(t.workspace_id, first.workspace_id);
        assert!(t.outcomes.keys().any(|k| k.contains("doubled_is_patched")));
        assert!(t.disk.as_ref().unwrap().build.files > 0);
    }
    let s = &repo.summary;
    assert_eq!(s.compared, 5);
    assert!(s.mismatches.is_empty(), "{:?}", s.mismatches);
    assert!(s.five_peak_bytes.is_some() && s.load_p95.is_some());
    assert!(!repo.list_ms.is_empty(), "the canary listed workspaces");
    // Every gate shows a figure; a dev run is never judged.
    let gates = report::gates(repo, report.profile, &report.host.os);
    assert_eq!(gates.len(), 9);
    assert!(gates.iter().all(|g| g.verdict == Verdict::Dev));
    assert!(!report::any_failed(&report));
    // The written report reads back, and every workspace is gone.
    let written: report::Report =
        serde_json::from_str(&std::fs::read_to_string(config.out.join("report.json")).unwrap())
            .unwrap();
    assert_eq!(written.repos[0].five.len(), 10);
    assert!(config.out.join("summary.md").is_file());
}

/// The spike's E1: a sibling's build output cloned into a new workspace.
/// Cargo judges freshness by mtime, so it reuses the sibling's test
/// binaries, and the probe crate (which checks that its binary was built in
/// its own directory) fails: the harness reports that as a G9 mismatch.
#[test]
fn a_sibling_seed_runs_and_is_compared() {
    if !tools_present() {
        return;
    }
    let root = tempfile::Builder::new()
        .prefix("fl")
        .tempdir_in("/tmp")
        .unwrap();
    let mut config = config(root.path(), SeedMode::Sibling, 2);
    config.idle = Duration::ZERO;
    let report = run::run(&config).unwrap();
    let repo = &report.repos[0];
    assert_eq!(repo.error, None, "{}", report::render(&report));
    let seed = repo.seed.as_ref().expect("the seed's source ran");
    assert_eq!(seed.exit_code, Some(0));
    assert!(seed.disk.as_ref().unwrap().build.files > 0);
    assert_eq!(repo.summary.compared, 2);
    assert_eq!(
        repo.summary.mismatches.len(),
        2,
        "{}",
        report::render(&report)
    );
    for diff in repo.summary.mismatches.values() {
        assert!(
            diff.iter()
                .any(|d| d.contains("manifest_dir_is_this_workspace: Pass -> Fail")),
            "{diff:?}"
        );
    }
}
