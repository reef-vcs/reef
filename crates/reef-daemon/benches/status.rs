//! What a workspace's `GetStatus` costs through the daemon, from the
//! incremental status index and from a walk (R2.23), on the synthetic
//! preset `REEF_BENCH_PRESET` names (`small`, `medium`, the default, or
//! `large`), one notify workspace:
//!
//! - `status/<preset>/walk`: the tree unchanged, the journal marked
//!   `events-lost` before each call, so the snapshot walks every file;
//! - `walk_one_edit`: the same after one file appended (untimed); both
//!   with the workspace's automatic snapshots paused, so none takes the
//!   journal first;
//! - `index_clean`: the tree unchanged and the watcher settled: no snapshot;
//! - `index_one_edit`: one file appended just before the call, so its event
//!   may still be in flight: a snapshot of that path;
//! - `index_100_edits`: 100 files appended before the call;
//! - `skip_snapshot`: `skip_snapshot`, the floor.
//!
//! An `index_*` call that answers other than `index` panics: that is Phase
//! 2's "without a tree walk", checked on every run. Results are in
//! `docs/benchmarks.md`, "Status index (R2.23)".
//!
//! ```sh
//! REEF_BENCH_PRESET=large cargo bench -p reef-daemon --features testing --bench status
//! ```

#![allow(missing_docs, reason = "a criterion harness")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a benchmark: a panic is the failure report"
)]

use std::path::Path;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use reef_api::v1::{
    CreateWorkspaceRequest, GetStatusRequest, StatusSource, StatusWalkReason, WorkspaceStatus,
};
use reef_daemon::config::WatcherBackend;
use reef_daemon::testing::TestDaemon;
use reef_testing::synth::Preset;

fn preset() -> (Preset, &'static str) {
    match std::env::var("REEF_BENCH_PRESET").as_deref() {
        Ok("small") => (Preset::Small, "small"),
        Ok("medium") | Err(std::env::VarError::NotPresent) => (Preset::Medium, "medium"),
        Ok("large") => (Preset::Large, "large"),
        other => panic!("REEF_BENCH_PRESET must be small, medium, or large, not {other:?}"),
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "the fixture and six benches over one workspace"
)]
fn bench(c: &mut Criterion) {
    let (preset, name) = preset();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (daemon, repo) = rt.block_on(TestDaemon::with_synthetic_repo_and(preset, |config| {
        let watcher = &mut config.file.watcher;
        watcher.backend = WatcherBackend::Notify;
        // No timer scan stands in for the call under test.
        watcher.fallback_interval = Duration::from_hours(1);
        watcher.watched_fallback_interval = Duration::from_hours(1);
        watcher.notify_attach_background = false;
    }));
    let ws = rt
        .block_on(daemon.client.create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "bench@reef".to_owned(),
            ..CreateWorkspaceRequest::default()
        }))
        .unwrap()
        .workspace
        .unwrap();
    let id = ws.id.parse().unwrap();
    let scheduler = daemon.handle.state().manager.scheduler();
    let root = Path::new(&ws.path).to_owned();
    let status = |skip: bool| -> WorkspaceStatus {
        rt.block_on(daemon.client.get_status(GetStatusRequest {
            workspace: ws.id.clone(),
            skip_snapshot: skip,
            ..GetStatusRequest::default()
        }))
        .unwrap()
        .workspace_status
        .unwrap()
    };
    let index = |status: &WorkspaceStatus, row: &str| {
        assert_eq!(
            (status.source(), status.walk_reason()),
            (StatusSource::Index, StatusWalkReason::Unspecified),
            "{row} walked: {status:?}"
        );
    };
    let mut appended = 0_usize;
    let mut append = |count: usize| {
        for _ in 0..count {
            std::fs::write(root.join(format!("bench-{appended}.txt")), "x\n").unwrap();
            appended += 1;
        }
    };
    // Settle: the first statuses after the create, and the watcher's.
    let settle = || {
        std::thread::sleep(Duration::from_millis(1_500));
        let begun = Instant::now();
        while status(false).snapshot.is_some() {
            assert!(begun.elapsed() < Duration::from_secs(60), "never settled");
        }
    };
    settle();

    let mut group = c.benchmark_group(format!("status/{name}"));
    group.sample_size(20);
    // No watcher snapshot of an injected event or an edit: one that took
    // the journal first would leave the timed read the index's.
    let paused = scheduler.pause(id);
    group.bench_function("walk", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                scheduler.inject_event(id, Vec::new(), true);
                let begun = Instant::now();
                let answer = status(false);
                total += begun.elapsed();
                assert_eq!(answer.source(), StatusSource::Walk, "{answer:?}");
            }
            total
        });
    });
    group.bench_function("walk_one_edit", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                append(1);
                scheduler.inject_event(id, Vec::new(), true);
                let begun = Instant::now();
                let answer = status(false);
                total += begun.elapsed();
                assert_eq!(answer.source(), StatusSource::Walk, "{answer:?}");
            }
            total
        });
    });
    drop(paused);
    settle();
    group.bench_function("index_clean", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let begun = Instant::now();
                let answer = status(false);
                total += begun.elapsed();
                index(&answer, "index_clean");
                assert!(answer.snapshot.is_none(), "index_clean snapshotted");
            }
            total
        });
    });
    group.bench_function("index_one_edit", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                append(1);
                let begun = Instant::now();
                let answer = status(false);
                total += begun.elapsed();
                index(&answer, "index_one_edit");
            }
            total
        });
    });
    group.bench_function("index_100_edits", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                append(100);
                let begun = Instant::now();
                let answer = status(false);
                total += begun.elapsed();
                index(&answer, "index_100_edits");
            }
            total
        });
    });
    group.bench_function("skip_snapshot", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let begun = Instant::now();
                let answer = status(true);
                total += begun.elapsed();
                assert_eq!(answer.source(), StatusSource::LastSnapshot);
            }
            total
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
