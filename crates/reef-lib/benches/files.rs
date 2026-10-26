//! File benchmarks on the synthetic repository (R2.04): what reading a file,
//! listing a directory, and editing without a working copy cost in
//! `reef_lib::jj::files`, on a tree of the `REEF_BENCH_PRESET` size.
//! `docs/benchmarks.md` ("Files") says what each measures.
//!
//! Every benchmark is `files/<preset>/<name>`. The fixture is shared with
//! the other benches; the edits write commits on the working-copy commit
//! and never move it, so the files on disk stay as the head says.

#![allow(
    clippy::expect_used,
    reason = "a failed step aborts the benchmark run; there is no caller to report to"
)]

#[allow(
    dead_code,
    reason = "the shared fixture's mutations are for the other benches"
)]
mod common;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use jj_lib::repo_path::RepoPathBuf;
use reef_lib::jj::diff::EntryKind;
use reef_lib::jj::files::{self, Edit, EditAction, ListOptions};
use reef_lib::jj::revset::{self, RevsetScope};
use reef_lib::jj::{CommitIdHex, OpKind, OpMeta};

fn meta() -> OpMeta {
    OpMeta::new(OpKind::Edit, "bench: edit")
        .workspace("0123456789abcdef")
        .agent("bench")
}

fn content(path: &str, bytes: Vec<u8>) -> Edit {
    Edit {
        path: path.to_owned(),
        action: Some(EditAction::Content(bytes)),
        executable: None,
    }
}

fn path(text: &str) -> RepoPathBuf {
    RepoPathBuf::from_internal_string(text).expect("a repository path")
}

#[allow(
    clippy::print_stderr,
    clippy::too_many_lines,
    reason = "the fixture's identity is reported to whoever runs the benches; one fixture, every bench"
)]
fn file_benches(c: &mut Criterion) {
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
    let everything = |commit: &CommitIdHex| {
        let options = ListOptions {
            dir: None,
            recursive: true,
            after: None,
            limit: usize::MAX,
        };
        rt.block_on(files::list_files(
            &repo.readonly(),
            commit,
            &options,
            &|| false,
        ))
        .expect("list")
        .entries
    };
    let all = everything(&wc);
    // The first text file, which the generator keeps small.
    let small = all
        .iter()
        .find(|entry| entry.kind == EntryKind::File)
        .expect("a file")
        .path
        .clone();
    // The cursor before the last page of 100.
    let last_cursor = path(&all[all.len().saturating_sub(101)].path);
    // A 16 MiB file and a leaf to edit, on the working-copy commit.
    let big: Vec<u8> = (0..16u32 << 20)
        .map(|i| b'a' + u8::try_from(i % 26).expect("< 26"))
        .collect();
    let big_commit = rt
        .block_on(repo.new_change_on(&wc, &[content("bench-big.bin", big)], None, &meta()))
        .expect("big")
        .commit_id;
    let mut leaf = rt
        .block_on(repo.new_change_on(
            &wc,
            &[content("bench-edit.txt", b"value 0\n".to_vec())],
            None,
            &meta(),
        ))
        .expect("leaf")
        .commit_id;

    let mut group = c.benchmark_group("files");
    let name = fixture.preset_name();
    let head = repo.readonly();
    group.bench_function(BenchmarkId::new(name, "read_small"), |b| {
        let small = path(&small);
        b.iter(|| {
            rt.block_on(files::read_file(&head, &wc, &small, 0, 0))
                .expect("read");
        });
    });
    group.bench_function(BenchmarkId::new(name, "read_paged_16mib"), |b| {
        let big = path("bench-big.bin");
        b.iter(|| {
            let mut offset = 0;
            loop {
                let page = rt
                    .block_on(files::read_file(&head, &big_commit, &big, offset, 0))
                    .expect("read");
                if page.content.is_empty() {
                    break;
                }
                offset += u64::try_from(page.content.len()).expect("fits");
            }
        });
    });
    let list = |after: Option<RepoPathBuf>, recursive: bool| ListOptions {
        dir: None,
        recursive,
        after,
        limit: 100,
    };
    group.bench_function(BenchmarkId::new(name, "list_root"), |b| {
        let options = ListOptions {
            limit: 1000,
            ..list(None, false)
        };
        b.iter(|| {
            rt.block_on(files::list_files(&head, &wc, &options, &|| false))
                .expect("list");
        });
    });
    group.bench_function(BenchmarkId::new(name, "list_recursive_first"), |b| {
        let options = list(None, true);
        b.iter(|| {
            rt.block_on(files::list_files(&head, &wc, &options, &|| false))
                .expect("list");
        });
    });
    group.bench_function(BenchmarkId::new(name, "list_recursive_last"), |b| {
        let options = list(Some(last_cursor.clone()), true);
        b.iter(|| {
            let page = rt
                .block_on(files::list_files(&head, &wc, &options, &|| false))
                .expect("list");
            assert!(!page.more, "the last page");
        });
    });
    let mut n = 0u64;
    group.bench_function(BenchmarkId::new(name, "edit_one"), |b| {
        b.iter(|| {
            let edit = Edit {
                path: "bench-edit.txt".to_owned(),
                action: Some(EditAction::Replace {
                    old: format!("value {n}\n"),
                    new: format!("value {}\n", n + 1),
                    all: false,
                }),
                executable: None,
            };
            n += 1;
            leaf = rt
                .block_on(repo.edit_commit(&leaf, &[edit], None, &meta()))
                .expect("edit")
                .expect("a change")
                .commit_id;
        });
    });
    let hundred: Vec<Edit> = (0..100)
        .map(|i| {
            content(
                &format!("bench-new/f{i:03}.txt"),
                format!("file {i}\n").into_bytes(),
            )
        })
        .collect();
    group.bench_function(BenchmarkId::new(name, "new_on_hundred"), |b| {
        b.iter(|| {
            rt.block_on(repo.new_change_on(&wc, &hundred, None, &meta()))
                .expect("new change");
        });
    });
    group.finish();
}

criterion_group!(benches, file_benches);
criterion_main!(benches);
