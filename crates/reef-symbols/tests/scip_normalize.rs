//! Test 8 of R2.14: SCIP documents normalized to UTF-8 byte columns.

use prost::Message as _;
use reef_symbols::scip::proto::{
    self, Document, MultiLineRange, Occurrence, SingleLineRange, SymbolInformation,
};
use reef_symbols::scip::{Rejection, normalize};
use reef_symbols::{Kind, Language, Limits, Role, flags};

/// `🚀 Woo(x)` and a definition of `Woo` at the start of line 1.
const TEXT: &str = "s = \"🚀\"; Woo(x)\ndef Woo(x):\n    return x\n";
const WOO: &str = "scip-python python lib 0.1 `m.py`/Woo().";

fn reference_at(column: i32) -> Occurrence {
    Occurrence {
        symbol: WOO.into(),
        typed_range: Some(proto::occurrence::TypedRange::SingleLineRange(
            SingleLineRange {
                line: 0,
                start_character: column,
                end_character: column + 3,
            },
        )),
        ..Default::default()
    }
}

fn definition() -> Occurrence {
    Occurrence {
        symbol: WOO.into(),
        symbol_roles: proto::SymbolRole::Definition as i32,
        typed_range: Some(proto::occurrence::TypedRange::SingleLineRange(
            SingleLineRange {
                line: 1,
                start_character: 4,
                end_character: 7,
            },
        )),
        ..Default::default()
    }
}

fn document(encoding: proto::PositionEncoding, occurrences: Vec<Occurrence>) -> Vec<u8> {
    Document {
        relative_path: "m.py".into(),
        position_encoding: encoding as i32,
        occurrences,
        ..Default::default()
    }
    .encode_to_vec()
}

fn run(document: &[u8], tool: &str) -> Result<reef_symbols::FileSymbols, Rejection> {
    let tags = reef_symbols::extract(
        Language::Python,
        "m.py",
        TEXT.as_bytes(),
        &Limits::default(),
    )
    .ok();
    normalize(
        document,
        tool,
        TEXT.as_bytes(),
        tags.as_ref(),
        &Limits::default(),
    )
}

#[test]
#[expect(clippy::too_many_lines, reason = "one case per normalization rule")]
fn scip_normalize() {
    use proto::PositionEncoding as E;
    // `Woo` on line 0: byte 13, UTF-16 unit 11 (the rocket is a surrogate
    // pair), UTF-32 unit 10.
    for (encoding, column, tool) in [
        (E::Utf8CodeUnitOffsetFromLineStart, 13, "any"),
        (E::Utf16CodeUnitOffsetFromLineStart, 11, "any"),
        (E::Utf32CodeUnitOffsetFromLineStart, 10, "any"),
        (E::UnspecifiedPositionEncoding, 11, "scip-typescript"),
        (E::UnspecifiedPositionEncoding, 11, "scip-python"),
        (E::UnspecifiedPositionEncoding, 13, "scip-go"),
    ] {
        let s = run(&document(encoding, vec![reference_at(column)]), tool).unwrap();
        let r = s.occurrences[0].range;
        assert_eq!(
            (r.start_line, r.start_col, r.end_col),
            (0, 13, 16),
            "{encoding:?} {tool}"
        );
    }

    // The deprecated packed forms, three and four elements.
    #[expect(deprecated, reason = "the packed range is what this reads")]
    let packed = |range: Vec<i32>| Occurrence {
        symbol: WOO.into(),
        range,
        ..Default::default()
    };
    let s = run(
        &document(
            E::Utf8CodeUnitOffsetFromLineStart,
            vec![packed(vec![0, 13, 16]), packed(vec![1, 4, 1, 7])],
        ),
        "x",
    )
    .unwrap();
    assert_eq!(s.occurrences.len(), 2);
    assert_eq!(s.occurrences[1].range.start_line, 1);
    // A typed range wins over the packed one.
    #[expect(deprecated, reason = "the packed range is what this overrides")]
    let both = Occurrence {
        range: vec![1, 0, 1, 1],
        ..reference_at(13)
    };
    let s = run(
        &document(E::Utf8CodeUnitOffsetFromLineStart, vec![both]),
        "x",
    )
    .unwrap();
    assert_eq!(s.occurrences[0].range.start_line, 0);
    let s = run(
        &document(
            E::Utf8CodeUnitOffsetFromLineStart,
            vec![Occurrence {
                typed_range: Some(proto::occurrence::TypedRange::MultiLineRange(
                    MultiLineRange {
                        start_line: 1,
                        start_character: 0,
                        end_line: 2,
                        end_character: 12,
                    },
                )),
                ..reference_at(0)
            }],
        ),
        "x",
    )
    .unwrap();
    assert_eq!(s.occurrences[0].range.end_line, 2);

    // Locals are dropped; kind from SymbolInformation, else the descriptor.
    let mut doc = Document {
        relative_path: "m.py".into(),
        position_encoding: E::Utf8CodeUnitOffsetFromLineStart as i32,
        occurrences: vec![
            reference_at(13),
            definition(),
            Occurrence {
                symbol: "local 4".into(),
                ..reference_at(13)
            },
        ],
        ..Default::default()
    };
    let s = run(&doc.encode_to_vec(), "x").unwrap();
    assert_eq!(s.occurrences.len(), 2, "the local is dropped");
    let def = s
        .occurrences
        .iter()
        .find(|o| o.role == Role::Definition)
        .unwrap();
    assert_eq!(def.kind, Kind::Function, "from the `().` suffix");
    assert_eq!(def.name, "Woo");
    // No enclosing range: the tags definition with the same name range gives it.
    let range = def.definition_range.expect("filled from tags");
    assert_eq!((range.start_line, range.end_line), (1, 2));
    assert!(def.digest.is_some());
    doc.symbols.push(SymbolInformation {
        symbol: WOO.into(),
        kind: proto::symbol_information::Kind::Method as i32,
        display_name: "Woo!".into(),
        ..Default::default()
    });
    let s = run(&doc.encode_to_vec(), "x").unwrap();
    let def = s
        .occurrences
        .iter()
        .find(|o| o.role == Role::Definition)
        .unwrap();
    assert_eq!((def.kind, def.name.as_str()), (Kind::Method, "Woo!"));

    // Roles beyond Definition are flags.
    let roles = Occurrence {
        symbol_roles: proto::SymbolRole::Import as i32 | proto::SymbolRole::Test as i32,
        ..reference_at(13)
    };
    let s = run(
        &document(E::Utf8CodeUnitOffsetFromLineStart, vec![roles]),
        "x",
    )
    .unwrap();
    assert_eq!(s.occurrences[0].flags, flags::IMPORT | flags::TEST);
    assert_eq!(s.occurrences[0].role, Role::Reference);

    // Rejections.
    let out = run(
        &document(E::Utf8CodeUnitOffsetFromLineStart, vec![reference_at(40)]),
        "x",
    );
    assert_eq!(out, Err(Rejection::RangeOutOfBounds));
    let inside_pair = run(
        &document(E::Utf16CodeUnitOffsetFromLineStart, vec![reference_at(6)]),
        "x",
    );
    assert_eq!(
        inside_pair,
        Err(Rejection::RangeOutOfBounds),
        "a column inside a surrogate pair"
    );
    let far = Occurrence {
        typed_range: Some(proto::occurrence::TypedRange::SingleLineRange(
            SingleLineRange {
                line: 9,
                start_character: 0,
                end_character: 1,
            },
        )),
        ..reference_at(0)
    };
    assert_eq!(
        run(
            &document(E::Utf8CodeUnitOffsetFromLineStart, vec![far]),
            "x"
        ),
        Err(Rejection::RangeOutOfBounds)
    );
    assert_eq!(
        run(b"\xff\xff\xff garbage", "x"),
        Err(Rejection::InvalidDocument)
    );
    let bad_symbol = Occurrence {
        symbol: "not a symbol".into(),
        ..reference_at(13)
    };
    assert_eq!(
        run(
            &document(E::Utf8CodeUnitOffsetFromLineStart, vec![bad_symbol]),
            "x"
        ),
        Err(Rejection::InvalidDocument)
    );
    let many = vec![reference_at(13); 11];
    let few = Limits {
        max_occurrences: 10,
        ..Limits::default()
    };
    assert_eq!(
        normalize(
            &document(E::Utf8CodeUnitOffsetFromLineStart, many),
            "x",
            TEXT.as_bytes(),
            None,
            &few
        ),
        Err(Rejection::TooManyOccurrences)
    );
}

/// Real indexers' output (R2.14's open question 2): `scip-typescript`
/// 0.5.2 and `scip-python` 0.6.8 leave `position_encoding` unset and count
/// UTF-16 code units, which `normalize` reads as the brief's rule says. Each
/// fixture is the indexer's `index.scip` over the source file beside it, a
/// line with a non-BMP character (two UTF-16 units, four bytes) and an
/// accented one.
#[test]
fn real_indexer_output() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/scip");
    for (index, source, language, symbol, start) in [
        (
            "scip-typescript-0.5.2.scip",
            "a.ts",
            Language::TypeScript,
            "scip-typescript npm x 1.0.0 src/`a.ts`/woo().",
            36,
        ),
        (
            "scip-python-0.6.8.scip",
            "a.py",
            Language::Python,
            "scip-python python x 0.1 a/woo.",
            14,
        ),
    ] {
        let index =
            proto::Index::decode(std::fs::read(format!("{dir}/{index}")).unwrap().as_slice())
                .unwrap();
        let tool = index.metadata.as_ref().unwrap().tool_info.as_ref().unwrap();
        let document = &index.documents[0];
        assert_eq!(document.position_encoding, 0, "{}: unspecified", tool.name);
        let bytes = std::fs::read(format!("{dir}/{source}")).unwrap();
        let tags = reef_symbols::extract(language, source, &bytes, &Limits::default()).ok();
        let symbols = normalize(
            &document.encode_to_vec(),
            &tool.name,
            &bytes,
            tags.as_ref(),
            &Limits::default(),
        )
        .unwrap();
        let woo = symbols
            .occurrences
            .iter()
            .find(|o| o.symbol.as_deref() == Some(symbol) && o.role == Role::Definition)
            .unwrap_or_else(|| panic!("{}: no definition of woo", tool.name));
        assert_eq!(
            (woo.range.start_line, woo.range.start_col, woo.range.end_col),
            (0, start, start + 3),
            "{}: UTF-16 columns read as bytes",
            tool.name
        );
        let text = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(&text[start as usize..start as usize + 3], "woo");
    }
}
