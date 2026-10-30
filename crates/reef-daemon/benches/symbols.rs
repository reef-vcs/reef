//! What the symbol index costs through the daemon (R2.14), on a synthetic
//! repository of `REEF_BENCH_PRESET` (`small`, `medium`, or `large`):
//!
//! - `full_index`: the first index of the tree, timed once (a second would
//!   find every entry), with the cache's size after it, printed;
//! - `symbols/<preset>/incremental_one_file`: index a child commit that
//!   changes one file;
//! - `symbols/<preset>/list_file`: `ListSymbols` of one indexed file;
//! - `symbols/<preset>/list_name`: `ListSymbols` of a name across the tree
//!   (the first in one of its Python files, its count printed), every page;
//! - `symbols/<preset>/list_name_deep`: one page of 10 of that name, 40
//!   pages in (R2.16);
//! - `symbols/<preset>/diff_one_file`, beside `diff_one_file_plain`:
//!   `GetDiff` with symbols of a child that rewrites one Python file with
//!   20 definitions, warm, call sites 10 (R2.15);
//! - `symbols/<preset>/diff_100_files` and `diff_paged` (one page of 20 of
//!   them): a child rewriting 100 Python files, warm; the same diff with no
//!   entries on either side is timed once, `diff_100_files_cold`;
//! - `symbols/<preset>/call_sites_common`: one changed function named as
//!   `list_name`'s name, whose call sites are looked up by that name.
//!
//! Results are in `docs/benchmarks.md`, "Symbols (R2.14)" and "Symbol-level
//! diff (R2.15)".
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

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use reef_api::v1::{
    DiffFormat, EditFilesRequest, FileEdit, GetDiffRequest, IndexSymbolsRequest, ListFilesRequest,
    ListSymbolsRequest, ListSymbolsResponse, SymbolIndexState, edit_files_request, file_edit,
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
        self.index_commit(revision).0
    }

    /// Indexes `revision`: its supported files, and the commit it names.
    fn index_commit(&self, revision: &str) -> (u64, String) {
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
        (status.files, status.commit_id)
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

    /// A child of `on` writing `files`; its commit id.
    fn write(&self, on: &str, files: &[(String, Vec<u8>)]) -> String {
        self.rt
            .block_on(
                self.daemon.client.edit_files(EditFilesRequest {
                    repo: self.repo.id.clone(),
                    target: Some(edit_files_request::Target::NewOn(on.to_owned())),
                    edits: files
                        .iter()
                        .map(|(path, bytes)| FileEdit {
                            path: path.clone(),
                            action: Some(file_edit::Action::Content(bytes.clone())),
                            executable: None,
                        })
                        .collect(),
                    ..EditFilesRequest::default()
                }),
            )
            .unwrap()
            .change
            .unwrap()
            .commit_id
    }

    /// One page of `to`'s diff against `from`, with symbols or without.
    fn diff(&self, from: &str, to: &str, symbols: bool, page_size: i32) -> usize {
        self.rt
            .block_on(self.daemon.client.get_diff_page(GetDiffRequest {
                repo: self.repo.id.clone(),
                from_revision: from.to_owned(),
                to_revision: to.to_owned(),
                format: DiffFormat::Summary.into(),
                symbols,
                page_size,
                ..GetDiffRequest::default()
            }))
            .unwrap()
            .files
            .len()
    }

    /// One page of `request`.
    fn page(&self, request: ListSymbolsRequest) -> ListSymbolsResponse {
        self.rt
            .block_on(self.daemon.client.list_symbols_page(ListSymbolsRequest {
                repo: self.repo.id.clone(),
                ..request
            }))
            .unwrap()
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
    let started = Instant::now();
    // The synthetic repository's one commit, which has no bookmark, by id:
    // the benches below add children beside it.
    let (files, main) = bench.index_commit("heads(all())");
    let main = main.as_str();
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
    list_name_deep(c, &bench, main, &lookup);
    diffs(c, &bench, main, &listing.entries, &python, &lookup);
}

/// R2.16's later page: page 40 of `lookup`, by pages of 10, from the token
/// the 39 pages before it end with.
fn list_name_deep(c: &mut Criterion, bench: &Bench, main: &str, lookup: &str) {
    let name = preset_name(preset());
    let request = |token: String| ListSymbolsRequest {
        revision: main.to_owned(),
        name: lookup.to_owned(),
        page_size: 10,
        page_token: token,
        ..ListSymbolsRequest::default()
    };
    let mut token = String::new();
    for _ in 0..39 {
        token = bench.page(request(token)).next_page_token;
        if token.is_empty() {
            println!("symbols/{name}/list_name_deep: skipped, `{lookup}` has under 40 pages");
            return;
        }
    }
    let mut group = c.benchmark_group(format!("symbols/{name}"));
    group.sample_size(20);
    group.bench_function("list_name_deep", |b| {
        b.iter(|| bench.page(request(token.clone())).symbols.len());
    });
    group.finish();
}

/// R2.15's diffs: 20 definitions per file, each calling `lookup`.
fn diffs(
    c: &mut Criterion,
    bench: &Bench,
    main: &str,
    entries: &[reef_api::v1::FileEntry],
    python: &str,
    lookup: &str,
) {
    let name = preset_name(preset());
    let pythons: Vec<String> = entries
        .iter()
        .filter(|e| e.path.rsplit_once('.').is_some_and(|(_, ext)| ext == "py"))
        .map(|e| e.path.clone())
        .take(100)
        .collect();
    let source = |salt: &str| -> Vec<u8> {
        let mut text = String::new();
        for i in 0..20 {
            let _ = write!(
                text,
                "def bench_{i}():\n    return {lookup}({salt:?})\n\n\n"
            );
        }
        text.into_bytes()
    };
    let python = python.to_owned();
    let one_before = bench.write(main, &[(python.clone(), source("a"))]);
    let one_after = bench.write(&one_before, &[(python.clone(), source("b"))]);
    let rewrite = |salt: &str| -> Vec<(String, Vec<u8>)> {
        pythons.iter().map(|p| (p.clone(), source(salt))).collect()
    };
    let many_before = bench.write(main, &rewrite("c"));
    let many_after = bench.write(&many_before, &rewrite("d"));
    let cold_after = bench.write(&many_before, &rewrite("e"));
    let common_before = bench.write(
        main,
        &[(
            python.clone(),
            format!("def {lookup}():\n    return 1\n").into_bytes(),
        )],
    );
    let common_after = bench.write(
        &common_before,
        &[(
            python.clone(),
            format!("def {lookup}():\n    return 2\n").into_bytes(),
        )],
    );
    for commit in [&one_after, &many_after, &common_after] {
        bench.index(commit);
    }
    // Cold: neither side's new contents have entries yet, though the rest
    // of the tree is indexed.
    let started = Instant::now();
    bench.diff(&many_after, &cold_after, true, 1000);
    println!(
        "symbols/{name}/diff_100_files_cold: {} files in {:.0} ms",
        pythons.len(),
        started.elapsed().as_secs_f64() * 1000.0
    );

    let mut group = c.benchmark_group(format!("symbols/{name}"));
    group.sample_size(20);
    group.bench_function("diff_one_file_plain", |b| {
        b.iter(|| bench.diff(&one_before, &one_after, false, 100));
    });
    group.bench_function("diff_one_file", |b| {
        b.iter(|| bench.diff(&one_before, &one_after, true, 100));
    });
    group.bench_function("diff_100_files", |b| {
        b.iter(|| bench.diff(&many_before, &many_after, true, 1000));
    });
    group.bench_function("diff_paged", |b| {
        b.iter(|| bench.diff(&many_before, &many_after, true, 20));
    });
    group.bench_function("call_sites_common", |b| {
        b.iter(|| bench.diff(&common_before, &common_after, true, 100));
    });
    group.finish();
}

criterion_group!(benches, symbols);
criterion_main!(benches);
