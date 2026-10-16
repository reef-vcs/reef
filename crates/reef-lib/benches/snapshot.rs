//! Snapshot benchmarks on the synthetic repository (R0.06): how much a
//! snapshot, an operation log read, and opening the repository cost on a
//! tree of the `REEF_BENCH_PRESET` size. `docs/benchmarks.md` says what each
//! benchmark measures and how to run them.
//!
//! Every benchmark is `snapshot/<preset>/<name>`. The tree is mutated in
//! place, so it drifts from the generated one across iterations and runs,
//! but its file count stays the same.

#![allow(
    clippy::expect_used,
    reason = "a failed step aborts the benchmark run; there is no caller to report to"
)]

mod common;

use std::cell::RefCell;
use std::time::Duration;

use criterion::measurement::WallTime;
use criterion::{
    BatchSize, BenchmarkGroup, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main,
};
use reef_lib::jj::Repo;
use reef_lib::jj::snapshot::SnapshotOutcome;
use reef_testing::synth::{MutateOps, Preset};

use common::Fixture;

/// Operations `op_log_100` reads.
const OP_LOG_LIMIT: usize = 100;

#[allow(
    clippy::print_stderr,
    reason = "the fixture's identity is reported to whoever runs the benches"
)]
fn snapshot_benches(c: &mut Criterion) {
    let fixture = common::fixture();
    let rt = common::runtime();
    eprintln!(
        "bench fixture: {} seed {} at {}, {} tracked files",
        fixture.preset_name(),
        fixture.seed,
        fixture.root.display(),
        fixture.files
    );
    let mut group = c.benchmark_group("snapshot");

    for modify in [1, 50, 500] {
        let ops = MutateOps {
            modify,
            ..MutateOps::default()
        };
        bench_after_mutation(&mut group, fixture, &rt, &format!("modify_{modify}"), ops);
    }
    let ops = MutateOps {
        add: 50,
        ..MutateOps::default()
    };
    bench_after_mutation(&mut group, fixture, &rt, "add_50", ops);
    bench_op_log(&mut group, fixture, &rt);
    bench_open(&mut group, fixture, &rt);
    // Last, because criterion has no way to clear a group's throughput again.
    bench_noop(&mut group, fixture, &rt);

    group.finish();
}

/// Applies `ops` in each iteration's setup and measures the snapshot that
/// records them. Files added by one iteration are offset by deleting as many
/// in the next iteration's setup, snapshotted before the measured mutation,
/// so the tree keeps its size.
fn bench_after_mutation(
    group: &mut BenchmarkGroup<'_, WallTime>,
    fixture: &Fixture,
    rt: &tokio::runtime::Runtime,
    name: &str,
    ops: MutateOps,
) {
    let repo = RefCell::new(fixture.open(rt));
    let description = format!("bench: {name}");
    let id = BenchmarkId::new(fixture.preset_name(), name);
    group.bench_with_input(id, &ops, |b, &ops| {
        b.iter_batched(
            || {
                if ops.add > 0 {
                    fixture.mutate(MutateOps {
                        delete: ops.add,
                        ..MutateOps::default()
                    });
                    snapshot(rt, &mut repo.borrow_mut(), "bench: restore the file count");
                }
                fixture.mutate(ops);
            },
            |()| {
                let outcome = snapshot(rt, &mut repo.borrow_mut(), &description);
                assert!(outcome.changed, "{name}: the snapshot found nothing");
                outcome
            },
            BatchSize::PerIteration,
        );
    });
}

/// `Repo::op_log(100)`, after snapshotting enough changes for the log to
/// hold that many operations.
fn bench_op_log(
    group: &mut BenchmarkGroup<'_, WallTime>,
    fixture: &Fixture,
    rt: &tokio::runtime::Runtime,
) {
    let mut repo = fixture.open(rt);
    let one_file = MutateOps {
        modify: 1,
        ..MutateOps::default()
    };
    while op_log(rt, &repo).len() < OP_LOG_LIMIT {
        fixture.mutate(one_file);
        snapshot(rt, &mut repo, "bench: grow the operation log");
    }
    let id = BenchmarkId::new(fixture.preset_name(), "op_log_100");
    group.bench_function(id, |b| b.iter(|| op_log(rt, &repo)));
}

/// `Repo::open`: loading the workspace, op heads, view, and index.
fn bench_open(
    group: &mut BenchmarkGroup<'_, WallTime>,
    fixture: &Fixture,
    rt: &tokio::runtime::Runtime,
) {
    let id = BenchmarkId::new(fixture.preset_name(), "open");
    group.bench_function(id, |b| b.iter(|| fixture.open(rt)));
}

/// A snapshot of an unchanged tree: the scan cost of finding that nothing
/// changed. Reported in files per second as well as time.
fn bench_noop(
    group: &mut BenchmarkGroup<'_, WallTime>,
    fixture: &Fixture,
    rt: &tokio::runtime::Runtime,
) {
    let mut repo = fixture.open(rt);
    // Earlier benchmarks leave the tree clean, but do not depend on it.
    snapshot(rt, &mut repo, "bench: clean the tree");
    group.throughput(Throughput::Elements(fixture.files));
    let id = BenchmarkId::new(fixture.preset_name(), "noop");
    group.bench_function(id, |b| {
        b.iter(|| {
            let outcome = snapshot(rt, &mut repo, "bench: noop");
            assert!(!outcome.changed, "noop: the tree changed");
            outcome
        });
    });
}

fn snapshot(rt: &tokio::runtime::Runtime, repo: &mut Repo, description: &str) -> SnapshotOutcome {
    rt.block_on(repo.snapshot(description))
        .expect("snapshot the fixture")
}

fn op_log(rt: &tokio::runtime::Runtime, repo: &Repo) -> Vec<reef_lib::jj::oplog::OpEntry> {
    rt.block_on(repo.op_log(OP_LOG_LIMIT))
        .expect("read the operation log")
}

/// Sample size 10, and a measurement time that grows with the preset. Both
/// can be overridden on the command line (`-- --measurement-time 5`).
fn config() -> Criterion {
    let measurement = match common::preset() {
        Preset::Small => 10,
        Preset::Medium => 30,
        Preset::Large | Preset::Xl => 60,
    };
    Criterion::default()
        .sample_size(10)
        .measurement_time(Duration::from_secs(measurement))
}

criterion_group! {
    name = benches;
    config = config();
    targets = snapshot_benches
}
criterion_main!(benches);
