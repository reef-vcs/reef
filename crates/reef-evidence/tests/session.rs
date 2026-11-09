//! Session-key certificates (R3.03 tests 1 to 3): certifying and checking,
//! one refusal per check, and the status at a witness time.
//!
//! The fixtures under `tests/fixtures/session/` are what this file makes,
//! from fixed seeds, so they build anywhere: a valid certificate, its
//! authority's public key, and one envelope per refusal, with
//! `refusals.json` naming each one's check. `fixtures_are_current` fails
//! when the files drift, and `REEF_UPDATE_FIXTURES=1` rewrites them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests: a panic is the failure report"
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reef_evidence::session::{
    Actor, Authority, Binding, Certified, Custody, IdentityKind, IdentityRef, IdentityStatus,
    Issuer, IssuerKind, Presence, Revocation, Revocations, SESSION_KEY_PREDICATE_TYPE, certify,
    check_certificate, status_at,
};
use reef_evidence::{Invalid, KeyId, PAYLOAD_TYPE, SigningKey, pae};
use serde_json::{Value, json};

/// The authority's seed.
const AUTHORITY_SEED: [u8; 32] = [0xa1; 32];
/// The session key's seed.
const SESSION_SEED: [u8; 32] = [0xb2; 32];
/// Someone else's key.
const OTHER_SEED: [u8; 32] = [7; 32];

/// 2026-11-06T10:00:00Z.
const NOT_BEFORE: i64 = 1_793_959_200_000;
/// An hour later.
const NOT_AFTER: i64 = NOT_BEFORE + 3_600_000;

fn authority_key() -> SigningKey {
    SigningKey::from_seed(&AUTHORITY_SEED).unwrap()
}

fn session_key() -> SigningKey {
    SigningKey::from_seed(&SESSION_SEED).unwrap()
}

fn other_key() -> SigningKey {
    SigningKey::from_seed(&OTHER_SEED).unwrap()
}

fn authority() -> Authority {
    Authority {
        key: authority_key().public_key().clone(),
        kind: IssuerKind::ReefDaemon,
        revocation: None,
    }
}

/// The authorities a verifier of the fixtures knows: the one.
fn known(key_id: &KeyId) -> Option<Authority> {
    (key_id == authority_key().public_key().id()).then(authority)
}

fn binding() -> Binding {
    Binding {
        public_key: session_key().public_key().clone(),
        identity: IdentityRef {
            id: "idn_8f3a0c2d91e4b675".into(),
            name: "alice".into(),
            kind: IdentityKind::Person,
        },
        actor: Actor::Agent,
        agent: Some("builder-3".into()),
        session: "ses_41c9e07ab2d35f18".into(),
        presence: Presence::None,
        custody: Custody::Client,
        not_before_ms: NOT_BEFORE,
        not_after_ms: NOT_AFTER,
        issuer: Issuer {
            kind: IssuerKind::ReefDaemon,
            key_id: authority_key().public_key().id().clone(),
            host: "mbp.local".into(),
        },
    }
}

fn certificate() -> Vec<u8> {
    certify(&authority_key(), &binding()).unwrap().to_bytes()
}

/// The valid certificate's statement as a JSON value, to change.
fn statement() -> Value {
    serde_json::from_slice(&binding().payload().unwrap()).unwrap()
}

/// `payload` signed by the authority.
fn signed(payload: &[u8]) -> Vec<u8> {
    authority_key().sign(PAYLOAD_TYPE, payload).to_bytes()
}

/// The statement after `change`, signed by the authority.
fn signed_with(change: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut value = statement();
    change(&mut value);
    signed(&serde_json::to_vec(&value).unwrap())
}

fn predicate(field: &'static str, value: Value) -> impl FnOnce(&mut Value) {
    move |statement| statement["predicate"][field] = value
}

/// The valid envelope's JSON, to change after signing.
fn envelope_value() -> Value {
    serde_json::from_slice(&certificate()).unwrap()
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
    let payload = binding().payload().unwrap();
    let text = String::from_utf8(payload.clone()).unwrap();
    let valid = String::from_utf8(certificate()).unwrap();
    let envelope = |change: &dyn Fn(&mut Value)| {
        let mut value = envelope_value();
        change(&mut value);
        serde_json::to_vec(&value).unwrap()
    };
    let mut small_order = reef_evidence::keys::SPKI_PREFIX.to_vec();
    small_order.extend_from_slice(&reef_evidence::keys::SMALL_ORDER[1]);
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
            envelope(&|e| e["extra"] = nested(32)),
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
            authority_key()
                .sign("application/json", &payload)
                .to_bytes(),
            Check("payload_type", None),
        ),
        (
            "two-signatures",
            envelope(&|e| {
                let second = e["signatures"][0].clone();
                e["signatures"].as_array_mut().unwrap().push(second);
            }),
            Check("signatures", None),
        ),
        // The authority, then the signature, before the statement.
        (
            "unknown-authority",
            other_key().sign(PAYLOAD_TYPE, &payload).to_bytes(),
            Check("authority", None),
        ),
        (
            "other-signer",
            {
                let junk = b"not even json";
                let sig = other_key().sign_bytes(&pae(PAYLOAD_TYPE, junk));
                reef_evidence::Envelope::new(
                    PAYLOAD_TYPE,
                    junk,
                    authority_key().public_key().id().as_str(),
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
            "subject-digest",
            signed_with(|s| s["subject"][0]["digest"]["sha256"] = json!("0".repeat(64))),
            Check("subject", None),
        ),
        (
            "subject-name",
            signed_with(|s| s["subject"][0]["name"] = json!("tree")),
            Check("subject", None),
        ),
        (
            "issuer-key-id",
            signed_with(|s| {
                s["predicate"]["issuer"]["keyId"] = json!(other_key().public_key().id().as_str());
            }),
            Check("predicate", Some("issuer")),
        ),
        (
            "not-after",
            signed_with(predicate("notAfter", json!("2026-11-06T10:00:00.000Z"))),
            Check("predicate", Some("notAfter")),
        ),
        (
            "human-for-ci",
            signed_with(|s| {
                s["predicate"]["identity"]["kind"] = json!("ci");
                s["predicate"]["actor"] = json!("human");
            }),
            Check("predicate", Some("actor")),
        ),
        (
            "unknown-presence",
            signed_with(predicate("presence", json!("user_verified"))),
            Check("predicate", Some("presence")),
        ),
        (
            "unknown-custody",
            signed_with(predicate("custody", json!("hardware"))),
            Check("predicate", Some("custody")),
        ),
        (
            "unknown-actor",
            signed_with(predicate("actor", json!("robot"))),
            Check("predicate", Some("actor")),
        ),
        (
            "unknown-kind",
            signed_with(|s| s["predicate"]["identity"]["kind"] = json!("team")),
            Check("predicate", Some("identity")),
        ),
        (
            "identity-id",
            signed_with(|s| s["predicate"]["identity"]["id"] = json!("idn_8F3A0C2D91E4B675")),
            Check("predicate", Some("identity")),
        ),
        (
            "identity-name",
            signed_with(|s| s["predicate"]["identity"]["name"] = json!("Alice")),
            Check("predicate", Some("identity")),
        ),
        (
            "small-order-key",
            signed_with(|s| {
                s["predicate"]["publicKey"] = json!(STANDARD.encode(&small_order));
                s["subject"][0]["digest"]["sha256"] = json!(KeyId::of(&small_order).as_str());
            }),
            Check("predicate", Some("publicKey")),
        ),
        (
            "non-canonical-key",
            signed_with(|s| {
                // y = p + 3 = 2^255 - 16, which ring would read as y = 3.
                let mut point = [0xff_u8; 32];
                point[0] = 0xf0;
                point[31] = 0x7f;
                let mut spki = reef_evidence::keys::SPKI_PREFIX.to_vec();
                spki.extend_from_slice(&point);
                s["predicate"]["publicKey"] = json!(STANDARD.encode(&spki));
                s["subject"][0]["digest"]["sha256"] = json!(KeyId::of(&spki).as_str());
            }),
            Check("predicate", Some("publicKey")),
        ),
        (
            "unknown-issuer-kind",
            signed_with(|s| s["predicate"]["issuer"]["kind"] = json!("sigstore")),
            Check("predicate", Some("issuer")),
        ),
        (
            "duplicate-predicate",
            signed(
                text.replacen(
                    "\"actor\":\"agent\"",
                    "\"actor\":\"human\",\"actor\":\"agent\"",
                    1,
                )
                .as_bytes(),
            ),
            Check("predicate", Some("actor")),
        ),
    ]
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/session")
}

/// Every fixture file and its bytes.
fn files() -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    files.insert("valid.json".to_owned(), certificate());
    files.insert(
        "authority.pub.pem".to_owned(),
        authority_key().public_key().to_pem().into_bytes(),
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
            "authority": "authority.pub.pem",
            "authority_key_id": authority_key().public_key().id().as_str(),
            "session_key_id": session_key().public_key().id().as_str(),
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
fn certify_and_check() {
    let envelope = certificate();
    let certified = check_certificate(&envelope, &known).unwrap();
    assert_eq!(certified.binding, binding());
    // Canonical and deterministic: the bytes certify writes are the
    // canonical form, and the same binding signs to the same bytes.
    assert_eq!(certified.canonical, envelope);
    assert_eq!(certificate(), envelope);
    assert_eq!(certified.id, reef_evidence::sha256_hex(&envelope));
    assert_eq!(&certified.authority, &authority());
    // The subject's digest is the session key's id.
    let statement: Value = serde_json::from_slice(&certified.payload).unwrap();
    assert_eq!(statement["predicateType"], SESSION_KEY_PREDICATE_TYPE);
    assert_eq!(
        statement["subject"][0]["digest"]["sha256"],
        json!(session_key().public_key().id().as_str())
    );
    let spki = STANDARD
        .decode(statement["predicate"]["publicKey"].as_str().unwrap())
        .unwrap();
    assert_eq!(&KeyId::of(&spki), session_key().public_key().id());
    // The fields print in the format's order, times to the millisecond.
    let text = String::from_utf8(certified.payload.clone()).unwrap();
    assert!(
        text.starts_with(
            "{\"_type\":\"https://in-toto.io/Statement/v1\",\"subject\":[{\"name\":\"session-key\""
        ),
        "{text}"
    );
    assert!(
        text.contains("\"actor\":\"agent\",\"agent\":\"builder-3\",\"session\":\"ses_41c9e07ab2d35f18\",\"presence\":\"none\",\"custody\":\"client\",\"notBefore\":\"2026-11-06T10:00:00.000Z\",\"notAfter\":\"2026-11-06T11:00:00.000Z\",\"issuer\":{\"kind\":\"reef-daemon\""),
        "{text}"
    );
    // An unknown predicate field is kept, as signed, and ignored.
    let future = signed_with(predicate("futureField", json!({"x": 1})));
    let read = check_certificate(&future, &known).unwrap();
    assert_eq!(read.binding, binding());
    assert!(
        String::from_utf8(read.payload)
            .unwrap()
            .contains("futureField")
    );
    // No agent is no field.
    let mut quiet = binding();
    quiet.agent = None;
    let checked = check_certificate(
        &certify(&authority_key(), &quiet).unwrap().to_bytes(),
        &known,
    )
    .unwrap();
    assert_eq!(checked.binding.agent, None);
    assert!(
        !String::from_utf8(checked.payload)
            .unwrap()
            .contains("\"agent\":")
    );
    // A verifier that does not know the authority refuses it.
    let error = check_certificate(&envelope, &|_| None).unwrap_err();
    assert_eq!(error.check(), Some("authority"));
}

// 2.
#[test]
fn certificate_refusals() {
    let fixtures = refusals();
    assert!(fixtures.len() >= 20, "{}", fixtures.len());
    for (file, envelope, expect) in fixtures {
        let error = check_certificate(&envelope, &known).unwrap_err();
        match expect {
            Expect::Check(check, field) => {
                assert_eq!(error.check(), Some(check), "{file}: {error}");
                assert_eq!(error.field(), field, "{file}: {error}");
            }
            Expect::Signature => {
                assert!(
                    matches!(error, Invalid::Signature { .. }),
                    "{file}: {error}"
                );
            }
        }
    }
}

fn certified() -> Certified {
    check_certificate(&certificate(), &known).unwrap()
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "every caller stores it as an Option"
)]
fn revoked(at: i64, invalid_from: i64) -> Option<Revocation> {
    Some(Revocation {
        revoked_at_ms: at,
        invalid_from_ms: invalid_from,
        reason: None,
    })
}

// 3.
#[test]
fn statuses_at_their_boundaries() {
    let certified = certified();
    let none = Revocations::default();
    let at = |ms: i64| status_at(&certified, ms, &none);
    assert_eq!(at(NOT_BEFORE - 1), IdentityStatus::Expired);
    assert_eq!(at(NOT_BEFORE), IdentityStatus::Bound);
    assert_eq!(at(NOT_BEFORE + 1), IdentityStatus::Bound);
    assert_eq!(at(NOT_AFTER - 1), IdentityStatus::Bound);
    assert_eq!(at(NOT_AFTER), IdentityStatus::Bound);
    assert_eq!(at(NOT_AFTER + 1), IdentityStatus::Expired);
    assert_eq!(certified.revocation(&none), None);

    // Each of the key's, the session's and the identity's invalid_from, at
    // the witness time (revoked) and a millisecond after it (bound, with
    // the revocation reported).
    let witnessed = NOT_BEFORE + 600_000;
    let later = NOT_AFTER + 86_400_000;
    for slot in 0..3 {
        for (invalid_from, expect) in [
            (witnessed, IdentityStatus::Revoked),
            (witnessed + 1, IdentityStatus::Bound),
            (witnessed - 1, IdentityStatus::Revoked),
        ] {
            let mut revocations = Revocations::default();
            let revocation = revoked(later, invalid_from);
            match slot {
                0 => revocations.key = revocation,
                1 => revocations.session = revocation,
                _ => revocations.identity = revocation,
            }
            assert_eq!(
                status_at(&certified, witnessed, &revocations),
                expect,
                "slot {slot}, invalid_from {invalid_from}"
            );
            assert_eq!(
                certified.revocation(&revocations).unwrap().invalid_from_ms,
                invalid_from
            );
        }
    }
    // The authority's.
    for (invalid_from, expect) in [
        (witnessed, IdentityStatus::Revoked),
        (witnessed + 1, IdentityStatus::Bound),
    ] {
        let mut by_authority = certified.clone();
        by_authority.authority.revocation = revoked(later, invalid_from);
        assert_eq!(status_at(&by_authority, witnessed, &none), expect);
    }
    // A revocation after the witness time keeps it bound, and the earliest
    // applicable one is the one reported.
    let revocations = Revocations {
        key: revoked(later, later - 10),
        session: revoked(later, witnessed + 5),
        identity: revoked(later, later),
        binding: None,
    };
    assert_eq!(
        status_at(&certified, witnessed, &revocations),
        IdentityStatus::Bound
    );
    assert_eq!(
        certified.revocation(&revocations).unwrap().invalid_from_ms,
        witnessed + 5
    );
    let mut by_authority = certified.clone();
    by_authority.authority.revocation = revoked(later, witnessed + 2);
    assert_eq!(
        by_authority
            .revocation(&revocations)
            .unwrap()
            .invalid_from_ms,
        witnessed + 2
    );
    // Revoked outranks expired: a key revoked from issue reads revoked
    // after its window too.
    let since_issued = Revocations {
        key: revoked(later, NOT_BEFORE),
        ..Revocations::default()
    };
    assert_eq!(
        status_at(&certified, NOT_AFTER + 1, &since_issued),
        IdentityStatus::Revoked
    );
    assert_eq!(IdentityStatus::Revoked.as_str(), "revoked");
}
