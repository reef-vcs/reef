//! R3.07's test 10: every policy and input under `tests/corpus/` evaluates
//! to its expected JSON; the documentation's policies and the starter load;
//! the JSON Schemas validate the corpus.
//!
//! An input `<policy>[-<case>].input.json` is evaluated against
//! `<policy>.toml`, and compared with `<policy>[-<case>].expected.json`.
//! `REEF_POLICY_BLESS=1` rewrites the expected files.

#![allow(missing_docs)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::{Path, PathBuf};

use reef_policy::{Case, Evaluation, Policy};

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

fn cases() -> Vec<(String, PathBuf, PathBuf)> {
    let mut cases = Vec::new();
    for entry in std::fs::read_dir(corpus()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let Some(case) = name.strip_suffix(".input.json") else {
            continue;
        };
        let policy = case.split('-').next().unwrap();
        cases.push((
            case.to_owned(),
            corpus().join(format!("{policy}.toml")),
            path,
        ));
    }
    cases.sort();
    cases
}

fn outcome(policy: &Path, input: &Path) -> serde_json::Value {
    let bytes = std::fs::read(policy).unwrap();
    match Case::from_json(&std::fs::read(input).unwrap()).unwrap() {
        Case::Change(input) => serde_json::to_value(Evaluation::of(Some(&bytes), &input)).unwrap(),
        Case::Conflict(conflict) => match Policy::load(&bytes).unwrap().conflict_tier(&conflict) {
            Ok(decision) => serde_json::to_value(decision).unwrap(),
            Err(error) => serde_json::json!({ "error": error }),
        },
    }
}

#[test]
fn the_corpus_evaluates_to_its_expected_json() {
    let bless = std::env::var_os("REEF_POLICY_BLESS").is_some();
    let cases = cases();
    assert!(cases.len() >= 6, "{cases:?}");
    for (case, policy, input) in cases {
        let got = outcome(&policy, &input);
        let expected = corpus().join(format!("{case}.expected.json"));
        if bless {
            std::fs::write(
                &expected,
                serde_json::to_string_pretty(&got).unwrap() + "\n",
            )
            .unwrap();
            continue;
        }
        let want: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&expected).unwrap_or_else(|_| {
                panic!(
                    "{} is missing; run with REEF_POLICY_BLESS=1",
                    expected.display()
                )
            }))
            .unwrap();
        assert_eq!(got, want, "{case}");
    }
}

fn validator(schema: &serde_json::Value) -> jsonschema::Validator {
    jsonschema::validator_for(schema).unwrap()
}

#[test]
fn the_schemas_validate_the_corpus() {
    let input = validator(&reef_policy::schema::input());
    let evaluation = validator(&reef_policy::schema::evaluation());
    let decision = validator(&reef_policy::schema::conflict_decision());
    let file = validator(&reef_policy::schema::file());
    for (case, policy, path) in cases() {
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let got = outcome(&policy, &path);
        if json.get("conflict").is_some() {
            if got.get("error").is_none() {
                assert!(decision.is_valid(&got), "{case}");
            }
        } else {
            let errors: Vec<String> = input.iter_errors(&json).map(|e| e.to_string()).collect();
            assert!(errors.is_empty(), "{case}: {errors:?}");
            let errors: Vec<String> = evaluation
                .iter_errors(&got)
                .map(|e| e.to_string())
                .collect();
            assert!(errors.is_empty(), "{case}: {errors:?}");
        }
        let text = std::fs::read_to_string(&policy).unwrap();
        let as_json = serde_json::to_value(toml::from_str::<toml::Value>(&text).unwrap()).unwrap();
        if Policy::load(text.as_bytes()).is_ok() {
            let errors: Vec<String> = file.iter_errors(&as_json).map(|e| e.to_string()).collect();
            assert!(errors.is_empty(), "{}: {errors:?}", policy.display());
        }
    }
    // The file's schema is closed, as the loader is.
    assert!(
        !file.is_valid(
            &serde_json::json!({"version": 1, "owners": {"identities": ["a"]}, "nope": 1})
        )
    );
}

#[test]
fn the_starter_and_the_documentations_policies_load() {
    Policy::load(reef_policy::starter("alice").as_bytes()).unwrap();
    let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/policy.md");
    let text = std::fs::read_to_string(&docs).unwrap();
    let mut blocks = 0;
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.trim() != "```toml policy" {
            continue;
        }
        let block: Vec<&str> = lines
            .by_ref()
            .take_while(|line| line.trim() != "```")
            .collect();
        let block = block.join("\n");
        if let Err(errors) = Policy::load(block.as_bytes()) {
            panic!("a policy in docs/policy.md does not load:\n{block}\n{errors:#?}");
        }
        blocks += 1;
    }
    assert!(blocks >= 5, "{blocks} marked policies in docs/policy.md");
}
