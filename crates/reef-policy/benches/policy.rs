//! Policy load and evaluation (R3.07, `docs/benchmarks.md`):
//! `cargo bench -p reef-policy --bench policy`.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use reef_policy::input::{Change, Evidence, File, Key, Provenance};
use reef_policy::{Input, Policy};

fn key(identity: &str, actor: &str) -> Key {
    Key {
        id: format!("k-{identity}"),
        status: "bound".to_owned(),
        identity: identity.to_owned(),
        identity_kind: "ci".to_owned(),
        actor: actor.to_owned(),
        presence: "none".to_owned(),
        ..Key::default()
    }
}

fn input(paths: usize, evidence: usize, provenance: usize) -> Input {
    let paths: Vec<String> = (0..paths)
        .map(|i| format!("crates/c{}/src/m{}/f{i}.rs", i % 50, i % 7))
        .collect();
    Input {
        change: Change {
            id: "c".to_owned(),
            tree: "t".to_owned(),
            files: paths
                .iter()
                .map(|path| File {
                    path: path.clone(),
                    status: "modified".to_owned(),
                    lines_added: 3,
                    lines_removed: 1,
                    size: 1_000,
                    ..File::default()
                })
                .collect(),
            lines_added: 3 * i64::try_from(paths.len()).unwrap(),
            lines_removed: i64::try_from(paths.len()).unwrap(),
            paths,
            ..Change::default()
        },
        evidence: (0..evidence)
            .map(|i| Evidence {
                id: format!("e{i:05}"),
                kind: ["test", "lint", "build"][i % 3].to_owned(),
                result: "pass".to_owned(),
                added_at: i64::try_from(i).unwrap(),
                key: key(if i % 2 == 0 { "ci-main" } else { "other" }, "executor"),
                ..Evidence::default()
            })
            .collect(),
        provenance: (0..provenance)
            .map(|i| Provenance {
                id: format!("p{i}"),
                signature: "valid".to_owned(),
                key: key("alice", "agent"),
                ..Provenance::default()
            })
            .collect(),
        ..Input::default()
    }
}

/// 20 rules with globs, of each kind.
fn large_policy() -> String {
    let mut text = String::from(
        "version = 1\n[owners]\nidentities = [\"alice\"]\n[attestors.ci]\nidentities = [\"ci-main\"]\n\
         [limits]\nmax_files = 200000\nmax_lines = 10000000\nmax_file_size = \"5MiB\"\n",
    );
    for i in 0..8 {
        writeln!(
            text,
            "[[require]]\nname = \"r{i}\"\npaths = [\"crates/c{i}*/**\"]\nkinds = [\"test\", \"lint\"]\nattestors = [\"ci\"]"
        )
        .unwrap();
    }
    for i in 0..6 {
        writeln!(
            text,
            "[[forbid]]\nname = \"f{i}\"\npaths = [\"secrets{i}/**\", \"**/*.pem{i}\"]"
        )
        .unwrap();
    }
    for i in 0..5 {
        writeln!(
            text,
            "[[rule]]\nname = \"x{i}\"\nexpr = \"any_glob(change.paths, ['crates/c{i}/**']) || change.files.all(f, f.size < 5000000)\""
        )
        .unwrap();
    }
    text
}

/// 200 cheap rules.
fn many_rules() -> String {
    let mut text = String::from("version = 1\n[owners]\nidentities = [\"alice\"]\n");
    for i in 0..200 {
        writeln!(
            text,
            "[[rule]]\nname = \"r{i}\"\nwhen = \"change.lines_added > {i}\"\nexpr = \"change.paths.exists(p, p.startsWith('crates/c{}/'))\"",
            i % 50
        )
        .unwrap();
    }
    text
}

fn bench(c: &mut Criterion) {
    let starter = reef_policy::starter("alice");
    c.bench_function("policy/load_starter", |b| {
        b.iter(|| Policy::load(black_box(starter.as_bytes())).unwrap());
    });
    let many_text = many_rules();
    c.bench_function("policy/load_200_rules", |b| {
        b.iter(|| Policy::load(black_box(many_text.as_bytes())).unwrap());
    });
    let policy = Policy::load(starter.as_bytes()).unwrap();
    let small = input(50, 20, 10);
    c.bench_function("policy/evaluate_small", |b| {
        b.iter(|| policy.evaluate(black_box(&small)));
    });
    let large = Policy::load(large_policy().as_bytes()).unwrap();
    let big = input(100_000, 1_000, 10);
    let many = Policy::load(many_text.as_bytes()).unwrap();
    let mut group = c.benchmark_group("policy/large");
    group.sample_size(10);
    group.bench_function("evaluate_100k_paths", |b| {
        b.iter(|| large.evaluate(black_box(&big)));
    });
    group.bench_function("evaluate_200_rules_100k_paths", |b| {
        b.iter(|| many.evaluate(black_box(&big)));
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
