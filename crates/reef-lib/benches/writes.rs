//! Write benchmarks on the synthetic repository (R2.03): what describe,
//! abandon and undo cost in `reef_lib::jj`, transaction and publish, on a
//! tree of the `REEF_BENCH_PRESET` size. `docs/benchmarks.md` ("Writes")
//! says what each measures.
//!
//! Every benchmark is `writes/<preset>/<name>`. The fixture is shared with
//! the snapshot and read benches; each iteration's setup commits what it
//! writes on, outside the measurement.

#![allow(
    clippy::expect_used,
    reason = "a failed step aborts the benchmark run; there is no caller to report to"
)]

mod common;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use reef_lib::jj::revset::{self, RevsetScope};
use reef_lib::jj::{CommitIdHex, OpKind, OpMeta, Repo};
use reef_testing::synth::MutateOps;

fn meta(kind: OpKind) -> OpMeta {
    OpMeta::new(kind, format!("bench: {}", kind.as_str()))
        .workspace("0123456789abcdef")
        .agent("bench")
}

fn hex(commit: &Commit) -> CommitIdHex {
    commit.id().hex().parse().expect("commit id")
}

/// Commits `count` new commits of one change (one when `count` is 1) on
/// `parent`, each changing one file's contents, plus a child of the first
/// when `child`; returns them, the child last.
fn commits_on(
    rt: &tokio::runtime::Runtime,
    repo: &mut Repo,
    parent: &CommitIdHex,
    count: usize,
    child: bool,
    salt: u64,
) -> Vec<Commit> {
    rt.block_on(async {
        let head = repo.readonly();
        let mut tx = head.start_transaction();
        let store = tx.repo().store().clone();
        let parent = store
            .get_commit_async(&CommitId::try_from_hex(parent.as_str()).expect("hex"))
            .await
            .expect("parent");
        let mut made: Vec<Commit> = Vec::new();
        for n in 0..count {
            let mut builder = tx
                .repo_mut()
                .new_commit(vec![parent.id().clone()], parent.tree())
                .set_description(format!("bench {salt} {n}"));
            if let Some(first) = made.first() {
                builder = builder.set_change_id(first.change_id().clone());
            }
            made.push(builder.write().await.expect("write"));
        }
        if child {
            let first = made[0].clone();
            let commit = tx
                .repo_mut()
                .new_commit(vec![first.id().clone()], first.tree())
                .set_description(format!("bench child {salt}"))
                .write()
                .await
                .expect("write child");
            made.push(commit);
        }
        tx.repo_mut().rebase_descendants().await.expect("rebase");
        repo.set_head(tx.commit("bench: setup").await.expect("commit"));
        made
    })
}

#[allow(
    clippy::print_stderr,
    reason = "the fixture's identity is reported to whoever runs the benches"
)]
fn write_benches(c: &mut Criterion) {
    let fixture = common::fixture();
    let rt = common::runtime();
    eprintln!(
        "bench fixture: {} seed {} at {}, {} tracked files",
        fixture.preset_name(),
        fixture.seed,
        fixture.root.display(),
        fixture.files
    );
    fixture.mutate(MutateOps {
        modify: 1,
        ..MutateOps::default()
    });
    let mut repo = fixture.open(&rt);
    rt.block_on(repo.snapshot("bench: one edit"))
        .expect("snapshot");
    let root = fixture.root.clone();
    let scope = RevsetScope {
        workspace: Some("default"),
        user_email: "bench@reef.invalid",
        root: &root,
    };
    let wc = rt
        .block_on(revset::resolve_revision(&repo.readonly(), "@", &scope))
        .expect("resolve @")
        .commit_id;
    // Setup and measurement both write: shared through a cell.
    let repo = std::cell::RefCell::new(repo);
    let mut group = c.benchmark_group("writes");
    let name = fixture.preset_name();
    let mut salt = 0u64;

    // The working-copy commit with one descendant.
    commits_on(&rt, &mut repo.borrow_mut(), &wc, 1, false, salt);
    group.bench_function(BenchmarkId::new(name, "describe_wc"), |b| {
        b.iter(|| {
            salt += 1;
            let wc = repo.borrow().wc_change().expect("wc").commit_id;
            rt.block_on(repo.borrow_mut().describe_commit(
                &wc,
                &format!("described {salt}"),
                &meta(OpKind::Describe),
            ))
            .expect("describe")
            .expect("a change");
        });
    });
    group.bench_function(BenchmarkId::new(name, "abandon_leaf"), |b| {
        b.iter_batched(
            || {
                salt += 1;
                hex(&commits_on(&rt, &mut repo.borrow_mut(), &wc, 1, false, salt)[0])
            },
            |leaf| {
                rt.block_on(
                    repo.borrow_mut()
                        .abandon_commit(&leaf, &meta(OpKind::Abandon)),
                )
                .expect("abandon");
            },
            BatchSize::PerIteration,
        );
    });
    group.bench_function(BenchmarkId::new(name, "abandon_divergent"), |b| {
        b.iter_batched(
            || {
                salt += 1;
                // Two commits of one change; the first has a child.
                hex(&commits_on(&rt, &mut repo.borrow_mut(), &wc, 2, true, salt)[0])
            },
            |loser| {
                let abandoned = rt
                    .block_on(
                        repo.borrow_mut()
                            .abandon_commit(&loser, &meta(OpKind::Abandon)),
                    )
                    .expect("abandon");
                assert!(abandoned.successor.is_some(), "a resolution");
            },
            BatchSize::PerIteration,
        );
    });
    group.bench_function(BenchmarkId::new(name, "revert_snapshot"), |b| {
        b.iter_batched(
            || {
                fixture.mutate(MutateOps {
                    modify: 1,
                    ..MutateOps::default()
                });
                rt.block_on(repo.borrow_mut().snapshot("bench: one more edit"))
                    .expect("snapshot")
                    .op_id
            },
            |op| {
                rt.block_on(repo.borrow_mut().revert_operation(&op, &meta(OpKind::Undo)))
                    .expect("revert")
                    .expect("an operation");
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
    // Leave the files as the head says, for the next bench run's clean-up.
    rt.block_on(repo.borrow_mut().snapshot("bench: clean up after writes"))
        .expect("snapshot");
}

criterion_group!(benches, write_benches);
criterion_main!(benches);
