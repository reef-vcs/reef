//! Test 5 of R2.14: SCIP's symbol grammar.

use reef_symbols::symbol::{Descriptor, Suffix, Symbol, format, parse, tags_symbol};

#[test]
fn symbol_grammar() {
    let descriptors = vec![
        Descriptor::new("src", Suffix::Namespace),
        Descriptor::new("my file.py", Suffix::Namespace),
        Descriptor::new("we`ird", Suffix::Type),
        Descriptor::new("run", Suffix::Method(None)),
    ];
    let s = tags_symbol(&descriptors);
    assert_eq!(s, "reef-tags . . . src/`my file.py`/`we``ird`#run().");
    assert_eq!(
        parse(&s).unwrap(),
        Symbol::Global {
            scheme: "reef-tags".into(),
            package: [String::new(), String::new(), String::new()],
            descriptors,
        }
    );

    // SCIP's own examples.
    for example in [
        "scip-typescript npm @example/lib 1.0.0 src/`index.ts`/Foo#bar().",
        "rust-analyzer cargo std 1.0.0 vec/Vec#push().",
        "scip-java maven com.example 1.0 com/example/Main#main(+1).",
        "scip-go gomod github.com/a/b v1.2.3 `github.com/a/b/pkg`/T#[K]",
        "scip-python python lib 0.1 `lib.mod`/f().(x)",
        "lsif-java maven . . java/lang/String#",
        "x . . . Meta:Macro!",
        "spaced  scheme . . . a.",
    ] {
        let parsed = parse(example).unwrap_or_else(|e| panic!("{example}: {e}"));
        let Symbol::Global {
            scheme,
            package,
            descriptors,
        } = &parsed
        else {
            panic!("{example} is global");
        };
        let package = [
            package[0].as_str(),
            package[1].as_str(),
            package[2].as_str(),
        ];
        assert_eq!(format(scheme, package, descriptors), example, "round trip");
    }
    assert_eq!(parse("local 3").unwrap(), Symbol::Local("3".into()));
    for bad in [
        "",
        "local ",
        "scheme",
        "a b c d",
        "a . . . x",
        "a . . . `x",
        "a . . . x(y",
        "localx . . . a.",
    ] {
        assert!(parse(bad).is_err(), "{bad:?} parses");
    }
}
