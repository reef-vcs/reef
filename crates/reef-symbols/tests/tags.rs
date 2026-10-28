//! Tests 1 to 4 and 6 to 7 of R2.14: tag extraction per language, ranges,
//! digests and skips.

mod common;

use common::{extract, golden, render};
use reef_symbols::{FileSymbols, Kind, Occurrence, Role};

fn definition<'a>(symbols: &'a FileSymbols, name: &str) -> &'a Occurrence {
    symbols
        .occurrences
        .iter()
        .find(|o| o.role == Role::Definition && o.name == name)
        .unwrap_or_else(|| panic!("no definition of {name}"))
}

fn references<'a>(symbols: &'a FileSymbols, name: &str) -> Vec<&'a Occurrence> {
    symbols
        .occurrences
        .iter()
        .filter(|o| o.role == Role::Reference && o.name == name)
        .collect()
}

fn enclosing_name(symbols: &FileSymbols, o: &Occurrence) -> Option<String> {
    o.enclosing
        .map(|e| symbols.occurrences[e as usize].name.clone())
}

#[test]
fn tags_python() {
    let symbols = extract("parser.py");
    golden("parser.py", &render(&symbols));
    let parse = definition(&symbols, "parse");
    assert_eq!(parse.kind, Kind::Method);
    assert_eq!(
        parse.symbol.as_deref(),
        Some("reef-tags . . . `parser.py`/Parser#parse().")
    );
    // A decorated definition's range starts at its decorator.
    assert_eq!(parse.definition_range.unwrap().start_line, 10);
    assert_eq!(
        definition(&symbols, "make_parser")
            .definition_range
            .unwrap()
            .start_line,
        22
    );
    assert_eq!(
        definition(&symbols, "step").symbol.as_deref(),
        Some("reef-tags . . . `parser.py`/Parser#build().step().")
    );
    assert_eq!(definition(&symbols, "MAX_DEPTH").kind, Kind::Constant);
    assert!(
        symbols.occurrences.iter().all(|o| o.name != "default_name"),
        "a lower-case module name is not a constant"
    );
    assert_eq!(definition(&symbols, "Parser").kind, Kind::Class);
    for (name, caller) in [
        ("tokenize", "parse"),
        ("build", "parse"),
        ("emit", "step"),
        ("Parser", "make_parser"),
    ] {
        let found = references(&symbols, name);
        assert_eq!(found.len(), 1, "one reference to {name}");
        assert!(
            found
                .iter()
                .all(|r| enclosing_name(&symbols, r).as_deref() == Some(caller)),
            "{name} is called from {caller}"
        );
    }
    assert_eq!(references(&symbols, "Base").len(), 1);
    assert_eq!(references(&symbols, "Logged").len(), 1);
}

#[test]
fn tags_typescript() {
    let symbols = extract("shapes.ts");
    golden("shapes.ts", &render(&symbols));
    for (name, kind) in [
        ("Geometry", Kind::Module),
        ("Shape", Kind::Interface),
        ("Color", Kind::Enum),
        ("Point", Kind::Type),
        ("Base", Kind::Class),
        ("Circle", Kind::Class),
        ("radius", Kind::Field),
        ("area", Kind::Method),
        ("square", Kind::Function),
        ("makeCircle", Kind::Function),
    ] {
        assert_eq!(definition(&symbols, name).kind, kind, "{name}");
    }
    assert!(symbols.occurrences.iter().all(|o| o.name != "constructor"));
    assert_eq!(
        definition(&symbols, "Shape").symbol.as_deref(),
        Some("reef-tags . . . `shapes.ts`/Geometry/Shape#")
    );
    let square = references(&symbols, "square");
    assert_eq!(square.len(), 1);
    assert_eq!(enclosing_name(&symbols, square[0]).as_deref(), Some("area"));
    assert_eq!(references(&symbols, "Base").len(), 1, "extends");
    assert_eq!(
        references(&symbols, "Circle").len(),
        2,
        "new and an annotation"
    );

    let tsx = extract("view.tsx");
    golden("view.tsx", &render(&tsx));
    assert!(!tsx.has_errors, "JSX parses");
    assert_eq!(definition(&tsx, "Button").kind, Kind::Function);
    assert_eq!(definition(&tsx, "Panel").kind, Kind::Function);
    assert_eq!(definition(&tsx, "Props").kind, Kind::Interface);
    let button = references(&tsx, "Button");
    assert_eq!(button.len(), 1);
    assert_eq!(enclosing_name(&tsx, button[0]).as_deref(), Some("Panel"));

    let js = extract("app.js");
    golden("app.js", &render(&js));
    assert!(!js.has_errors, "JavaScript parses with the TSX grammar");
    assert_eq!(definition(&js, "get").kind, Kind::Method);
    assert_eq!(definition(&js, "open").kind, Kind::Function);
    assert_eq!(definition(&js, "close").kind, Kind::Function);
    assert_eq!(references(&js, "Store").len(), 1);
    assert_eq!(
        enclosing_name(&js, references(&js, "flush")[0]).as_deref(),
        Some("close")
    );
}

#[test]
fn tags_go() {
    let symbols = extract("pkg/store/store.go");
    golden("store.go", &render(&symbols));
    let other = extract("pkg/store/close.go");
    golden("close.go", &render(&other));
    // `*Store` and `Store` receivers give one type, scoped by the directory.
    assert_eq!(
        definition(&symbols, "Get").symbol.as_deref(),
        Some("reef-tags . . . pkg/store/Store#Get().")
    );
    assert_eq!(
        definition(&symbols, "Close").symbol.as_deref(),
        Some("reef-tags . . . pkg/store/Store#Close().")
    );
    assert_eq!(
        definition(&other, "Flush").symbol.as_deref(),
        Some("reef-tags . . . pkg/store/Store#Flush().")
    );
    assert_eq!(
        definition(&symbols, "Store").symbol,
        Some("reef-tags . . . pkg/store/Store#".to_owned())
    );
    for (name, kind) in [
        ("Store", Kind::Struct),
        ("Getter", Kind::Interface),
        ("ID", Kind::Type),
        ("New", Kind::Function),
        ("Get", Kind::Method),
        ("Version", Kind::Constant),
        ("Default", Kind::Variable),
    ] {
        assert_eq!(definition(&symbols, name).kind, kind, "{name}");
    }
    let close = references(&other, "Close");
    assert_eq!(close.len(), 1);
    assert_eq!(enclosing_name(&other, close[0]).as_deref(), Some("Flush"));
    assert_eq!(references(&symbols, "Println").len(), 1);
}

#[test]
fn tags_rust() {
    let symbols = extract("lib.rs");
    golden("lib.rs", &render(&symbols));
    for (name, kind) in [
        ("Foo", Kind::Struct),
        ("value", Kind::Field),
        ("Mode", Kind::Enum),
        ("Run", Kind::Interface),
        ("run", Kind::Method),
        ("Pair", Kind::Type),
        ("new", Kind::Method),
        ("bar", Kind::Method),
        ("fmt", Kind::Method),
        ("inner", Kind::Module),
        ("twice", Kind::Macro),
        ("LIMIT", Kind::Constant),
        ("NAME", Kind::Constant),
    ] {
        assert_eq!(definition(&symbols, name).kind, kind, "{name}");
    }
    assert_eq!(
        definition(&symbols, "bar").symbol.as_deref(),
        Some("reef-tags . . . `lib.rs`/Foo#bar().")
    );
    // `impl Trait for T` belongs to `T`.
    assert_eq!(
        definition(&symbols, "fmt").symbol.as_deref(),
        Some("reef-tags . . . `lib.rs`/Foo#fmt().")
    );
    assert_eq!(
        definition(&symbols, "name").symbol.as_deref(),
        Some("reef-tags . . . `lib.rs`/Run#name().")
    );
    let helpers: Vec<_> = symbols
        .occurrences
        .iter()
        .filter(|o| o.role == Role::Definition && o.name == "helper")
        .map(|o| o.symbol.clone().unwrap())
        .collect();
    assert_eq!(
        helpers,
        [
            "reef-tags . . . `lib.rs`/inner/helper().",
            "reef-tags . . . `lib.rs`/helper().",
        ]
    );
    let calls = references(&symbols, "helper");
    assert_eq!(calls.len(), 2, "a call and a scoped call");
    assert_eq!(enclosing_name(&symbols, calls[0]).as_deref(), Some("bar"));
    assert_eq!(references(&symbols, "twice").len(), 1);
    assert_eq!(references(&symbols, "write").len(), 1);
    assert_eq!(references(&symbols, "Display").len(), 1, "the impl's trait");
    assert_eq!(references(&symbols, "max").len(), 1, "a method call");
}

#[test]
fn ranges_and_digests() {
    use reef_symbols::{Language, Limits};
    let text = "def café():\n    return \"é\" + go()\n\ndef other():\n    pass\n";
    let symbols = reef_symbols::extract(
        Language::Python,
        "m.py",
        text.as_bytes(),
        &Limits::default(),
    )
    .unwrap();
    let cafe = definition(&symbols, "café");
    // `café` is five bytes: columns are UTF-8 bytes.
    assert_eq!((cafe.range.start_col, cafe.range.end_col), (4, 9));
    let go = &references(&symbols, "go")[0];
    assert_eq!(
        (go.range.start_col, go.range.end_col),
        (18, 20),
        "after a two-byte `é`"
    );
    assert!(go.digest.is_none() && go.definition_range.is_none());

    let digest = |text: &str, name: &str| {
        let s = reef_symbols::extract(
            Language::Python,
            "m.py",
            text.as_bytes(),
            &Limits::default(),
        )
        .unwrap();
        definition(&s, name).digest.unwrap()
    };
    let changed = text.replace("go()", "went()");
    assert_ne!(
        digest(text, "café"),
        digest(&changed, "café"),
        "its body changed"
    );
    assert_eq!(
        digest(text, "other"),
        digest(&changed, "other"),
        "another body changed"
    );
    let moved = format!("\n\n{text}");
    assert_eq!(
        digest(text, "other"),
        digest(&moved, "other"),
        "position is not content"
    );

    // Innermost first: a reference in a nested function names it, not the outer one.
    let nested = "def a():\n    def b():\n        c()\n    d()\n";
    let s = reef_symbols::extract(
        Language::Python,
        "m.py",
        nested.as_bytes(),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(
        enclosing_name(&s, references(&s, "c")[0]).as_deref(),
        Some("b")
    );
    assert_eq!(
        enclosing_name(&s, references(&s, "d")[0]).as_deref(),
        Some("a")
    );
    assert_eq!(
        enclosing_name(&s, definition(&s, "b")).as_deref(),
        Some("a")
    );
    assert_eq!(definition(&s, "a").enclosing, None);
}

#[test]
fn skips() {
    use reef_symbols::{Language, Limits, Skip};
    use std::time::Duration;
    let limits = Limits::default();
    let mut binary = b"def f():\n    pass\n".to_vec();
    binary.push(0);
    assert_eq!(
        reef_symbols::extract(Language::Go, "x.go", &binary, &limits),
        Err(Skip::Binary)
    );
    let large = vec![b' '; (1 << 20) + 1];
    assert_eq!(
        reef_symbols::extract(Language::Python, "x.py", &large, &limits),
        Err(Skip::TooLarge)
    );
    let calls = "f()\n".repeat(11);
    let few = Limits {
        max_occurrences: 10,
        ..limits
    };
    assert_eq!(
        reef_symbols::extract(Language::Python, "x.py", calls.as_bytes(), &few),
        Err(Skip::TooManyOccurrences)
    );
    let no_time = Limits {
        parse_timeout: Duration::ZERO,
        ..limits
    };
    assert_eq!(
        reef_symbols::extract(Language::Python, "x.py", calls.as_bytes(), &no_time),
        Err(Skip::ParseTimeout)
    );
    // A syntax error keeps the rest of the file's tags.
    let broken = "def good():\n    pass\n\ndef bad(:\n\nclass Fine:\n    pass\n";
    let s = reef_symbols::extract(Language::Python, "x.py", broken.as_bytes(), &limits).unwrap();
    assert!(s.has_errors);
    definition(&s, "good");
    definition(&s, "Fine");
}

/// Pathological inputs finish within the default parse budget.
#[test]
fn hostile_inputs_finish() {
    use reef_symbols::{Language, Limits};
    let limits = Limits::default();
    let nested = format!("const x = {}1{};\n", "[".repeat(20_000), "]".repeat(20_000));
    let started = std::time::Instant::now();
    let _ = reef_symbols::extract(Language::TypeScript, "x.ts", nested.as_bytes(), &limits);
    let line = format!("var a = [{}];", "f(1),".repeat(200_000));
    let line = &line[..line.len().min((1 << 20) - 16)];
    let _ = reef_symbols::extract(Language::JavaScript, "x.js", line.as_bytes(), &limits);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}
