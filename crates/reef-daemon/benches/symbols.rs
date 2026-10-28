//! What the symbol index costs through the daemon (R2.14), on a synthetic
//! repository of `REEF_BENCH_PRESET` (`small`, `medium`, or `large`):
//!
//! - `full_index`: the first index of the tree, timed once (a second would
//!   find every entry), with the cache's size after it, printed;
//! - `symbols/<preset>/incremental_one_file`: index a child commit that
//!   changes one file;
//! - `symbols/<preset>/list_file`: `ListSymbols` of one indexed file;
//! - `symbols/<preset>/list_name`: `ListSymbols` of a name across the tree
//!   (the first in one of its Python files, its count printed).
//!
//! Results are in `docs/benchmarks.md`, "Symbols (R2.14)".
//!
//! ```sh
//! REEF_BENCH_PRESET=medium cargo bench -p reef-daemon --bench symbols
//! ```

#![allow(missing_docs, reason = "a criterion harness")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    reason = "a benchmark: a panic is the failure report, and it prints its one-shot figures"
)]

use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use reef_api::v1::{
    EditFilesRequest, FileEdit, IndexSymbolsRequest, ListFilesRequest, ListSymbolsRequest,
    SymbolIndexState, edit_files_request, file_edit,
};
use reef_daemon::testing::{SyntheticRepo, TestDaemon};
use reef_testing::synth::Preset;

fn preset() -> Preset {
    match std::env::var("REEF_BENCH_PRESET").as_deref() {
        Ok("small") => Preset::Small,
        Ok("large") => Preset::Large,
        _ => Preset::Medium,
    }
}

fn preset_name(preset: Preset) -> &'static str {
    match preset {
        Preset::Small => "small",
        Preset::Medium => "medium",
        Preset::Large => "large",
        Preset::Xl => "xl",
    }
}

struct Bench {
    rt: tokio::runtime::Runtime,
    daemon: TestDaemon,
    repo: SyntheticRepo,
}

impl Bench {
    fn index(&self, revision: &str) -> u64 {
        let status = self
            .rt
            .block_on(self.daemon.client.index_symbols(IndexSymbolsRequest {
                repo: self.repo.id.clone(),
                revision: revision.to_owned(),
                wait: Some(Duration::from_secs(3600).try_into().unwrap()),
                ..IndexSymbolsRequest::default()
            }))
            .unwrap()
            .status
            .unwrap();
        assert_eq!(status.state, SymbolIndexState::Complete as i32);
        status.files
    }

    /// A child of `on` with `path` rewritten; its commit id.
    fn edit(&self, on: &str, path: &str, n: u64) -> String {
        self.rt
            .block_on(self.daemon.client.edit_files(EditFilesRequest {
                repo: self.repo.id.clone(),
                target: Some(edit_files_request::Target::NewOn(on.to_owned())),
                edits: vec![FileEdit {
                    path: path.to_owned(),
                    action: Some(file_edit::Action::Content(
                        format!("def edited_{n}():\n    return {n}\n").repeat(200).into_bytes(),
                    )),
                    executable: None,
                }],
                ..EditFilesRequest::default()
            }))
            .unwrap()
            .change
            .unwrap()
            .commit_id
    }

    fn list(&self, request: ListSymbolsRequest) -> usize {
        self.rt
            .block_on(self.daemon.client.list_symbols(ListSymbolsRequest {
                repo: self.repo.id.clone(),
                ..request
            }))
            .unwrap()
            .symbols
            .len()
    }
}

fn symbols(c: &mut Criterion) {
    let preset = preset();
    let name = preset_name(preset);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (daemon, repo) = rt.block_on(TestDaemon::with_synthetic_repo(preset));
    let bench = Bench { rt, daemon, repo };
    // The synthetic repository's one commit; it has no bookmark.
    let main = "heads(all())";

    let started = Instant::now();
    let files = bench.index(main);
    let took = started.elapsed();
    let path = bench
        .daemon
        .handle
        .state()
        .symbols
        .path_of(bench.repo.id.parse().unwrap());
    let bytes: u64 = ["", "-wal"]
        .iter()
        .filter_map(|s| std::fs::metadata(format!("{}{s}", path.display())).ok())
        .map(|m| m.len())
        .sum();
    println!(
        "symbols/{name}/full_index: {files} supported files in {:.2} s; symbols.sqlite3 {:.1} MiB",
        took.as_secs_f64(),
        bytes as f64 / f64::from(1 << 20)
    );

    // A Python file of the tree, edited by every incremental iteration.
    let listing = bench
        .rt
        .block_on(bench.daemon.client.list_files(ListFilesRequest {
            repo: bench.repo.id.clone(),
            revision: main.to_owned(),
            recursive: true,
            ..ListFilesRequest::default()
        }))
        .unwrap();
    let python = listing
        .entries
        .iter()
        .find(|e| e.path.rsplit_once('.').is_some_and(|(_, ext)| ext == "py"))
        .map(|e| e.path.clone())
        .unwrap();

    // The most common name in main's tree, for the name lookup.
    let occurrences = bench
        .rt
        .block_on(bench.daemon.client.list_symbols(ListSymbolsRequest {
            repo: bench.repo.id.clone(),
            revision: main.to_owned(),
            path: python.clone(),
            ..ListSymbolsRequest::default()
        }))
        .unwrap()
        .symbols;
    let lookup = occurrences
        .first()
        .map_or_else(|| "main".to_owned(), |o| o.name.clone());
    let hits = bench.list(ListSymbolsRequest {
        revision: main.to_owned(),
        name: lookup.clone(),
        ..ListSymbolsRequest::default()
    });
    println!("symbols/{name}/list_name: `{lookup}` has {hits} occurrences in main");

    let mut group = c.benchmark_group(format!("symbols/{name}"));
    group.sample_size(20);
    let mut n = 0;
    group.bench_function("incremental_one_file", |b| {
        b.iter_custom(|iterations| {
            let mut total = Duration::ZERO;
            for _ in 0..iterations {
                n += 1;
                let child = bench.edit(main, &python, n);
                let started = Instant::now();
                bench.index(&child);
                total += started.elapsed();
            }
            total
        });
    });
    group.bench_function("list_file", |b| {
        b.iter(|| {
            bench.list(ListSymbolsRequest {
                revision: main.to_owned(),
                path: python.clone(),
                ..ListSymbolsRequest::default()
            })
        });
    });
    group.bench_function("list_name", |b| {
        b.iter(|| {
            bench.list(ListSymbolsRequest {
                revision: main.to_owned(),
                name: lookup.clone(),
                ..ListSymbolsRequest::default()
            })
        });
    });
    group.finish();
}

criterion_group!(benches, symbols);
criterion_main!(benches);
