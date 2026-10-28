//! Tag extraction throughput per language (R2.14): about 2,000 lines of
//! each through [`reef_symbols::extract`]. Python, TypeScript and Go repeat
//! this crate's test fixtures; Rust is Reef's own `reef-lib/src/jj/files.rs`.
//! Results are in `docs/benchmarks.md`, "Symbols (R2.14)".
//!
//! ```sh
//! cargo bench -p reef-symbols --bench extract
//! ```

#![allow(missing_docs, reason = "a criterion harness")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a benchmark: a panic is the failure report"
)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use reef_symbols::{Language, Limits};

/// `text` repeated to at least `lines` lines.
fn repeat_to(text: &str, lines: usize) -> Vec<u8> {
    let each = text.lines().count().max(1);
    text.repeat(lines.div_ceil(each)).into_bytes()
}

fn inputs() -> Vec<(Language, &'static str, Vec<u8>)> {
    let python = include_str!("../tests/fixtures/parser.py");
    let typescript = include_str!("../tests/fixtures/shapes.ts");
    let go = include_str!("../tests/fixtures/pkg/store/store.go");
    vec![
        (Language::Python, "bench.py", repeat_to(python, 2_000)),
        (
            Language::TypeScript,
            "bench.ts",
            repeat_to(typescript, 2_000),
        ),
        (Language::Go, "pkg/bench.go", repeat_to(go, 2_000)),
        (
            Language::Rust,
            "bench.rs",
            repeat_to(include_str!("../../reef-lib/src/jj/files.rs"), 2_000),
        ),
    ]
}

fn extract(c: &mut Criterion) {
    let limits = Limits {
        parse_timeout: std::time::Duration::from_secs(60),
        ..Limits::default()
    };
    let mut group = c.benchmark_group("extract");
    for (language, path, bytes) in inputs() {
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_function(language.name(), |b| {
            b.iter(|| reef_symbols::extract(language, path, black_box(&bytes), &limits).unwrap());
        });
    }
    group.finish();
}

criterion_group!(benches, extract);
criterion_main!(benches);
