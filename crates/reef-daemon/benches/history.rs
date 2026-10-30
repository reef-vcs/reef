//! What the history index costs through the daemon (R2.16), on a small
//! synthetic repository and `REEF_BENCH_CHANGES` changes (default 1,000),
//! each an `EditFiles` change by one of 50 agents adding a Python file of
//! ten functions under one of 20 directories:
//!
//! - `follow`: from the last edit to its row, and to every change's
//!   touches computed, timed once, printed;
//! - `history/query_symbol`: one page by a function name that 5% of the
//!   changes define;
//! - `history/query_path_dir`: one page by one of the 20 directories;
//! - `history/query_agent_week`: one page by one agent within seven days;
//! - `history/group_agent_outcome`: counts by agent and outcome over every
//!   change.
//!
//! Results are in `docs/benchmarks.md`, "History (R2.16)".
//!
//! ```sh
//! cargo bench -p reef-daemon --features testing --bench history
//! ```

#![allow(missing_docs, reason = "a criterion harness")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    reason = "a benchmark: a panic is the failure report, and it prints its one-shot figures"
)]

use std::fmt::Write as _;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use criterion::{Criterion, criterion_group, criterion_main};
use reef_api::v1::{
    ChangeDimension, EditFilesRequest, FileEdit, QueryChangesRequest, TouchState,
    edit_files_request, file_edit,
};
use reef_daemon::testing::TestDaemon;
use reef_testing::synth::Preset;

const AGENTS: usize = 50;
const DIRS: usize = 20;

fn changes() -> usize {
    std::env::var("REEF_BENCH_CHANGES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1000)
}

/// Ten functions; `rare_<i / 20>` names one in 20 changes.
fn module(i: usize) -> String {
    let mut text = String::new();
    for f in 0..9 {
        let _ = write!(text, "def f{i}_{f}():\n    return {f}\n\n");
    }
    let _ = write!(text, "def rare_{}():\n    return 0\n", i / 20);
    text
}

#[expect(
    clippy::too_many_lines,
    reason = "the fixture, the follow timing, four benches"
)]
fn bench(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (daemon, repo) = rt.block_on(TestDaemon::with_synthetic_repo_and(
        Preset::Small,
        |config| {
            config.file.watcher.debounce = Duration::from_hours(1);
        },
    ));
    let state = daemon.handle.state();
    state.history.set_throttle(Duration::ZERO, Duration::ZERO);
    let n = changes();
    let clients: Vec<_> = (0..AGENTS)
        .map(|a| {
            daemon
                .client
                .clone()
                .with_agent(format!("bot-{a}"))
                .unwrap()
        })
        .collect();
    let query = |request: QueryChangesRequest| {
        let client = daemon.client.clone();
        let repo = repo.id.clone();
        async move {
            client
                .query_changes(QueryChangesRequest { repo, ..request })
                .await
                .unwrap()
        }
    };

    let began = Instant::now();
    let mut last = String::new();
    rt.block_on(async {
        for i in 0..n {
            let change = clients[i % AGENTS]
                .edit_files(EditFilesRequest {
                    repo: repo.id.clone(),
                    target: Some(edit_files_request::Target::NewOn("trunk()".into())),
                    edits: vec![FileEdit {
                        path: format!("hist{}/m{i}.py", i % DIRS),
                        action: Some(file_edit::Action::Content(module(i).into_bytes())),
                        executable: None,
                    }],
                    ..EditFilesRequest::default()
                })
                .await
                .unwrap()
                .change
                .unwrap();
            last = change.change_id;
        }
    });
    let written = began.elapsed();
    let edited = Instant::now();
    rt.block_on(async {
        loop {
            let found = query(QueryChangesRequest {
                changes: vec![last.clone()],
                include_empty: true,
                ..QueryChangesRequest::default()
            })
            .await;
            if !found.changes.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let to_row = edited.elapsed();
    rt.block_on(async {
        loop {
            let found = query(QueryChangesRequest::default()).await;
            let coverage = found.coverage.unwrap();
            let done = coverage.pending == 0
                && found
                    .changes
                    .iter()
                    .all(|c| c.touches_state() == TouchState::Current);
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    let touched = edited.elapsed();
    println!(
        "follow: {n} edits in {written:.1?}; last edit to its row {to_row:.1?}; to every change's touches {touched:.1?}"
    );

    let week_ago = SystemTime::now() - Duration::from_hours(7 * 24);
    let since = week_ago.duration_since(UNIX_EPOCH).unwrap();
    let mut group = c.benchmark_group("history");
    group.sample_size(20);
    group.bench_function("query_symbol", |b| {
        b.iter(|| {
            rt.block_on(query(QueryChangesRequest {
                symbols: vec!["rare_3".into()],
                ..QueryChangesRequest::default()
            }))
        });
    });
    group.bench_function("query_path_dir", |b| {
        b.iter(|| {
            rt.block_on(query(QueryChangesRequest {
                paths: vec!["hist7".into()],
                ..QueryChangesRequest::default()
            }))
        });
    });
    group.bench_function("query_agent_week", |b| {
        b.iter(|| {
            rt.block_on(query(QueryChangesRequest {
                agents: vec!["bot-7".into()],
                since: Some(prost_types::Timestamp {
                    seconds: i64::try_from(since.as_secs()).unwrap(),
                    nanos: 0,
                }),
                ..QueryChangesRequest::default()
            }))
        });
    });
    group.bench_function("group_agent_outcome", |b| {
        b.iter(|| {
            rt.block_on(query(QueryChangesRequest {
                group_by: vec![
                    ChangeDimension::Agent.into(),
                    ChangeDimension::Outcome.into(),
                ],
                ..QueryChangesRequest::default()
            }))
        });
    });
    group.finish();
    rt.block_on(daemon.stop());
}

criterion_group!(benches, bench);
criterion_main!(benches);
