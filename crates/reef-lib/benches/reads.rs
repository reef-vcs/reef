//! Read benchmarks on the synthetic repository (R2.02): what the daemon's
//! read RPCs cost in `reef_lib::jj`, on a tree of the `REEF_BENCH_PRESET`
//! size. `docs/benchmarks.md` ("Reads") says what each measures.
//!
//! Every benchmark is `reads/<preset>/<name>`. The fixture is shared with
//! the snapshot benches; the reads mutate it only to set up their state.

#![allow(
    clippy::expect_used,
    reason = "a failed step aborts the benchmark run; there is no caller to report to"
)]

mod common;

use std::ops::ControlFlow;
use std::time::Instant;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use jj_lib::default_index::DefaultIndexStore;
use reef_lib::jj::diff::{self, DiffBase, DiffFormat, DiffOptions};
use reef_lib::jj::log::{self, Describer};
use reef_lib::jj::revset::{self, RevsetScope};
use reef_lib::jj::{CommitIdHex, Repo, effects, oplog};
use reef_testing::synth::MutateOps;

/// The page sizes the daemon uses by default.
const PAGE: usize = 100;

fn options(format: DiffFormat) -> DiffOptions {
    DiffOptions {
        format,
        context: 3,
        max_lines: 1000,
        max_hunk_bytes: 1 << 20,
        max_file_bytes: 16 << 20,
        paths: Vec::new(),
        after: None,
        limit: PAGE,
        pairs: false,
    }
}

fn snapshot(rt: &tokio::runtime::Runtime, repo: &mut Repo, description: &str) {
    rt.block_on(repo.snapshot(description))
        .expect("snapshot the fixture");
}

#[allow(
    clippy::too_many_lines,
    clippy::print_stderr,
    reason = "one fixture set up in steps, each followed by the reads it serves; its identity is reported to whoever runs the benches"
)]
fn read_benches(c: &mut Criterion) {
    let fixture = common::fixture();
    let rt = common::runtime();
    eprintln!(
        "bench fixture: {} seed {} at {}, {} tracked files",
        fixture.preset_name(),
        fixture.seed,
        fixture.root.display(),
        fixture.files
    );
    let mut repo = fixture.open(&rt);
    let mut group = c.benchmark_group("reads");
    let name = fixture.preset_name();
    let root = fixture.root.clone();
    let scope = RevsetScope {
        workspace: Some("default"),
        user_email: "bench@reef.invalid",
        root: &root,
    };

    // One file changed in the working copy, as a status after one edit. The
    // fixture's working copy sits on the root commit, so the edit is
    // diffed against the snapshot before it: the same one-file diff.
    let before = rt
        .block_on(revset::resolve_revision(&repo.readonly(), "@", &scope))
        .expect("resolve @")
        .commit_id;
    fixture.mutate(MutateOps {
        modify: 1,
        ..MutateOps::default()
    });
    snapshot(&rt, &mut repo, "bench: one edit");
    let head = repo.readonly();
    let wc = rt
        .block_on(revset::resolve_revision(&head, "@", &scope))
        .expect("resolve @")
        .commit_id;
    group.bench_function(BenchmarkId::new(name, "status_diff"), |b| {
        b.iter(|| {
            rt.block_on(async {
                let describer = Describer::new(&head, &scope).expect("describer");
                describer.describe(&wc).await.expect("describe @");
                let heads = revset::parse(&head, "heads(mutable())", &scope).expect("parse");
                log::commits(&head, &heads, PAGE, &|| false)
                    .await
                    .expect("heads");
                let page = diff::diff_page(
                    &head,
                    &DiffBase::Commit(before.clone()),
                    &wc,
                    &options(DiffFormat::Summary),
                    &|| false,
                )
                .await
                .expect("diff");
                assert_eq!(page.total, 1, "one file changed");
            });
        });
    });
    group.bench_function(BenchmarkId::new(name, "log_default"), |b| {
        b.iter(|| {
            rt.block_on(async {
                let parsed = revset::parse(&head, revset::DEFAULT_LOG, &scope).expect("parse");
                let page = log::log_page(&head, &parsed, 0, PAGE, &|| false)
                    .await
                    .expect("log");
                let describer = Describer::new(&head, &scope).expect("describer");
                for node in &page.nodes {
                    describer.describe(&node.commit_id).await.expect("describe");
                }
            });
        });
    });
    group.bench_function(BenchmarkId::new(name, "change_describe_100"), |b| {
        b.iter(|| {
            rt.block_on(async {
                let describer = Describer::new(&head, &scope).expect("describer");
                for _ in 0..100 {
                    describer.describe(&wc).await.expect("describe");
                }
            });
        });
    });
    let root_commit: CommitIdHex = rt
        .block_on(revset::resolve_revision(&head, "root()", &scope))
        .expect("resolve root()")
        .commit_id;
    group.bench_function(BenchmarkId::new(name, "diff_summary_full"), |b| {
        b.iter(|| {
            rt.block_on(async {
                let page = diff::diff_page(
                    &head,
                    &DiffBase::Commit(root_commit.clone()),
                    &wc,
                    &options(DiffFormat::Summary),
                    &|| false,
                )
                .await
                .expect("diff");
                assert!(page.total > PAGE);
            });
        });
    });
    // The effects of the snapshot just taken, uncached.
    group.bench_function(BenchmarkId::new(name, "effects_snapshot"), |b| {
        b.iter(|| {
            rt.block_on(effects::operation_effects(
                &head,
                head.operation(),
                effects::MAX_EFFECTS,
            ))
            .expect("effects")
        });
    });

    // Fifty files changed, with hunks, against the snapshot before them.
    let before = wc.clone();
    fixture.mutate(MutateOps {
        modify: 50,
        ..MutateOps::default()
    });
    snapshot(&rt, &mut repo, "bench: fifty edits");
    let head = repo.readonly();
    let wc = rt
        .block_on(revset::resolve_revision(&head, "@", &scope))
        .expect("resolve @")
        .commit_id;
    group.bench_function(BenchmarkId::new(name, "diff_hunks_50"), |b| {
        b.iter(|| {
            rt.block_on(diff::diff_page(
                &head,
                &DiffBase::Commit(before.clone()),
                &wc,
                &options(DiffFormat::Hunks),
                &|| false,
            ))
            .expect("diff")
        });
    });

    // A page of the operation log, classified.
    let one = MutateOps {
        modify: 1,
        ..MutateOps::default()
    };
    while rt.block_on(repo.op_log(PAGE)).expect("op log").len() < PAGE {
        fixture.mutate(one);
        snapshot(&rt, &mut repo, "bench: grow the operation log");
    }
    let head = repo.readonly();
    let heads = rt.block_on(oplog::op_heads(&head)).expect("op heads");
    group.bench_function(BenchmarkId::new(name, "op_page_100"), |b| {
        b.iter(|| {
            let mut listed = 0;
            rt.block_on(oplog::walk_from(&head, &heads, 0, async |operation| {
                oplog::classify(&oplog::entry(operation));
                listed += 1;
                if listed == PAGE {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            }))
            .expect("walk");
            listed
        });
    });

    // R2.09: what one subscription reads for the operations after its
    // position, classified: one new operation (the per-subscriber cost of
    // every operation until R2.22's broker, which reads it once per
    // repository) and a thousand (a reader catching up).
    let mut ids = vec![oplog::op_id_of(&repo.readonly())];
    for n in 0..1000 {
        let tx = repo.readonly().start_transaction();
        let next = rt
            .block_on(tx.commit(format!("bench: operation {n}")))
            .expect("commit an operation");
        repo.set_head(next);
        ids.push(oplog::op_id_of(&repo.readonly()));
    }
    let head = repo.readonly();
    let heads = rt.block_on(oplog::op_heads(&head)).expect("op heads");
    for (bench, from) in [("op_range_1", ids.len() - 2), ("op_range_1000", 0)] {
        let frontier = oplog::Frontier::new([ids[from].clone()]);
        group.bench_function(BenchmarkId::new(name, bench), |b| {
            b.iter(|| {
                let range = rt
                    .block_on(oplog::walk_after(&head, &heads, &frontier, 10_000))
                    .expect("walk");
                for operation in &range.operations {
                    oplog::classify(&oplog::entry(operation));
                }
                range.operations.len()
            });
        });
    }

    // R2.12: after GC's reinit, which keeps only the head's index, a read
    // at an operation 500 back borrows the head's index; jj's own load of
    // it builds and saves a full index for that operation, timed once and
    // reported. And a lookup by time back to 1,000 operations ago.
    let store = repo.readonly().index_store().clone();
    if let Some(store) = store.downcast_ref::<DefaultIndexStore>() {
        store.reinit().expect("reinit the index");
    }
    rt.block_on(repo.reload_at_head())
        .expect("rebuild the head's index");
    let head = repo.readonly();
    let back = rt
        .block_on(oplog::resolve_operation(&head, ids[500].as_str()))
        .expect("an operation 500 back");
    group.bench_function(BenchmarkId::new(name, "load_at_shared"), |b| {
        b.iter(|| {
            rt.block_on(oplog::load_at_shared(&head, &back))
                .expect("load")
        });
    });
    let started = Instant::now();
    rt.block_on(oplog::load_at(&head, &back)).expect("load");
    eprintln!("reads/{name}/load_at_rebuilt: {:?}", started.elapsed());
    let oldest = rt
        .block_on(oplog::resolve_operation(&head, ids[0].as_str()))
        .expect("the operation 1,000 back");
    let at = oplog::entry(&oldest).end;
    group.bench_function(BenchmarkId::new(name, "locate_1000"), |b| {
        b.iter(|| {
            rt.block_on(oplog::locate(&head, &[], at, 100_000))
                .expect("locate")
        });
    });

    group.finish();
}

criterion_group!(benches, read_benches);
criterion_main!(benches);
