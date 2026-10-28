//! Fixtures and golden files for reef-symbols' tests.

#![allow(
    dead_code,
    unreachable_pub,
    reason = "each test binary uses part of it"
)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers: a panic is the failure report"
)]

use std::path::PathBuf;

use reef_symbols::{FileSymbols, Language, Limits, Range};
use serde_json::{Value, json};

pub fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

pub fn fixture(path: &str) -> Vec<u8> {
    std::fs::read(fixture_dir().join(path)).expect("fixture")
}

pub fn extract(path: &str) -> FileSymbols {
    let language = Language::from_path(path).expect("a supported fixture");
    reef_symbols::extract(language, path, &fixture(path), &Limits::default()).expect("extracts")
}

fn range(r: Range) -> Value {
    json!([r.start_line, r.start_col, r.end_line, r.end_col])
}

/// One line per occurrence, for goldens a reviewer can read.
pub fn render(symbols: &FileSymbols) -> Value {
    let rows: Vec<Value> = symbols
        .occurrences
        .iter()
        .map(|o| {
            json!({
                "name": o.name,
                "role": format!("{:?}", o.role),
                "kind": format!("{:?}", o.kind),
                "symbol": o.symbol,
                "range": range(o.range),
                "definition_range": o.definition_range.map(range),
                "enclosing": o.enclosing.map(|e| symbols.occurrences[e as usize].name.clone()),
            })
        })
        .collect();
    json!({ "has_errors": symbols.has_errors, "occurrences": rows })
}

/// Compares `value` with `tests/fixtures/golden/<name>.json`; `REEF_BLESS=1`
/// rewrites it.
pub fn golden(name: &str, value: &Value) {
    let path = fixture_dir().join("golden").join(format!("{name}.json"));
    let text = serde_json::to_string_pretty(value).expect("json") + "\n";
    if std::env::var_os("REEF_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("golden dir");
        std::fs::write(&path, text).expect("write golden");
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("{} is missing; run with REEF_BLESS=1", path.display()));
    assert_eq!(
        text, want,
        "{name} differs from its golden; REEF_BLESS=1 rewrites it"
    );
}
