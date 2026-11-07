//! Provenance records (R3.04 tests 1 to 3): signing and checking, one
//! refusal per check, and the context digest.
//!
//! The fixtures under `tests/fixtures/provenance/` are what this file
//! makes, from fixed seeds, so they build anywhere: a valid record, its
//! key's public half, and one envelope per refusal, with `refusals.json`
//! naming each one's check. `fixtures_are_current` fails when the files
//! drift, and `REEF_UPDATE_FIXTURES=1` rewrites them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests: a panic is the failure report"
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use reef_evidence::provenance::{
    Context, PROVENANCE_PREDICATE_TYPE, Record, check, context_digest, sign,
};
use reef_evidence::session::{Authority, IssuerKind, check_certificate};
use reef_evidence::{Invalid, PAYLOAD_TYPE, SigningKey, Verified, pae};
use serde_json::{Value, json};

/// The held key's seed.
const KEY_SEED: [u8; 32] = [0xc3; 32];
/// Someone else's key.
const OTHER_SEED: [u8; 32] = [7; 32];

fn key() -> SigningKey {
    SigningKey::from_seed(&KEY_SEED).unwrap()
}

fn other_key() -> SigningKey {
    SigningKey::from_seed(&OTHER_SEED).unwrap()
}

fn spki() -> Vec<u8> {
    key().public_key().spki().to_vec()
}

fn context() -> Context {
    Context {
        model: Some("example-provider/model-x-2026-10".into()),
        harness: Some("example-harness/2.1.0".into()),
        task: Some("https://tracker.example/T-123".into()),
        instruction_sha256: Some(reef_evidence::sha256_hex(b"add a parser test")),
        client_session: Some("c0ffee00-1234".into()),
        // 2026-11-06T10:11:58.002Z.
        declared_on_ms: Some(1_793_959_918_002),
    }
}

fn record() -> Record {
    Record {
        commit_id: "5d41402abc4b2a76b9719d911017c592ae2b6a3e".into(),
        change_id: "8f3a0c2d91e4b6751c2d0e9f4a7b3c18".into(),
        tree_id: Some("4b825dc642cb6eb9a060e54bf8d69288fbee4904".into()),
        workspace: "0123456789abcdef".into(),
        kind: "watcher".into(),
        session: "ses_41c9e07ab2d35f18".into(),
        sequence: 42,
        agent: Some("builder-3".into()),
        // 2026-11-06T10:12:03.418Z.
        recorded_on_ms: 1_793_959_923_418,
        context: context(),
    }
}

fn envelope() -> Vec<u8> {
    sign(&key(), &record()).unwrap().to_bytes()
}

/// The valid record's statement as a JSON value, to change.
fn statement() -> Value {
    serde_json::from_slice(&record().payload().unwrap()).unwrap()
}

/// `payload` signed by the held key.
fn signed(payload: &[u8]) -> Vec<u8> {
    key().sign(PAYLOAD_TYPE, payload).to_bytes()
}

/// The statement after `change`, signed by the held key.
fn signed_with(change: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut value = statement();
    change(&mut value);
    signed(&serde_json::to_vec(&value).unwrap())
}

fn predicate(field: &'static str, value: Value) -> impl FnOnce(&mut Value) {
    move |statement| statement["predicate"][field] = value
}

fn context_field(field: &'static str, value: Value) -> impl FnOnce(&mut Value) {
    move |statement| statement["predicate"]["context"][field] = value
}

fn nested(n: usize) -> Value {
    (0..n).fold(json!(0), |inner, _| Value::Array(vec![inner]))
}

/// What a fixture is refused with.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Expect {
    /// [`Invalid::check`] and [`Invalid::field`].
    Check(&'static str, Option<&'static str>),
    /// [`Invalid::Signature`].
    Signature,
}

/// Every refusal of test 2, one per file.
#[expect(clippy::too_many_lines, reason = "a table")]
fn refusals() -> Vec<(&'static str, Vec<u8>, Expect)> {
    use Expect::{Check, Signature};
    let payload = record().payload().unwrap();
    let text = String::from_utf8(payload.clone()).unwrap();
    let valid = String::from_utf8(envelope()).unwrap();
    let envelope_with = |change: &dyn Fn(&mut Value)| {
        let mut value: Value = serde_json::from_slice(&envelope()).unwrap();
        change(&mut value);
        serde_json::to_vec(&value).unwrap()
    };
    vec![
        // R3.01's envelope, size and depth refusals.
        (
            "size",
            signed_with(predicate("pad", json!("x".repeat(48 * 1024)))),
            Check("size", None),
        ),
        (
            "not-json",
            b"{\"payload\": ".to_vec(),
            Check("envelope", None),
        ),
        (
            "depth-envelope",
            envelope_with(&|e| e["extra"] = nested(32)),
            Check("envelope", None),
        ),
        (
            "depth-payload",
            signed_with(predicate("pad", nested(31))),
            Check("predicate", Some("pad")),
        ),
        (
            "duplicate-envelope",
            valid
                .replacen(
                    "\"payloadType\"",
                    "\"payloadType\":\"text/plain\",\"payloadType\"",
                    1,
                )
                .into_bytes(),
            Check("envelope", None),
        ),
        (
            "payload-type",
            key().sign("application/json", &payload).to_bytes(),
            Check("payload_type", None),
        ),
        (
            "two-signatures",
            envelope_with(&|e| {
                let second = e["signatures"][0].clone();
                e["signatures"].as_array_mut().unwrap().push(second);
            }),
            Check("signatures", None),
        ),
        // The key, then the signature, before the statement.
        (
            "other-key-id",
            other_key().sign(PAYLOAD_TYPE, &payload).to_bytes(),
            Check("key_id", None),
        ),
        (
            "other-signer",
            {
                let junk = b"not even json";
                let sig = other_key().sign_bytes(&pae(PAYLOAD_TYPE, junk));
                reef_evidence::Envelope::new(
                    PAYLOAD_TYPE,
                    junk,
                    key().public_key().id().as_str(),
                    &sig,
                )
                .to_bytes()
            },
            Signature,
        ),
        // The statement.
        (
            "predicate-type",
            signed_with(|s| s["predicateType"] = json!(reef_evidence::PREDICATE_TYPE)),
            Check("statement", None),
        ),
        (
            "subject-name",
            signed_with(|s| s["subject"][0]["name"] = json!("tree")),
            Check("subject", None),
        ),
        (
            "two-subjects",
            signed_with(|s| {
                let second = s["subject"][0].clone();
                s["subject"].as_array_mut().unwrap().push(second);
            }),
            Check("subject", None),
        ),
        (
            "subject-other-digest",
            signed_with(|s| s["subject"][0]["digest"]["sha256"] = json!("0".repeat(64))),
            Check("subject", None),
        ),
        (
            "subject-short-commit",
            signed_with(|s| s["subject"][0]["digest"]["gitCommit"] = json!("5d41402abc")),
            Check("subject", None),
        ),
        (
            "subject-uppercase-commit",
            signed_with(|s| {
                s["subject"][0]["digest"]["gitCommit"] =
                    json!("5D41402ABC4B2A76B9719D911017C592AE2B6A3E");
            }),
            Check("subject", None),
        ),
        // The predicate.
        (
            "change",
            signed_with(predicate("change", json!("kxyzvqmp"))),
            Check("predicate", Some("change")),
        ),
        (
            "tree",
            signed_with(predicate("tree", json!({"gitTree": "4b825dc6"}))),
            Check("predicate", Some("tree")),
        ),
        (
            "workspace",
            signed_with(predicate("workspace", json!("0123456789ABCDEF"))),
            Check("predicate", Some("workspace")),
        ),
        (
            "kind",
            signed_with(predicate("kind", json!("Watcher"))),
            Check("predicate", Some("kind")),
        ),
        (
            "session",
            signed_with(predicate("session", json!("ses_41c9"))),
            Check("predicate", Some("session")),
        ),
        (
            "sequence-zero",
            signed_with(predicate("sequence", json!(0))),
            Check("predicate", Some("sequence")),
        ),
        (
            "sequence-over",
            signed_with(predicate("sequence", json!((1_u64 << 53) + 1))),
            Check("predicate", Some("sequence")),
        ),
        (
            "recorded-on",
            signed_with(predicate("recordedOn", json!("2026-11-06T10:12:03+00:00"))),
            Check("predicate", Some("recordedOn")),
        ),
        (
            "no-context",
            signed_with(|s| {
                s["predicate"].as_object_mut().unwrap().remove("context");
            }),
            Check("predicate", Some("context")),
        ),
        (
            "instruction-text",
            signed_with(context_field(
                "instruction",
                json!({"sha256": "add a parser test"}),
            )),
            Check("predicate", Some("context")),
        ),
        (
            "instruction-other-digest",
            signed_with(context_field(
                "instruction",
                json!({"sha256": "0".repeat(64), "sha512": "0".repeat(128)}),
            )),
            Check("predicate", Some("context")),
        ),
        (
            "model-oversize",
            signed_with(context_field("model", json!("m".repeat(129)))),
            Check("predicate", Some("context")),
        ),
        (
            "harness-space",
            signed_with(context_field("harness", json!("example harness"))),
            Check("predicate", Some("context")),
        ),
        (
            "task-control",
            signed_with(context_field("task", json!("T-1\u{7}"))),
            Check("predicate", Some("context")),
        ),
        (
            "client-session-empty",
            signed_with(context_field("clientSession", json!(""))),
            Check("predicate", Some("context")),
        ),
        (
            "duplicate-predicate",
            signed(
                text.replacen(
                    "\"kind\":\"watcher\"",
                    "\"kind\":\"edit\",\"kind\":\"watcher\"",
                    1,
                )
                .as_bytes(),
            ),
            Check("predicate", Some("kind")),
        ),
    ]
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/provenance")
}

/// Every fixture file and its bytes.
fn files() -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    files.insert("valid.json".to_owned(), envelope());
    files.insert(
        "key.pub.pem".to_owned(),
        key().public_key().to_pem().into_bytes(),
    );
    let mut manifest = Vec::new();
    for (file, envelope, expect) in refusals() {
        let (check, field) = match &expect {
            Expect::Check(check, field) => (json!(check), json!(field)),
            Expect::Signature => (Value::Null, Value::Null),
        };
        manifest.push(json!({
            "file": format!("{file}.json"),
            "check": check,
            "field": field,
            "signature_invalid": expect == Expect::Signature,
        }));
        files.insert(format!("{file}.json"), envelope);
    }
    let manifest = json!({
        "valid": {
            "file": "valid.json",
            "key": "key.pub.pem",
            "key_id": key().public_key().id().as_str(),
        },
        "refusals": manifest,
    });
    let mut bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    bytes.push(b'\n');
    files.insert("refusals.json".to_owned(), bytes);
    files
}

#[test]
fn fixtures_are_current() {
    let dir = fixtures_dir();
    let update = std::env::var_os("REEF_UPDATE_FIXTURES").is_some();
    if update {
        std::fs::create_dir_all(&dir).unwrap();
    }
    for (name, bytes) in files() {
        let path = dir.join(&name);
        if update {
            std::fs::write(&path, &bytes).unwrap();
            continue;
        }
        let found = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("{name}: {error}; run with REEF_UPDATE_FIXTURES=1"));
        assert!(
            found == bytes,
            "{name} is stale; run with REEF_UPDATE_FIXTURES=1"
        );
    }
}

// 1.
#[test]
fn provenance_sign_and_check() {
    let envelope = envelope();
    let checked = check(&envelope, &spki()).unwrap();
    assert_eq!(checked.record, record());
    // Canonical and deterministic.
    assert_eq!(checked.canonical, envelope);
    assert_eq!(envelope_again(), envelope);
    assert_eq!(checked.id, reef_evidence::sha256_hex(&envelope));
    assert_eq!(checked.key_id(), key().public_key().id());

    // The payload in the module's order, with the subject a gitCommit.
    let text = String::from_utf8(checked.payload.clone()).unwrap();
    assert!(
        text.starts_with(
            "{\"_type\":\"https://in-toto.io/Statement/v1\",\"subject\":[{\"name\":\"commit\",\"digest\":{\"gitCommit\":\"5d41402abc4b2a76b9719d911017c592ae2b6a3e\"}}],\"predicateType\":\"urn:reef-vcs:provenance:v1\",\"predicate\":{\"change\":"
        ),
        "{text}"
    );
    let order = [
        "\"change\"",
        "\"tree\"",
        "\"workspace\"",
        "\"kind\"",
        "\"session\"",
        "\"sequence\"",
        "\"agent\"",
        "\"recordedOn\"",
        "\"context\"",
        "\"model\"",
        "\"harness\"",
        "\"task\"",
        "\"instruction\"",
        "\"clientSession\"",
        "\"declaredOn\"",
    ];
    let positions: Vec<usize> = order.iter().map(|key| text.find(key).unwrap()).collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{text}");

    // Absent optional fields are left out, and an empty context is valid.
    let bare = Record {
        tree_id: None,
        agent: None,
        context: Context::default(),
        ..record()
    };
    let bare_envelope = sign(&key(), &bare).unwrap().to_bytes();
    let checked = check(&bare_envelope, &spki()).unwrap();
    assert_eq!(checked.record, bare);
    let text = String::from_utf8(checked.payload).unwrap();
    assert!(
        !text.contains("\"tree\"") && !text.contains("\"agent\""),
        "{text}"
    );
    assert!(text.contains("\"context\":{}"), "{text}");

    // An unknown predicate field, or context field, is kept and ignored.
    let extended = signed_with(|s| {
        s["predicate"]["later"] = json!({"x": 1});
        s["predicate"]["context"]["laterToo"] = json!("y");
    });
    assert_eq!(check(&extended, &spki()).unwrap().record, record());

    // An unknown kind in the vocabulary's form is accepted and reported.
    let later = signed_with(predicate("kind", json!("rebase-carry")));
    assert_eq!(check(&later, &spki()).unwrap().record.kind, "rebase-carry");

    // A 64-digit (SHA-256) commit and tree are accepted.
    let wide = Record {
        commit_id: "a".repeat(64),
        tree_id: Some("b".repeat(64)),
        ..record()
    };
    assert_eq!(
        check(&sign(&key(), &wide).unwrap().to_bytes(), &spki())
            .unwrap()
            .record,
        wide
    );

    // A record the checker would refuse is never signed.
    let bad = Record {
        sequence: 0,
        ..record()
    };
    assert_eq!(sign(&key(), &bad).unwrap_err().field(), Some("sequence"));
}

fn envelope_again() -> Vec<u8> {
    sign(&key(), &record()).unwrap().to_bytes()
}

// 2.
#[test]
fn provenance_refusals() {
    let key = spki();
    for (name, envelope, expect) in refusals() {
        let error = check(&envelope, &key).expect_err(&format!("{name}: refused"));
        match expect {
            Expect::Check(check, field) => {
                assert_eq!(error.check(), Some(check), "{name}: {error}");
                assert_eq!(error.field(), field, "{name}: {error}");
            }
            Expect::Signature => {
                assert!(
                    matches!(error, Invalid::Signature { .. }),
                    "{name}: {error}"
                );
            }
        }
    }

    // The other predicate types' checkers refuse a record, and this one
    // refuses theirs.
    let envelope = envelope();
    let error = Verified::check(&envelope, &key).unwrap_err();
    assert_eq!(error.check(), Some("predicate_type"), "{error}");
    let authority = Authority {
        key: self::key().public_key().clone(),
        kind: IssuerKind::ReefDaemon,
        revocation: None,
    };
    let error = check_certificate(&envelope, &|_| Some(authority.clone())).unwrap_err();
    assert_eq!(error.check(), Some("statement"), "{error}");
    let evidence = signed_with(|s| s["predicateType"] = json!(reef_evidence::PREDICATE_TYPE));
    assert_eq!(
        check(&evidence, &key).unwrap_err().check(),
        Some("statement")
    );
    assert_eq!(
        statement()["predicateType"],
        json!(PROVENANCE_PREDICATE_TYPE)
    );
}

// 3.
#[test]
fn provenance_context_digest() {
    // The digest of a checked record's context equals the digest of the
    // context it was signed from, whatever order the JSON keys came in.
    let reordered = signed_with(|s| {
        let context = s["predicate"]["context"].as_object().unwrap().clone();
        let mut reversed = serde_json::Map::new();
        for (key, value) in context.into_iter().rev() {
            reversed.insert(key, value);
        }
        s["predicate"]["context"] = Value::Object(reversed);
    });
    let checked = check(&reordered, &spki()).unwrap();
    assert_eq!(
        context_digest(&checked.record.context),
        context_digest(&context())
    );
    // A change to any declared field changes it; declaredOn does not.
    let base = context_digest(&context());
    let variants: [fn(&mut Context); 5] = [
        |c| c.model = Some("other".into()),
        |c| c.harness = None,
        |c| c.task = Some("T-2".into()),
        |c| c.instruction_sha256 = Some(reef_evidence::sha256_hex(b"other")),
        |c| c.client_session = Some("other".into()),
    ];
    for change in variants {
        let mut other = context();
        change(&mut other);
        assert_ne!(context_digest(&other), base);
    }
    let mut later = context();
    later.declared_on_ms = Some(1_793_960_000_000);
    assert_eq!(context_digest(&later), base);
}
