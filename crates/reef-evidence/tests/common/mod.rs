//! The format's test fixtures (R3.01): a key from a fixed seed, a valid
//! envelope, and one envelope per refusal of test 3, all made here rather
//! than by `openssl` so they build anywhere. `tests/fixtures/` holds them as
//! files with `refusals.json` naming each one's check, for the daemon's API
//! tests; `fixtures_are_current` fails when the files drift from this code,
//! and `REEF_UPDATE_FIXTURES=1` rewrites them.

#![allow(
    dead_code,
    unreachable_pub,
    reason = "each test binary uses part of the helpers"
)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    missing_docs,
    reason = "test helpers: a panic is the failure report"
)]

use std::collections::BTreeMap;
use std::time::{Duration, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reef_evidence::{Artifact, Claim, Envelope, Outcome, PAYLOAD_TYPE, SigningKey, TreeDigest};
use serde_json::{Map, Value, json};

/// The fixtures' key's seed: the bytes 0 to 31.
pub const SEED: [u8; 32] = {
    let mut seed = [0; 32];
    let mut i = 0;
    while i < 32 {
        #[expect(clippy::cast_possible_truncation, reason = "i < 32")]
        {
            seed[i] = i as u8;
        }
        i += 1;
    }
    seed
};

/// That key's id, as OpenSSL 3.6.3 derives it from the same seed:
/// `openssl pkey -in v1.pem -pubout -outform DER | shasum -a 256`, with
/// [`PKCS8_V1_PEM`] as `v1.pem`.
pub const KEY_ID: &str = "a050837d85070582ccf7394b0988847cc312cb88259b894899f6f239cf1791a5";

/// The seed's PKCS#8 v1 key as OpenSSL 3 writes it (`openssl pkey` of the
/// DER), 48 bytes with no public key.
pub const PKCS8_V1_PEM: &str = "-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIAABAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhscHR4f
-----END PRIVATE KEY-----
";

/// A second key, for envelopes signed by someone else.
pub const OTHER_SEED: [u8; 32] = [7; 32];

/// The empty tree's git id.
pub const TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

pub fn key() -> SigningKey {
    SigningKey::from_seed(&SEED).unwrap()
}

pub fn other_key() -> SigningKey {
    SigningKey::from_seed(&OTHER_SEED).unwrap()
}

pub fn tree() -> TreeDigest {
    TreeDigest::git(TREE).unwrap()
}

/// The fixtures' claim: a passing test run with one artifact.
pub fn claim() -> Claim {
    let at = |seconds| UNIX_EPOCH + Duration::from_secs(seconds);
    Claim {
        kind: "test".into(),
        result: Outcome::Pass,
        target: Some("crates/reef-lib:jj_writes".into()),
        started_on: at(1_793_872_800),
        finished_on: at(1_793_872_872),
        artifacts: vec![Artifact {
            name: "test.log".into(),
            sha256: reef_evidence::sha256_hex(b"test log\n"),
            uri: None,
            media_type: Some("text/plain".into()),
        }],
        annotations: Map::from_iter([("runner".to_owned(), json!("ci-7"))]),
    }
}

/// The claim's statement as a JSON value, to change before signing.
pub fn statement_value() -> Value {
    serde_json::to_value(claim().statement(&tree()).unwrap()).unwrap()
}

/// `payload` signed by the fixtures' key, as envelope bytes.
pub fn signed(payload: &[u8]) -> Vec<u8> {
    key().sign(PAYLOAD_TYPE, payload).to_bytes()
}

/// The statement after `change`, signed.
pub fn signed_with(change: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut value = statement_value();
    change(&mut value);
    signed(&serde_json::to_vec(&value).unwrap())
}

/// The valid envelope's JSON as a value, to change after signing.
pub fn envelope_value() -> Value {
    serde_json::from_slice(&signed(&claim().payload(&tree()).unwrap())).unwrap()
}

/// What a fixture is refused with: an `INVALID_EVIDENCE` check (and
/// field), or `SIGNATURE_INVALID`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expect {
    Check(&'static str, Option<&'static str>),
    SignatureInvalid,
}

/// A refusal fixture.
pub struct Fixture {
    pub file: &'static str,
    pub envelope: Vec<u8>,
    /// The DER `SubjectPublicKeyInfo` sent with it.
    pub public_key: Vec<u8>,
    pub expect: Expect,
}

/// The public key sent with every fixture but `public-key`.
pub fn spki() -> Vec<u8> {
    key().public_key().spki().to_vec()
}

/// A predicate field set to `value`.
fn predicate(field: &'static str, value: Value) -> impl FnOnce(&mut Value) {
    move |statement| statement["predicate"][field] = value
}

/// `n` nested arrays around 0.
fn nested(n: usize) -> Value {
    (0..n).fold(json!(0), |inner, _| Value::Array(vec![inner]))
}

/// Every refusal of test 3, one per file.
#[expect(clippy::too_many_lines, reason = "a table")]
pub fn refusals() -> Vec<Fixture> {
    use Expect::{Check, SignatureInvalid};
    let payload = claim().payload(&tree()).unwrap();
    let payload_text = String::from_utf8(payload.clone()).unwrap();
    let envelope = |change: &dyn Fn(&mut Value)| {
        let mut value = envelope_value();
        change(&mut value);
        serde_json::to_vec(&value).unwrap()
    };
    let valid_envelope = String::from_utf8(signed(&payload)).unwrap();
    let mut fixtures = vec![
        (
            "size",
            signed_with(|s| {
                s["predicate"]["annotations"]["padding"] = json!("x".repeat(48 * 1024));
            }),
            Check("size", None),
        ),
        (
            "not-json",
            b"{\"payload\": ".to_vec(),
            Check("envelope", None),
        ),
        (
            "depth-envelope",
            envelope(&|e| e["extra"] = nested(32)),
            Check("envelope", None),
        ),
        (
            "depth-payload",
            signed_with(predicate("annotations", json!({"deep": nested(30)}))),
            Check("predicate", Some("annotations")),
        ),
        (
            "duplicate-envelope",
            valid_envelope
                .replacen(
                    "\"payloadType\"",
                    "\"payloadType\":\"text/plain\",\"payloadType\"",
                    1,
                )
                .into_bytes(),
            Check("envelope", None),
        ),
        (
            "duplicate-statement",
            signed(
                payload_text
                    .replacen("{\"_type\"", "{\"_type\":\"x\",\"_type\"", 1)
                    .as_bytes(),
            ),
            Check("statement", None),
        ),
        (
            "duplicate-predicate",
            signed(
                payload_text
                    .replacen("{\"runner\"", "{\"runner\":\"ci-1\",\"runner\"", 1)
                    .as_bytes(),
            ),
            Check("predicate", Some("annotations")),
        ),
        (
            "payload-type",
            key().sign("application/json", &payload).to_bytes(),
            Check("payload_type", None),
        ),
        (
            "no-signatures",
            envelope(&|e| e["signatures"] = json!([])),
            Check("signatures", None),
        ),
        (
            "two-signatures",
            envelope(&|e| {
                let second = e["signatures"][0].clone();
                e["signatures"].as_array_mut().unwrap().push(second);
            }),
            Check("signatures", None),
        ),
        (
            "key-id",
            envelope(&|e| {
                e["signatures"][0]["keyid"] = json!(other_key().public_key().id().as_str());
            }),
            Check("key_id", None),
        ),
        (
            "bad-signature",
            envelope(&|e| {
                let mut sig = STANDARD
                    .decode(e["signatures"][0]["sig"].as_str().unwrap())
                    .unwrap();
                sig[0] ^= 1;
                e["signatures"][0]["sig"] = json!(STANDARD.encode(sig));
            }),
            SignatureInvalid,
        ),
        (
            "not-statement",
            signed_with(|s| s["_type"] = json!("https://in-toto.io/Statement/v0.1")),
            Check("statement", None),
        ),
        (
            "no-subject",
            signed_with(|s| s["subject"] = json!([])),
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
            "sha256-subject",
            signed_with(|s| s["subject"][0]["digest"] = json!({"sha256": "0".repeat(64)})),
            Check("subject", None),
        ),
        (
            "uppercase-tree",
            signed_with(|s| s["subject"][0]["digest"]["gitTree"] = json!(TREE.to_uppercase())),
            Check("subject", None),
        ),
        (
            "short-tree",
            signed_with(|s| s["subject"][0]["digest"]["gitTree"] = json!(&TREE[..39])),
            Check("subject", None),
        ),
        (
            "subject-no-name",
            signed_with(|s| {
                s["subject"][0].as_object_mut().unwrap().remove("name");
            }),
            Check("subject", None),
        ),
        (
            "subject-other-name",
            signed_with(|s| s["subject"][0]["name"] = json!("not-a-tree")),
            Check("subject", None),
        ),
        (
            "subject-uri-number",
            signed_with(|s| s["subject"][0]["uri"] = json!(7)),
            Check("subject", None),
        ),
        (
            "subject-annotations-string",
            signed_with(|s| s["subject"][0]["annotations"] = json!("x")),
            Check("subject", None),
        ),
        (
            "predicate-type",
            signed_with(|s| s["predicateType"] = json!("https://slsa.dev/provenance/v1")),
            Check("predicate_type", None),
        ),
        (
            "kind-uppercase",
            signed_with(predicate("kind", json!("Test"))),
            Check("predicate", Some("kind")),
        ),
        (
            "result-passed",
            signed_with(predicate("result", json!("passed"))),
            Check("predicate", Some("result")),
        ),
        (
            "finished-before-started",
            signed_with(predicate("finishedOn", json!("2026-11-05T09:59:59Z"))),
            Check("predicate", Some("finishedOn")),
        ),
        (
            "time-without-z",
            signed_with(predicate("startedOn", json!("2026-11-05T10:00:00+00:00"))),
            Check("predicate", Some("startedOn")),
        ),
        (
            "time-offset-before-z",
            signed_with(predicate("startedOn", json!("2026-11-05T10:00:00.0+0500Z"))),
            Check("predicate", Some("startedOn")),
        ),
        (
            "time-empty-fraction",
            signed_with(predicate("startedOn", json!("2026-11-05T10:00:00.Z"))),
            Check("predicate", Some("startedOn")),
        ),
        (
            "time-leap-second",
            signed_with(predicate("finishedOn", json!("2026-11-05T10:01:60Z"))),
            Check("predicate", Some("finishedOn")),
        ),
        (
            "artifacts-65",
            signed_with(|s| {
                let one = s["predicate"]["artifacts"][0].clone();
                s["predicate"]["artifacts"] = Value::Array(vec![one; 65]);
            }),
            Check("predicate", Some("artifacts")),
        ),
        (
            "artifact-no-sha256",
            signed_with(|s| {
                s["predicate"]["artifacts"][0]["digest"] = json!({"sha512": "0".repeat(128)});
            }),
            Check("predicate", Some("artifacts")),
        ),
        (
            "target-513",
            signed_with(predicate("target", json!("t".repeat(513)))),
            Check("predicate", Some("target")),
        ),
        (
            "target-control",
            signed_with(predicate("target", json!("a\u{7}b"))),
            Check("predicate", Some("target")),
        ),
        (
            "annotations-not-object",
            signed_with(predicate("annotations", json!(["ci-7"]))),
            Check("predicate", Some("annotations")),
        ),
    ]
    .into_iter()
    .map(|(file, envelope, expect)| Fixture {
        file,
        envelope,
        public_key: spki(),
        expect,
    })
    .collect::<Vec<_>>();
    // The valid envelope with a key that is not ed25519's: a small-order
    // point.
    let mut small_order = reef_evidence::keys::SPKI_PREFIX.to_vec();
    small_order.extend_from_slice(&reef_evidence::keys::SMALL_ORDER[1]);
    fixtures.push(Fixture {
        file: "public-key",
        envelope: valid_envelope.into_bytes(),
        public_key: small_order,
        expect: Check("public_key", None),
    });
    fixtures
}

/// `refusals.json`: each fixture's file, public key (base64 DER), check
/// and field, or `signature_invalid`.
pub fn manifest() -> Value {
    let refusals: Vec<Value> = refusals()
        .iter()
        .map(|fixture| {
            let (check, field) = match &fixture.expect {
                Expect::Check(check, field) => (Value::from(*check), json!(field)),
                Expect::SignatureInvalid => (Value::Null, Value::Null),
            };
            json!({
                "file": format!("{}.json", fixture.file),
                "public_key": STANDARD.encode(&fixture.public_key),
                "check": check,
                "field": field,
                "signature_invalid": fixture.expect == Expect::SignatureInvalid,
            })
        })
        .collect();
    json!({
        "valid": {"file": "valid.json", "public_key": STANDARD.encode(spki()), "key_id": KEY_ID},
        "refusals": refusals,
    })
}

/// Every fixture file and its bytes.
pub fn files() -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    files.insert(
        "valid.json".to_owned(),
        signed(&claim().payload(&tree()).unwrap()),
    );
    files.insert(
        "key.pub.pem".to_owned(),
        key().public_key().to_pem().into_bytes(),
    );
    files.insert("key-v1.pem".to_owned(), PKCS8_V1_PEM.as_bytes().to_vec());
    for fixture in refusals() {
        files.insert(format!("{}.json", fixture.file), fixture.envelope);
    }
    let mut manifest = serde_json::to_vec_pretty(&manifest()).unwrap();
    manifest.push(b'\n');
    files.insert("refusals.json".to_owned(), manifest);
    files
}

/// The envelope `bytes` as a typed value.
pub fn parse(bytes: &[u8]) -> Envelope {
    serde_json::from_slice(bytes).unwrap()
}
