//! Test 9 of R2.14: seeded random and mutated inputs through every grammar
//! and through SCIP normalization return without panicking.

mod common;

use prost::Message as _;
use rand::{RngExt as _, SeedableRng as _};
use rand_chacha::ChaCha8Rng;
use reef_symbols::{Language, Limits};

const FIXTURES: [&str; 7] = [
    "parser.py",
    "shapes.ts",
    "view.tsx",
    "app.js",
    "pkg/store/store.go",
    "pkg/store/close.go",
    "lib.rs",
];

fn mutate(rng: &mut ChaCha8Rng, bytes: &mut Vec<u8>) {
    for _ in 0..rng.random_range(1..16) {
        if bytes.is_empty() {
            bytes.push(rng.random());
            continue;
        }
        let at = rng.random_range(0..bytes.len());
        match rng.random_range(0..4) {
            0 => bytes[at] = rng.random(),
            1 => {
                bytes.remove(at);
            }
            2 => bytes.insert(at, b"{}()[]<>\"'`/#:\n"[rng.random_range(0..15)]),
            _ => {
                let end = rng
                    .random_range(at..bytes.len().min(at + 64) + 1)
                    .min(bytes.len());
                let chunk = bytes[at..end].to_vec();
                bytes.splice(at..at, chunk);
            }
        }
    }
}

#[test]
fn no_panics() {
    let mut rng = ChaCha8Rng::seed_from_u64(0x2014);
    let limits = Limits::default();
    let fixtures: Vec<Vec<u8>> = FIXTURES.iter().map(|f| common::fixture(f)).collect();
    for round in 0..2_000 {
        let language = Language::ALL[round % Language::ALL.len()];
        let mut bytes = if round % 10 == 0 {
            (0..rng.random_range(0..512))
                .map(|_| rng.random())
                .collect()
        } else {
            fixtures[rng.random_range(0..fixtures.len())].clone()
        };
        mutate(&mut rng, &mut bytes);
        let tags = reef_symbols::extract(language, "f.x", &bytes, &limits).ok();

        // A document from a fixture's own tags, mutated on the wire.
        let document = reef_symbols::scip::proto::Document {
            relative_path: "f.x".into(),
            position_encoding: rng.random_range(0..4),
            occurrences: tags
                .iter()
                .flat_map(|t| &t.occurrences)
                .take(64)
                .map(|o| reef_symbols::scip::proto::Occurrence {
                    symbol: o.symbol.clone().unwrap_or_else(|| "local 1".into()),
                    symbol_roles: rng.random_range(0..128),
                    #[expect(deprecated, reason = "the packed range is fuzzed too")]
                    range: vec![
                        i32::try_from(o.range.start_line).unwrap(),
                        i32::try_from(o.range.start_col).unwrap() + rng.random_range(-2..3),
                        i32::try_from(o.range.end_col).unwrap(),
                    ],
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let mut encoded = document.encode_to_vec();
        if round % 3 == 0 {
            mutate(&mut rng, &mut encoded);
        }
        let _ = reef_symbols::scip::normalize(
            &encoded,
            "scip-typescript",
            &bytes,
            tags.as_ref(),
            &limits,
        );
    }
}
