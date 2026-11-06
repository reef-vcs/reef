//! The evidence format (R3.01 tests 1 to 7).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use common::{Expect, KEY_ID, PKCS8_V1_PEM, TREE, claim, key, refusals, signed, spki, tree};
use reef_evidence::keys::{PKCS8_V1_PREFIX, SMALL_ORDER, SPKI_PREFIX};
use reef_evidence::{
    Invalid, KeyId, Outcome, PAYLOAD_TYPE, PublicKey, SigningKey, Statement, Verified, pae,
};
use serde_json::{Value, json};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

// The committed fixtures are what `common` makes.
#[test]
fn fixtures_are_current() {
    let dir = fixtures_dir();
    let update = std::env::var_os("REEF_UPDATE_FIXTURES").is_some();
    for (name, bytes) in common::files() {
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
fn pae_matches_the_spec() {
    assert_eq!(
        pae("http://example.com/HelloWorld", b"hello world"),
        b"DSSEv1 29 http://example.com/HelloWorld 11 hello world"
    );
    assert_eq!(pae("t", b""), b"DSSEv1 1 t 0 ");
    // LEN counts bytes, not characters: "é" is two.
    assert_eq!(pae("é", b"x"), "DSSEv1 2 é 1 x".as_bytes());
}

// 2.
#[test]
fn keys() {
    // A generated key loads back, and its .pub PEM is its public key.
    let (private, public) = SigningKey::generate().unwrap();
    assert!(
        private.starts_with("-----BEGIN PRIVATE KEY-----\n"),
        "{private}"
    );
    assert!(
        public.starts_with("-----BEGIN PUBLIC KEY-----\n"),
        "{public}"
    );
    let loaded = SigningKey::from_pkcs8_pem(&private).unwrap();
    let public = PublicKey::from_pem(&public).unwrap();
    assert_eq!(loaded.public_key(), &public);
    assert_eq!(public.spki().len(), 44);
    assert_eq!(public.id(), &KeyId::of(public.spki()));

    // A fixed seed's key id is what OpenSSL derives for the same key.
    let fixed = key();
    assert_eq!(fixed.public_key().id().as_str(), KEY_ID);
    assert_eq!(&fixed.public_key().spki()[..12], SPKI_PREFIX);
    let round_trip = PublicKey::from_spki(fixed.public_key().spki()).unwrap();
    assert_eq!(&round_trip, fixed.public_key());
    assert_eq!(
        PublicKey::from_pem(&round_trip.to_pem()).unwrap(),
        round_trip
    );

    // PKCS#8 v1 (OpenSSL 3's, no public key) and v2 (ring's) load and sign.
    let v1 = SigningKey::from_pkcs8_pem(PKCS8_V1_PEM).unwrap();
    assert_eq!(v1.public_key().id().as_str(), KEY_ID);
    let mut v1_der = PKCS8_V1_PREFIX.to_vec();
    v1_der.extend_from_slice(&common::SEED);
    assert_eq!(
        SigningKey::from_pkcs8_der(&v1_der).unwrap().public_key(),
        v1.public_key()
    );
    for signer in [&v1, &loaded] {
        let envelope = signer.sign(PAYLOAD_TYPE, &claim().payload(&tree()).unwrap());
        Verified::check(&envelope.to_bytes(), signer.public_key().spki()).unwrap();
    }

    // The eight small-order points and a non-canonical y are refused.
    for point in SMALL_ORDER {
        let mut der = SPKI_PREFIX.to_vec();
        der.extend_from_slice(&point);
        let error = PublicKey::from_spki(&der).unwrap_err();
        assert_eq!(error.check(), Some("public_key"), "{point:02x?}");
        assert!(error.to_string().contains("small-order"), "{error}");
    }
    // y = p + 3 = 2^255 - 16, which ring would read as y = 3.
    let mut above_p = [0xff; 32];
    above_p[0] = 0xf0;
    above_p[31] = 0x7f;
    let error = PublicKey::from_raw(above_p).unwrap_err();
    assert!(error.to_string().contains("non-canonical"), "{error}");
    // The identity with the sign bit set: x = 0 has no negative.
    let mut signed_identity = SMALL_ORDER[0];
    signed_identity[31] |= 0x80;
    let error = PublicKey::from_raw(signed_identity).unwrap_err();
    assert!(error.to_string().contains("non-canonical"), "{error}");

    // Another algorithm's SubjectPublicKeyInfo: P-256, and RSA's header.
    let rng = ring::rand::SystemRandom::new();
    let p256 = ring::signature::EcdsaKeyPair::generate_pkcs8(
        &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        &rng,
    )
    .unwrap();
    let p256 = ring::signature::EcdsaKeyPair::from_pkcs8(
        &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        p256.as_ref(),
        &rng,
    )
    .unwrap();
    let mut p256_spki = hex("3059301306072a8648ce3d020106082a8648ce3d030107034200");
    p256_spki.extend_from_slice(ring::signature::KeyPair::public_key(&p256).as_ref());
    let rsa_spki = hex("30820122300d06092a864886f70d01010105000382010f00");
    for der in [p256_spki, rsa_spki, Vec::new(), spki()[..43].to_vec()] {
        let error = PublicKey::from_spki(&der).unwrap_err();
        assert_eq!(error.check(), Some("public_key"), "{der:02x?}");
    }
    // A PEM of the wrong kind says what it is.
    let error = SigningKey::from_pkcs8_pem(&fixed.public_key().to_pem()).unwrap_err();
    assert!(error.to_string().contains("PUBLIC KEY block"), "{error}");
    let ssh =
        "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA==\n-----END OPENSSH PRIVATE KEY-----\n";
    let error = SigningKey::from_pkcs8_pem(ssh).unwrap_err();
    assert!(error.to_string().contains("OPENSSH PRIVATE KEY"), "{error}");
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

// 3.
#[test]
fn refusals_each_have_their_check() {
    let fixtures = refusals();
    assert!(fixtures.len() >= 36, "{}", fixtures.len());
    for fixture in fixtures {
        let error =
            Verified::check(&fixture.envelope, &fixture.public_key).expect_err(fixture.file);
        match fixture.expect {
            Expect::Check(check, field) => {
                assert_eq!(error.check(), Some(check), "{}: {error}", fixture.file);
                assert_eq!(error.field(), field, "{}: {error}", fixture.file);
            }
            Expect::SignatureInvalid => assert_eq!(
                error,
                Invalid::Signature {
                    key_id: KEY_ID.into()
                },
                "{}",
                fixture.file
            ),
        }
    }
}

// 4.
#[test]
fn signature_before_parse() {
    // Not JSON, and signed by another key: the signature fails first.
    let junk = b"not json at all";
    let mut envelope: Value = serde_json::from_slice(&signed(junk)).unwrap();
    let wrong = common::other_key().sign_bytes(&pae(PAYLOAD_TYPE, junk));
    envelope["signatures"][0]["sig"] = json!(STANDARD.encode(wrong));
    let bytes = serde_json::to_vec(&envelope).unwrap();
    let error = Verified::check(&bytes, &spki()).unwrap_err();
    assert!(matches!(error, Invalid::Signature { .. }), "{error}");
    // Correctly signed, it reaches the parser.
    let error = Verified::check(&signed(junk), &spki()).unwrap_err();
    assert_eq!(error.check(), Some("statement"), "{error}");
    // A malformed key is refused before the signature is looked at.
    let error = Verified::check(&bytes, &[0x30, 0x00]).unwrap_err();
    assert_eq!(error.check(), Some("public_key"), "{error}");
}

// 5.
#[test]
fn canonical_envelope() {
    let canonical = signed(&claim().payload(&tree()).unwrap());
    let envelope = common::parse(&canonical);
    let (payload, sig) = (&envelope.payload, &envelope.signatures[0].sig);
    let url_safe = |text: &str| URL_SAFE_NO_PAD.encode(STANDARD.decode(text).unwrap());
    let variants = [
        canonical.clone(),
        // URL-safe base64 without padding.
        serde_json::to_vec(&json!({
            "payload": url_safe(payload),
            "payloadType": PAYLOAD_TYPE,
            "signatures": [{"keyid": KEY_ID, "sig": url_safe(sig)}],
        }))
        .unwrap(),
        // Standard without padding, whitespace, other key order, and
        // unknown fields at both levels.
        format!(
            "{{\n  \"signatures\" : [ {{ \"sig\": \"{}\", \"keyid\": \"{KEY_ID}\", \"extra\": 1 }} ],\n  \"payloadType\": \"{PAYLOAD_TYPE}\",\n  \"payload\": \"{}\",\n  \"comment\": {{\"x\": [1, 2]}}\n}}\n",
            sig.trim_end_matches('='),
            payload.trim_end_matches('='),
        )
        .into_bytes(),
    ];
    let expected = format!(
        "{{\"payload\":\"{payload}\",\"payloadType\":\"application/vnd.in-toto+json\",\"signatures\":[{{\"keyid\":\"{KEY_ID}\",\"sig\":\"{sig}\"}}]}}"
    );
    assert!(
        payload.ends_with('=') || sig.ends_with('='),
        "padding is exercised"
    );
    for variant in variants {
        let verified = Verified::check(&variant, &spki()).unwrap();
        assert_eq!(
            String::from_utf8(verified.canonical.clone()).unwrap(),
            expected
        );
        assert_eq!(verified.id, reef_evidence::sha256_hex(expected.as_bytes()));
        assert_eq!(verified.id.len(), 64);
    }
}

// 6.
#[test]
fn round_trip_and_determinism() {
    let first = claim().sign(&tree(), &key()).unwrap().to_bytes();
    let second = claim().sign(&tree(), &key()).unwrap().to_bytes();
    assert_eq!(first, second);
    let verified = Verified::check(&first, &spki()).unwrap();
    assert_eq!(
        verified.canonical, first,
        "the producer writes canonical envelopes"
    );
    let index = &verified.index;
    assert_eq!(index.tree_id, TREE);
    assert_eq!(index.kind, "test");
    assert_eq!(index.result, Outcome::Pass);
    assert_eq!(index.target.as_deref(), Some("crates/reef-lib:jj_writes"));
    assert_eq!(index.started_at_ms, 1_793_872_800_000);
    assert_eq!(index.finished_at_ms, 1_793_872_872_000);
    assert_eq!(index.artifacts, claim().artifacts);
    assert_eq!(verified.key_id().as_str(), KEY_ID);
    assert_eq!(verified.statement, claim().statement(&tree()).unwrap());
    assert_eq!(verified.payload, claim().payload(&tree()).unwrap());
    // The payload's key order is the format's example's.
    let text = String::from_utf8(verified.payload).unwrap();
    let order = [
        "\"_type\"",
        "\"subject\"",
        "\"predicateType\"",
        "\"predicate\"",
        "\"kind\"",
        "\"result\"",
        "\"target\"",
        "\"startedOn\"",
        "\"finishedOn\"",
        "\"artifacts\"",
        "\"annotations\"",
    ];
    let positions: Vec<usize> = order.iter().map(|key| text.find(key).unwrap()).collect();
    assert!(positions.is_sorted(), "{text}");
    assert!(!text.ends_with('\n'));

    // A field a later Reef adds is kept, and does not fail.
    let future = common::signed_with(|s| s["predicate"]["futureField"] = json!({"x": 1}));
    let verified = Verified::check(&future, &spki()).unwrap();
    assert_eq!(
        verified.statement.predicate.unknown.get("futureField"),
        Some(&json!({"x": 1}))
    );
}

// R3.01 review F2: a claim outside the format's times is refused, never a
// panic.
#[test]
fn claims_out_of_range_are_refused() {
    use std::time::{Duration, UNIX_EPOCH};
    let mut early = claim();
    early.started_on = UNIX_EPOCH - Duration::from_hours(24 * 365 * 100);
    let error = early.sign(&tree(), &key()).unwrap_err();
    assert_eq!(error.field(), Some("startedOn"), "{error}");
    let mut late = claim();
    late.finished_on = UNIX_EPOCH + Duration::from_hours(24 * 3_500_000);
    assert_eq!(
        late.sign(&tree(), &key()).unwrap_err().field(),
        Some("finishedOn")
    );
    // The last instant the format writes is fine.
    let mut last = claim();
    last.finished_on = UNIX_EPOCH + Duration::from_millis(253_402_300_799_999);
    let envelope = last.sign(&tree(), &key()).unwrap().to_bytes();
    let verified = Verified::check(&envelope, &spki()).unwrap();
    assert_eq!(
        verified.statement.predicate.finished_on,
        "9999-12-31T23:59:59.999Z"
    );
    // A claim the checker would refuse is refused before signing.
    let mut bad = claim();
    bad.kind = "Test".into();
    assert_eq!(bad.payload(&tree()).unwrap_err().field(), Some("kind"));
    // The accepted forms of a time.
    for good in [
        "2026-11-05T10:00:00Z",
        "2026-11-05T10:00:00.5Z",
        "2026-11-05T10:00:00.123456789Z",
    ] {
        assert!(
            reef_evidence::statement::parse_time(good).is_some(),
            "{good}"
        );
    }
    for bad in [
        "2026-11-05T10:00:00.1234567890Z",
        "2026-11-05 10:00:00Z",
        "2026-11-05t10:00:00Z",
        "2026-11-05T10:00:00z",
        "1969-12-31T23:59:59Z",
        "2026-13-05T10:00:00Z",
        "2026-11-05T23:59:60Z",
    ] {
        assert!(reef_evidence::statement::parse_time(bad).is_none(), "{bad}");
    }
}

// 7.
#[test]
fn schema_matches_the_types() {
    let schema = Value::from(schemars::schema_for!(Statement));
    let validator = jsonschema::draft202012::options()
        .should_validate_formats(false)
        .build(&schema)
        .unwrap();
    let mut payload = common::statement_value();
    payload["predicate"]["futureField"] = json!(true);
    assert!(validator.is_valid(&payload), "{payload}");
    let wrong_result = {
        let mut p = payload.clone();
        p["predicate"]["result"] = json!("passed");
        p
    };
    let missing_kind = {
        let mut p = payload.clone();
        p["predicate"].as_object_mut().unwrap().remove("kind");
        p
    };
    let artifacts_not_array = {
        let mut p = payload.clone();
        p["predicate"]["artifacts"] = json!({"name": "log"});
        p
    };
    for invalid in [wrong_result, missing_kind, artifacts_not_array] {
        assert!(!validator.is_valid(&invalid), "{invalid}");
        // The schema is a description; the checker agrees.
        let bytes = signed(&serde_json::to_vec(&invalid).unwrap());
        assert_eq!(
            Verified::check(&bytes, &spki()).unwrap_err().check(),
            Some("predicate")
        );
    }
}
