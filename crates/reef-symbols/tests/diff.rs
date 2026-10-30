//! Tests 1 to 5 of R2.15: definitions matched across a diff's two sides,
//! renamed files, moves, duplicates, and `expand_symbols` entries.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers: a panic is the failure report"
)]

mod common;

use std::collections::BTreeMap;

use common::{fixture, golden};
use reef_symbols::diff::{self, Change, ChangeKind, Definition};
use reef_symbols::{Kind, Language, Limits};
use serde_json::{Value, json};

/// The definitions of fixture `diff/<fixture>` as the file at `path`.
fn side(path: &str, fixture_path: &str) -> Vec<Definition> {
    let bytes = fixture(&format!("diff/{fixture_path}"));
    let language = Language::from_path(path).unwrap();
    let symbols = reef_symbols::extract(language, path, &bytes, &Limits::default()).unwrap();
    diff::definitions(path, &symbols.occurrences)
}

fn sides(files: &[(&str, &str)]) -> Vec<Definition> {
    files.iter().flat_map(|(path, f)| side(path, f)).collect()
}

fn no_renames() -> BTreeMap<String, String> {
    BTreeMap::new()
}

/// One row per change, for goldens and asserts.
fn render(changes: &[Change], old: &[Definition], new: &[Definition]) -> Value {
    let rows: Vec<Value> = changes
        .iter()
        .map(|c| {
            let o = c.old.map(|i| &old[i]);
            let n = c.new.map(|i| &new[i]);
            json!({
                "kind": format!("{:?}", c.kind),
                "symbol": n.or(o).map(|d| d.symbol.clone()),
                "old_symbol": o
                    .filter(|o| n.is_some_and(|n| n.symbol != o.symbol))
                    .map(|o| o.symbol.clone()),
                "old_path": o
                    .filter(|o| n.is_some_and(|n| n.path != o.path))
                    .map(|o| o.path.clone()),
            })
        })
        .collect();
    Value::Array(rows)
}

/// `(kind, name)` of each change.
fn kinds(changes: &[Change], old: &[Definition], new: &[Definition]) -> Vec<(ChangeKind, String)> {
    changes
        .iter()
        .map(|c| {
            let d = c.new.map(|i| &new[i]).or(c.old.map(|i| &old[i]));
            (c.kind, d.unwrap().name.clone())
        })
        .collect()
}

#[test]
fn match_in_one_file() {
    let old = sides(&[
        ("parser.py", "before/parser.py"),
        ("lib.rs", "before/lib.rs"),
    ]);
    let new = sides(&[("parser.py", "after/parser.py"), ("lib.rs", "after/lib.rs")]);
    let changes = diff::match_definitions(&old, &new, &no_renames());
    let mut got = kinds(&changes, &old, &new);
    got.sort();
    let mut want = vec![
        (ChangeKind::Added, "helper".to_owned()),
        (ChangeKind::Added, "twice".to_owned()),
        (ChangeKind::Removed, "legacy".to_owned()),
        (ChangeKind::Removed, "origin".to_owned()),
        (ChangeKind::Changed, "Parser".to_owned()),
        (ChangeKind::Changed, "parse".to_owned()),
        (ChangeKind::Changed, "norm".to_owned()),
    ];
    // The impl's type changes with its methods.
    let point = got
        .iter()
        .any(|(k, n)| *k == ChangeKind::Changed && n == "Point");
    if point {
        want.push((ChangeKind::Changed, "Point".to_owned()));
    }
    want.sort();
    assert_eq!(got, want);
    // Shifted by a line, with the same bytes: not reported.
    for unchanged in ["tokenize", "__init__", "LIMIT", "new"] {
        assert!(
            got.iter().all(|(_, n)| n != unchanged),
            "{unchanged} reported"
        );
    }
    assert!(changes.iter().all(|c| !c.undigested));
    golden("diff_one_file", &render(&changes, &old, &new));
}

#[test]
fn match_renamed_files() {
    let renames: BTreeMap<String, String> = [("a.py".to_owned(), "b/a.py".to_owned())]
        .into_iter()
        .collect();
    let old = side("a.py", "before/parser.py");
    let same = side("b/a.py", "before/parser.py");
    assert_eq!(diff::match_definitions(&old, &same, &renames), []);

    let new = side("b/a.py", "after/parser.py");
    let changes = diff::match_definitions(&old, &new, &renames);
    let parse = changes
        .iter()
        .find(|c| c.new.is_some_and(|n| new[n].name == "parse"))
        .expect("parse changed");
    assert_eq!(parse.kind, ChangeKind::Changed);
    let (o, n) = (&old[parse.old.unwrap()], &new[parse.new.unwrap()]);
    assert_eq!(o.symbol, "reef-tags . . . `a.py`/Parser#parse().");
    assert_eq!(n.symbol, "reef-tags . . . b/`a.py`/Parser#parse().");
    assert!(
        changes.iter().all(|c| c.kind != ChangeKind::Moved),
        "a renamed file's definitions are not moves"
    );

    // Go: the package is the directory.
    let go: BTreeMap<String, String> = [("x/store.go".to_owned(), "y/store.go".to_owned())]
        .into_iter()
        .collect();
    let old = side("x/store.go", "before/store.go");
    let new = side("y/store.go", "before/store.go");
    assert_eq!(diff::match_definitions(&old, &new, &go), []);

    // SCIP symbols: a path under a project root is rewritten, a
    // package-scoped one is left alone.
    assert_eq!(
        diff::rewrite_for_rename(
            "scip-typescript npm web 1.0.0 src/`a.ts`/f().",
            "web/src/a.ts",
            "web/lib/a.ts"
        )
        .as_deref(),
        Some("scip-typescript npm web 1.0.0 lib/`a.ts`/f().")
    );
    assert_eq!(
        diff::rewrite_for_rename(
            "scip-go gomod example.com/m v1 `example.com/m/x`/T#M().",
            "x/a.go",
            "z/a.go"
        ),
        None
    );
    // A symbol shorter than the path (a module, or a package in a deep
    // directory) spells no path, and does not panic (review finding 1).
    for (symbol, from, to) in [
        (
            "scip-typescript npm web 1.0.0 src/`a.ts`/",
            "web/src/a.ts",
            "web/lib/a.ts",
        ),
        ("scip-go gomod m v1 `m/x`/F().", "a/b/c/d.go", "a/b/c/e.go"),
        (
            "rust-analyzer cargo x 0.1 module/Foo#",
            "crates/x/src/module.rs",
            "crates/x/src/m.rs",
        ),
    ] {
        let _ = diff::rewrite_for_rename(symbol, from, to);
    }
}

/// Equal bodies pair within a file before across files: Go's build-tagged
/// variants of one function (review finding 8).
#[test]
fn equal_digests_pair_in_one_file_first() {
    let def = |path: &str, digest: u64| Definition {
        path: path.to_owned(),
        symbol: "reef-tags . . . x/Foo().".to_owned(),
        name: "Foo".to_owned(),
        kind: Kind::Function,
        digest: Some(digest),
        seq: 0,
    };
    let old = vec![def("x/foo_darwin.go", 1), def("x/foo_linux.go", 1)];
    let new = vec![def("x/foo_darwin.go", 2), def("x/foo_linux.go", 1)];
    let changes = diff::match_definitions(&old, &new, &no_renames());
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].kind, ChangeKind::Changed);
    assert_eq!(new[changes[0].new.unwrap()].path, "x/foo_darwin.go");
    assert_eq!(old[changes[0].old.unwrap()].path, "x/foo_darwin.go");
}

#[test]
fn match_moves() {
    // Python names the file: a move is found by name, kind and bytes.
    let old = sides(&[("a.py", "before/parser.py"), ("b.py", "before/run.py")]);
    let tokenize = |defs: &[Definition], path: &str| {
        defs.iter()
            .position(|d| d.name == "tokenize" && d.path == path)
            .expect("tokenize")
    };
    let mut new = side("a.py", "before/parser.py");
    let moved = new.remove(tokenize(&new, "a.py"));
    new.extend(side("b.py", "before/run.py"));
    new.push(Definition {
        path: "b.py".to_owned(),
        symbol: "reef-tags . . . `b.py`/tokenize().".to_owned(),
        seq: 100,
        ..moved.clone()
    });
    let changes = diff::match_definitions(&old, &new, &no_renames());
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].kind, ChangeKind::Moved);
    assert_eq!(old[changes[0].old.unwrap()].path, "a.py");
    assert_eq!(new[changes[0].new.unwrap()].path, "b.py");

    // Moved and changed: a removal and an addition.
    let last = new.len() - 1;
    new[last].digest = Some(new[last].digest.unwrap() ^ 1);
    let changes = diff::match_definitions(&old, &new, &no_renames());
    let mut got = kinds(&changes, &old, &new);
    got.sort();
    assert_eq!(
        got,
        [
            (ChangeKind::Added, "tokenize".to_owned()),
            (ChangeKind::Removed, "tokenize".to_owned())
        ]
    );

    // Go names the package: a move between its files keeps the symbol.
    let old = side("pkg/store.go", "before/store.go");
    let new = sides(&[
        ("pkg/store.go", "after/store.go"),
        ("pkg/put.go", "after/put.go"),
    ]);
    let changes = diff::match_definitions(&old, &new, &no_renames());
    assert_eq!(
        kinds(&changes, &old, &new),
        [(ChangeKind::Moved, "Put".to_owned())]
    );
    let new = sides(&[
        ("pkg/store.go", "after/store.go"),
        ("pkg/put.go", "after/put_changed.go"),
    ]);
    let changes = diff::match_definitions(&old, &new, &no_renames());
    assert_eq!(
        kinds(&changes, &old, &new),
        [(ChangeKind::Changed, "Put".to_owned())]
    );
    assert_eq!(old[changes[0].old.unwrap()].path, "pkg/store.go");
    assert_eq!(new[changes[0].new.unwrap()].path, "pkg/put.go");

    // Two identical removed definitions and one added: ambiguous, so not a
    // move.
    let def = |path: &str| Definition {
        path: path.to_owned(),
        symbol: format!("reef-tags . . . `{path}`/f()."),
        name: "f".to_owned(),
        kind: Kind::Function,
        digest: Some(7),
        seq: 0,
    };
    let old = vec![def("a.py"), def("c.py")];
    let new = vec![def("b.py")];
    let changes = diff::match_definitions(&old, &new, &no_renames());
    let mut got: Vec<ChangeKind> = changes.iter().map(|c| c.kind).collect();
    got.sort();
    assert_eq!(
        got,
        [ChangeKind::Added, ChangeKind::Removed, ChangeKind::Removed]
    );
}

#[test]
fn match_duplicates() {
    let old = side("pick.ts", "before/pick.ts");
    let new = side("pick.ts", "after/pick.ts");
    assert_eq!(old.iter().filter(|d| d.name == "pick").count(), 4);
    let changes = diff::match_definitions(&old, &new, &no_renames());
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].kind, ChangeKind::Removed);
    let removed = &old[changes[0].old.unwrap()];
    let second = old
        .iter()
        .filter(|d| d.name == "pick")
        .nth(1)
        .expect("second");
    assert_eq!(
        removed.seq, second.seq,
        "the middle overload is the one removed"
    );

    let old = side("run.py", "before/run.py");
    let new = side("run.py", "after/run.py");
    assert_eq!(old.len(), 2);
    assert_eq!(old[0].symbol, old[1].symbol);
    let changes = diff::match_definitions(&old, &new, &no_renames());
    assert_eq!(
        kinds(&changes, &old, &new),
        [(ChangeKind::Changed, "run".to_owned())]
    );
    assert_eq!(new[changes[0].new.unwrap()].seq, new[1].seq);
}

#[test]
fn expand_entries() {
    let symbol = "reef-tags . . . `parser.py`/Parser#parse().";
    for entry in [
        symbol,
        "Parser#parse().",
        "Parser#parse()",
        "parse",
        "`parser.py`/Parser#parse()",
    ] {
        assert!(
            diff::matches_expand(entry, symbol, None, "parse"),
            "{entry}"
        );
    }
    assert!(diff::matches_expand(
        "reef-tags . . . `old.py`/Parser#parse().",
        symbol,
        Some("reef-tags . . . `old.py`/Parser#parse()."),
        "parse"
    ));
    for entry in ["arse#parse()", "", "Parser", "parse(x)"] {
        assert!(
            !diff::matches_expand(entry, symbol, None, "parse"),
            "{entry}"
        );
    }
}
