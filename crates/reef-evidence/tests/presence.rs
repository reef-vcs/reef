//! Verified human presence (R3.43 tests 1 and 2): passkey assertions from
//! Chromium's virtual authenticator and from the test authenticator verify,
//! each check refuses alone, and a `user_verified` certificate carries its
//! assertion through `certify` and `check_certificate`.
//!
//! `tests/fixtures/webauthn/chromium-*.json` are a real browser's bytes
//! (`chromium.cjs` beside them makes them again). `user-verified.json` is
//! the certificate this file certifies from the `EdDSA` one, kept so a
//! reader of another version can be run against it; `fixtures_are_current`
//! fails when it drifts, and `REEF_UPDATE_FIXTURES=1` rewrites it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests: a panic is the failure report"
)]

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use reef_evidence::session::{
    Actor, Authority, Binding, Custody, IdentityKind, IdentityRef, Issuer, IssuerKind, Presence,
    PresenceProof, certify, check_certificate,
};
use reef_evidence::webauthn::testing::{AAGUID, Authenticator, client_data_json};
use reef_evidence::webauthn::{
    Algorithm, ApprovalFor, Approves, Assertion, Check, PasskeyChange, PasskeyKey,
    PresenceEvidence, approval_challenge, approval_payload, check_assertion, check_registration,
    passkey_change_challenge, passkey_change_payload,
};
use reef_evidence::{Invalid, KeyId, PAYLOAD_TYPE, SigningKey};
use serde_json::{Value, json};

const RP: &str = "localhost";
const ORIGIN: &str = "http://localhost:4242";
const AUTHORITY_SEED: [u8; 32] = [0xa1; 32];
/// The session key the browser vectors approve.
const SESSION_SEED: [u8; 32] = [0xd4; 32];
const IDENTITY: &str = "idn_8f3a0c2d91e4b675";
const SESSION: &str = "ses_41c9e07ab2d35f18";
/// 2026-11-06T10:00:00Z, and two minutes later.
const NOT_BEFORE: i64 = 1_793_959_200_000;
const NOT_AFTER: i64 = NOT_BEFORE + 120_000;

fn authority_key() -> SigningKey {
    SigningKey::from_seed(&AUTHORITY_SEED).unwrap()
}

fn session_key() -> SigningKey {
    SigningKey::from_seed(&SESSION_SEED).unwrap()
}

fn known(key_id: &KeyId) -> Option<Authority> {
    let authority = authority_key();
    (key_id == authority.public_key().id()).then(|| Authority {
        key: authority.public_key().clone(),
        kind: IssuerKind::ReefDaemon,
        revocation: None,
    })
}

fn approves() -> Approves {
    Approves {
        statement: reef_evidence::sha256_hex(b"a signoff statement"),
        change: "8d1f0e7c6b5a49382716f5e4d3c2b1a0".into(),
    }
}

/// The challenge for `session_key()`'s sign-off of `approves`.
fn challenge_for(approves: &Approves) -> [u8; 32] {
    approval_challenge(&ApprovalFor {
        authority: authority_key().public_key().id(),
        key: session_key().public_key().id(),
        identity: IDENTITY,
        session: SESSION,
        approves,
    })
}

/// A browser vector: its JSON and the bytes it names.
struct Vector {
    json: Value,
    passkey: PasskeyKey,
    assertion: Assertion,
}

fn vector(name: &str) -> Vector {
    let path = fixtures_dir().join(format!("chromium-{name}.json"));
    let json: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let bytes = |field: &str| STANDARD.decode(json[field].as_str().unwrap()).unwrap();
    let passkey =
        PasskeyKey::from_spki(&bytes("publicKey"), json["algorithm"].as_i64().unwrap()).unwrap();
    let assertion = Assertion {
        authenticator_data: bytes("authenticatorData"),
        client_data_json: bytes("clientDataJSON"),
        signature: bytes("signature"),
    };
    Vector {
        json,
        passkey,
        assertion,
    }
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/webauthn")
}

/// `bytes` with one bit of byte `at` flipped.
fn flipped(bytes: &[u8], at: usize) -> Vec<u8> {
    let mut bytes = bytes.to_vec();
    bytes[at] ^= 0x01;
    bytes
}

fn refused(result: Result<impl std::fmt::Debug, reef_evidence::webauthn::AssertionError>) -> Check {
    result.unwrap_err().check
}

// 1: the browsers' own bytes.
#[test]
fn chromium_assertions_verify() {
    for (name, algorithm) in [("es256", Algorithm::Es256), ("eddsa", Algorithm::EdDsa)] {
        let Vector {
            json,
            passkey,
            assertion,
        } = vector(name);
        assert_eq!(passkey.algorithm(), algorithm, "{name}");
        // The inputs are this file's, and the challenge is derived from them
        // the same way in JavaScript and here.
        let inputs = &json["inputs"];
        assert_eq!(
            inputs["authority"],
            authority_key().public_key().id().as_str()
        );
        assert_eq!(inputs["key"], session_key().public_key().id().as_str());
        assert_eq!(inputs["statement"], approves().statement);
        assert_eq!(inputs["change"], approves().change);
        let challenge = challenge_for(&approves());
        assert_eq!(
            json["challenge"],
            URL_SAFE_NO_PAD.encode(challenge),
            "{name}"
        );
        let asserted = check_assertion(&passkey, RP, &challenge, &assertion).unwrap();
        assert_eq!(asserted.origin, json["origin"].as_str().unwrap());
        assert!(asserted.sign_count > 0, "{name}: {}", asserted.sign_count);
        assert_eq!(
            refused(check_assertion(
                &passkey,
                RP,
                &challenge,
                &Assertion {
                    signature: flipped(&assertion.signature, assertion.signature.len() / 2),
                    ..assertion.clone()
                }
            )),
            Check::Signature,
            "{name}"
        );
        let other = Approves {
            change: "0".repeat(32),
            ..approves()
        };
        assert_eq!(
            refused(check_assertion(
                &passkey,
                RP,
                &challenge_for(&other),
                &assertion
            )),
            Check::Challenge,
            "{name}"
        );
        assert_eq!(
            refused(check_assertion(
                &passkey,
                "example.com",
                &challenge,
                &assertion
            )),
            Check::Origin,
            "{name}"
        );
        let other_algorithm = if algorithm == Algorithm::Es256 {
            -8
        } else {
            -7
        };
        assert_eq!(
            refused(PasskeyKey::from_spki(passkey.spki(), other_algorithm)),
            Check::Algorithm,
            "{name}"
        );
    }
}

/// The RP ID hash, `flags` and a sign count of 1.
fn authenticator_data(rp_id: &str, flags: u8) -> Vec<u8> {
    let mut data = ring::digest::digest(&ring::digest::SHA256, rp_id.as_bytes())
        .as_ref()
        .to_vec();
    data.push(flags);
    data.extend_from_slice(&1_u32.to_be_bytes());
    data
}

// 1: each check refuses alone, for every algorithm.
#[test]
#[expect(clippy::too_many_lines, reason = "a table")]
fn each_check_refuses_alone() {
    let challenge = challenge_for(&approves());
    for &algorithm in Algorithm::ALL {
        let mut authenticator = Authenticator::new(algorithm);
        let passkey = authenticator.passkey().clone();
        let valid = authenticator.assert(RP, ORIGIN, &challenge);
        let asserted = check_assertion(&passkey, RP, &challenge, &valid).unwrap();
        assert_eq!((asserted.origin.as_str(), asserted.sign_count), (ORIGIN, 1));
        // Bytes the passkey signs, so only the check they break refuses.
        let signed = |authenticator_data: Vec<u8>, client_data_json: Vec<u8>| Assertion {
            signature: authenticator.sign_assertion(&authenticator_data, &client_data_json),
            authenticator_data,
            client_data_json,
        };
        let client = |ceremony: &str, challenge: &[u8; 32], origin: &str| {
            client_data_json(ceremony, challenge, origin)
        };
        let data = authenticator_data(RP, 0x05);
        let get = client("webauthn.get", &challenge, ORIGIN);
        let cross_origin = String::from_utf8(get.clone())
            .unwrap()
            .replace("\"crossOrigin\":false", "\"crossOrigin\":true")
            .into_bytes();
        let cases = [
            ("the signed bytes", signed(data.clone(), get.clone()), None),
            (
                "type webauthn.create",
                signed(data.clone(), client("webauthn.create", &challenge, ORIGIN)),
                Some(Check::Type),
            ),
            (
                "client data that is not JSON",
                signed(data.clone(), b"webauthn.get".to_vec()),
                Some(Check::Type),
            ),
            (
                "another challenge",
                signed(data.clone(), client("webauthn.get", &[7; 32], ORIGIN)),
                Some(Check::Challenge),
            ),
            (
                "another host",
                signed(
                    data.clone(),
                    client("webauthn.get", &challenge, "http://example.com:4242"),
                ),
                Some(Check::Origin),
            ),
            (
                "the loopback IP",
                signed(
                    data.clone(),
                    client("webauthn.get", &challenge, "http://127.0.0.1:4242"),
                ),
                Some(Check::Origin),
            ),
            (
                "a suffix of the host",
                signed(
                    data.clone(),
                    client("webauthn.get", &challenge, "http://localhost.example.com"),
                ),
                Some(Check::Origin),
            ),
            (
                "a path",
                signed(
                    data.clone(),
                    client("webauthn.get", &challenge, "http://localhost:4242/x"),
                ),
                Some(Check::Origin),
            ),
            (
                "port 0",
                signed(
                    data.clone(),
                    client("webauthn.get", &challenge, "http://localhost:0"),
                ),
                Some(Check::Origin),
            ),
            (
                "a cross-origin frame",
                signed(data.clone(), cross_origin),
                Some(Check::Origin),
            ),
            (
                "another RP ID hash",
                signed(authenticator_data("example.com", 0x05), get.clone()),
                Some(Check::Rp),
            ),
            (
                "a short authenticatorData",
                signed(data[..36].to_vec(), get.clone()),
                Some(Check::Rp),
            ),
            (
                "a byte past the sign count without extensions",
                signed([&data[..], &[0]].concat(), get.clone()),
                Some(Check::Rp),
            ),
            (
                "attested credential data",
                signed(authenticator_data(RP, 0x45), get.clone()),
                Some(Check::Rp),
            ),
            (
                "no UV flag",
                signed(authenticator_data(RP, 0x01), get.clone()),
                Some(Check::Flags),
            ),
            (
                "no UP flag",
                signed(authenticator_data(RP, 0x04), get.clone()),
                Some(Check::Flags),
            ),
            (
                "a flipped signature byte",
                Assertion {
                    signature: flipped(&valid.signature, 8),
                    ..valid.clone()
                },
                Some(Check::Signature),
            ),
            (
                "a flipped authenticatorData byte",
                Assertion {
                    authenticator_data: flipped(&valid.authenticator_data, 36),
                    ..valid.clone()
                },
                Some(Check::Signature),
            ),
        ];
        for (case, assertion, expect) in cases {
            let result = check_assertion(&passkey, RP, &challenge, &assertion);
            assert_eq!(
                result.as_ref().err().map(|error| error.check),
                expect,
                "{algorithm}: {case}: {result:?}"
            );
        }
        // An https origin is accepted for any host, `localhost` included.
        let https = signed(
            data.clone(),
            client("webauthn.get", &challenge, "https://localhost"),
        );
        check_assertion(&passkey, RP, &challenge, &https).unwrap();
        // Another passkey's signature.
        let other = Authenticator::new(Algorithm::EdDsa);
        assert_eq!(
            refused(check_assertion(other.passkey(), RP, &challenge, &valid)),
            Check::Signature,
            "{algorithm}"
        );
        // An algorithm this reader does not verify.
        assert_eq!(
            refused(PasskeyKey::from_spki(passkey.spki(), -35)),
            Check::Algorithm
        );
        // A passkey whose user is not verified.
        authenticator.set_user_verified(false);
        let unverified = authenticator.assert(RP, ORIGIN, &challenge);
        assert_eq!(
            refused(check_assertion(&passkey, RP, &challenge, &unverified)),
            Check::Flags
        );
    }
}

/// One DER element, for building keys the authenticator would never make.
fn der(tag: u8, contents: &[u8]) -> Vec<u8> {
    let len = contents.len();
    let [high, low] = u16::try_from(len).unwrap().to_be_bytes();
    let head = match len {
        0..0x80 => vec![tag, low],
        0x80..0x100 => vec![tag, 0x81, low],
        _ => vec![tag, 0x82, high, low],
    };
    [head, contents.to_vec()].concat()
}

fn rsa_spki(modulus_bytes: usize, exponent: &[u8]) -> Vec<u8> {
    let n = [&[0, 0x80][..], &vec![1; modulus_bytes - 1]].concat();
    let key = der(0x30, &[der(0x02, &n), der(0x02, exponent)].concat());
    let algorithm = der(
        0x30,
        &[
            der(
                0x06,
                &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01],
            ),
            vec![0x05, 0x00],
        ]
        .concat(),
    );
    der(
        0x30,
        &[algorithm, der(0x03, &[&[0][..], &key].concat())].concat(),
    )
}

// 1: the public keys a passkey may have.
#[test]
fn public_keys_are_read_strictly() {
    for &algorithm in Algorithm::ALL {
        let authenticator = Authenticator::new(algorithm);
        let spki = authenticator.passkey().spki();
        let read = PasskeyKey::from_spki(spki, algorithm.cose()).unwrap();
        assert_eq!(read.fingerprint(), &KeyId::of(spki));
        assert_eq!(Algorithm::from_cose(algorithm.cose()), Some(algorithm));
        // Trailing bytes, and a truncated key.
        let trailing = [spki, &[0][..]].concat();
        assert_eq!(
            refused(PasskeyKey::from_spki(&trailing, algorithm.cose())),
            Check::Algorithm
        );
        assert_eq!(
            refused(PasskeyKey::from_spki(
                &spki[..spki.len() - 1],
                algorithm.cose()
            )),
            Check::Algorithm
        );
    }
    // RSA: 2048 to 8192 bits, an odd exponent of at least 3.
    PasskeyKey::from_spki(&rsa_spki(256, &[1, 0, 1]), -257).unwrap();
    for (bytes, exponent) in [
        (128, &[1, 0, 1][..]),
        (1025, &[1, 0, 1]),
        (256, &[2]),
        (256, &[1]),
    ] {
        assert_eq!(
            refused(PasskeyKey::from_spki(&rsa_spki(bytes, exponent), -257)),
            Check::Algorithm,
            "{bytes} bytes, exponent {exponent:?}"
        );
    }
    // A compressed P-256 point, and a small-order ed25519 key.
    let es256 = Authenticator::new(Algorithm::Es256);
    let mut compressed = es256.passkey().spki().to_vec();
    let point = compressed.len() - 65;
    compressed[point] = 0x02;
    assert_eq!(
        refused(PasskeyKey::from_spki(&compressed, -7)),
        Check::Algorithm
    );
    let mut small_order = reef_evidence::keys::SPKI_PREFIX.to_vec();
    small_order.extend_from_slice(&reef_evidence::keys::SMALL_ORDER[1]);
    assert_eq!(
        refused(PasskeyKey::from_spki(&small_order, -8)),
        Check::Algorithm
    );
}

#[test]
fn registration_is_checked() {
    for &algorithm in Algorithm::ALL {
        let challenge = [9; 32];
        let mut authenticator = Authenticator::new(algorithm);
        let registration = authenticator.register(RP, ORIGIN, &challenge);
        assert_eq!(registration.algorithm, algorithm.cose());
        let key = PasskeyKey::from_spki(&registration.public_key, registration.algorithm).unwrap();
        assert_eq!(key.fingerprint(), authenticator.passkey().fingerprint());
        let registered = check_registration(
            RP,
            &challenge,
            &registration.client_data_json,
            &registration.authenticator_data,
        )
        .unwrap();
        assert_eq!(registered.origin, ORIGIN);
        assert_eq!(registered.aaguid, AAGUID);
        assert_eq!(registered.credential_id, authenticator.credential_id());
        assert_eq!(registered.sign_count, 1);
        let check =
            |client: &[u8], data: &[u8]| refused(check_registration(RP, &challenge, client, data));
        let assertion = authenticator.assert(RP, ORIGIN, &challenge);
        assert_eq!(
            check(
                &assertion.client_data_json,
                &registration.authenticator_data
            ),
            Check::Type
        );
        assert_eq!(
            check(
                &registration.client_data_json,
                &assertion.authenticator_data
            ),
            Check::Rp,
            "no attested credential data"
        );
        assert_eq!(
            refused(check_registration(
                RP,
                &[8; 32],
                &registration.client_data_json,
                &registration.authenticator_data
            )),
            Check::Challenge
        );
        authenticator.set_user_verified(false);
        let unverified = authenticator.register(RP, ORIGIN, &challenge);
        assert_eq!(
            check(&unverified.client_data_json, &unverified.authenticator_data),
            Check::Flags
        );
    }
}

#[test]
fn challenges_are_pinned() {
    let authority = authority_key().public_key().id().clone();
    let key = session_key().public_key().id().clone();
    let approves = approves();
    let approval = ApprovalFor {
        authority: &authority,
        key: &key,
        identity: IDENTITY,
        session: SESSION,
        approves: &approves,
    };
    assert_eq!(
        String::from_utf8(approval_payload(&approval)).unwrap(),
        format!(
            "reef-presence-approval/v1\n{authority}\n{key}\n{IDENTITY}\nhuman\n{SESSION}\n{}\n{}\n",
            approves.statement, approves.change
        )
    );
    let registration = [5; 32];
    let add = PasskeyChange::Add {
        registration_challenge: &registration,
    };
    let remove = PasskeyChange::Remove {
        passkey: "pk_0123456789abcdef",
    };
    assert_eq!(
        String::from_utf8(passkey_change_payload(add)).unwrap(),
        format!("add\n{}\n", URL_SAFE_NO_PAD.encode(registration))
    );
    assert_eq!(
        passkey_change_payload(remove),
        b"remove\npk_0123456789abcdef\n"
    );
    // A passkey change is approved by an assertion over its own challenge,
    // never another change's or an approval's.
    let mut authenticator = Authenticator::new(Algorithm::Es256);
    let assertion = authenticator.assert(RP, ORIGIN, &passkey_change_challenge(remove));
    check_assertion(
        authenticator.passkey(),
        RP,
        &passkey_change_challenge(remove),
        &assertion,
    )
    .unwrap();
    for other in [passkey_change_challenge(add), approval_challenge(&approval)] {
        assert_eq!(
            refused(check_assertion(
                authenticator.passkey(),
                RP,
                &other,
                &assertion
            )),
            Check::Challenge
        );
    }
}

/// A `user_verified` binding for `session_key()`, approving `approves()`,
/// with `evidence`.
fn binding(evidence: PresenceEvidence) -> Binding {
    Binding {
        public_key: session_key().public_key().clone(),
        identity: IdentityRef {
            id: IDENTITY.into(),
            name: "alice".into(),
            kind: IdentityKind::Person,
        },
        actor: Actor::Human,
        agent: None,
        session: SESSION.into(),
        presence: PresenceProof::UserVerified(Box::new(evidence)),
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

/// The Chromium `EdDSA` vector as presence evidence.
fn chromium_evidence() -> PresenceEvidence {
    let Vector {
        json,
        passkey,
        assertion,
    } = vector("eddsa");
    PresenceEvidence {
        passkey,
        credential_id: STANDARD
            .decode(json["credentialId"].as_str().unwrap())
            .unwrap(),
        rp_id: RP.into(),
        approves: approves(),
        assertion,
    }
}

/// A test authenticator's evidence for `approves()`.
fn evidence(algorithm: Algorithm) -> PresenceEvidence {
    let mut authenticator = Authenticator::new(algorithm);
    let assertion = authenticator.assert(RP, ORIGIN, &challenge_for(&approves()));
    PresenceEvidence {
        passkey: authenticator.passkey().clone(),
        credential_id: authenticator.credential_id().to_vec(),
        rp_id: RP.into(),
        approves: approves(),
        assertion,
    }
}

fn user_verified_certificate() -> Vec<u8> {
    certify(&authority_key(), &binding(chromium_evidence()))
        .unwrap()
        .to_bytes()
}

#[test]
fn fixtures_are_current() {
    let path = fixtures_dir().join("user-verified.json");
    let bytes = user_verified_certificate();
    if std::env::var_os("REEF_UPDATE_FIXTURES").is_some() {
        std::fs::write(&path, &bytes).unwrap();
    }
    let found = std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "{}: {error}; run with REEF_UPDATE_FIXTURES=1",
            path.display()
        )
    });
    assert!(
        found == bytes,
        "user-verified.json is stale; run with REEF_UPDATE_FIXTURES=1"
    );
}

// 2.
#[test]
fn user_verified_round_trips() {
    let browser = binding(chromium_evidence());
    let certified = check_certificate(&user_verified_certificate(), &known).unwrap();
    assert_eq!(certified.binding, browser);
    assert_eq!(certified.binding.presence.kind(), Presence::UserVerified);
    let text = String::from_utf8(certified.payload).unwrap();
    let passkey = browser
        .presence
        .evidence()
        .unwrap()
        .passkey
        .fingerprint()
        .clone();
    assert!(
        text.contains(&format!(
            "\"session\":\"{SESSION}\",\"presence\":\"user_verified\",\"presenceEvidence\":{{\"type\":\"webauthn\",\"passkey\":\"{passkey}\",\"credentialId\":\""
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "\"approves\":{{\"statement\":\"{}\",\"change\":\"{}\"}},\"authenticatorData\":\"",
            approves().statement,
            approves().change
        )),
        "{text}"
    );
    assert!(text.contains("\"clientDataJSON\":\""), "{text}");
    // The schema describes it.
    let schema = Value::from(schemars::schema_for!(
        reef_evidence::session::SessionKeyStatement
    ));
    let validator = jsonschema::draft202012::options()
        .should_validate_formats(false)
        .build(&schema)
        .unwrap();
    let statement: Value = serde_json::from_str(&text).unwrap();
    assert!(validator.is_valid(&statement), "{statement}");
    // Every algorithm, through the test authenticator.
    for &algorithm in Algorithm::ALL {
        let binding = binding(evidence(algorithm));
        let envelope = certify(&authority_key(), &binding).unwrap().to_bytes();
        assert_eq!(
            check_certificate(&envelope, &known).unwrap().binding,
            binding,
            "{algorithm}"
        );
    }
}

/// The user-verified certificate's statement as JSON.
fn statement() -> Value {
    serde_json::from_slice(&binding(chromium_evidence()).payload().unwrap()).unwrap()
}

/// `change` applied to the statement, signed by the authority.
fn signed_with(change: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut value = statement();
    change(&mut value);
    authority_key()
        .sign(PAYLOAD_TYPE, &serde_json::to_vec(&value).unwrap())
        .to_bytes()
}

// 2: what is INVALID, though the authority signed it.
#[test]
#[expect(clippy::too_many_lines, reason = "a table")]
fn user_verified_refusals() {
    let evidence_of = |s: &mut Value| -> Value {
        s["predicate"]
            .as_object_mut()
            .unwrap()
            .remove("presenceEvidence")
            .unwrap()
    };
    let none_evidence = statement()["predicate"]["presenceEvidence"].clone();
    let cases: Vec<(&str, Vec<u8>, &str, Option<&str>)> = vec![
        (
            "user_verified without evidence",
            signed_with(|s| {
                evidence_of(s);
            }),
            "presenceEvidence",
            Some("required"),
        ),
        (
            "evidence with presence none",
            signed_with(|s| {
                s["predicate"]["presence"] = json!("none");
                s["predicate"]["presenceEvidence"] = none_evidence;
            }),
            "presenceEvidence",
            Some("only with"),
        ),
        (
            "a statement approved after the assertion",
            signed_with(|s| {
                s["predicate"]["presenceEvidence"]["approves"]["statement"] = json!("0".repeat(64));
            }),
            "presenceEvidence",
            Some("challenge"),
        ),
        (
            "a change approved after the assertion",
            signed_with(|s| {
                s["predicate"]["presenceEvidence"]["approves"]["change"] = json!("f".repeat(32));
            }),
            "presenceEvidence",
            Some("challenge"),
        ),
        (
            "another session",
            signed_with(|s| s["predicate"]["session"] = json!("ses_0123456789abcdef")),
            "presenceEvidence",
            Some("challenge"),
        ),
        (
            "another identity",
            signed_with(|s| s["predicate"]["identity"]["id"] = json!("idn_0123456789abcdef")),
            "presenceEvidence",
            Some("challenge"),
        ),
        (
            "a fingerprint that is not the key's",
            signed_with(|s| s["predicate"]["presenceEvidence"]["passkey"] = json!("0".repeat(64))),
            "presenceEvidence",
            Some("passkey"),
        ),
        (
            "an unknown algorithm",
            signed_with(|s| s["predicate"]["presenceEvidence"]["algorithm"] = json!(-35)),
            "presenceEvidence",
            Some("algorithm"),
        ),
        (
            "an unknown evidence type",
            signed_with(|s| s["predicate"]["presenceEvidence"]["type"] = json!("u2f")),
            "presenceEvidence",
            Some("webauthn"),
        ),
        (
            "a change id that is not hex",
            signed_with(|s| {
                s["predicate"]["presenceEvidence"]["approves"]["change"] =
                    json!("kpyrvsoowosvsolxntkowlymwouwxqtx");
            }),
            "presenceEvidence",
            Some("approves.change"),
        ),
        (
            "an agent's key",
            signed_with(|s| s["predicate"]["actor"] = json!("agent")),
            "presence",
            Some("human"),
        ),
        (
            "a presence this reader does not know, its evidence kept",
            signed_with(|s| s["predicate"]["presence"] = json!("user_present")),
            "presence",
            Some("not one this reader knows"),
        ),
    ];
    for (case, envelope, field, message) in cases {
        let error = check_certificate(&envelope, &known).unwrap_err();
        assert_eq!(error.field(), Some(field), "{case}: {error}");
        if let Some(message) = message {
            assert!(error.to_string().contains(message), "{case}: {error}");
        }
    }
    // An unknown field inside the evidence is ignored, as in the predicate.
    let future = signed_with(|s| s["predicate"]["presenceEvidence"]["futureField"] = json!(1));
    assert_eq!(
        check_certificate(&future, &known).unwrap().binding,
        binding(chromium_evidence())
    );
}

// 2: the authority never signs what a reader refuses.
#[test]
fn certify_refuses_what_check_would() {
    let authority = authority_key();
    let mut altered = chromium_evidence();
    altered.approves.change = "f".repeat(32);
    let error = certify(&authority, &binding(altered)).unwrap_err();
    assert_eq!(error.field(), Some("presenceEvidence"), "{error}");
    let mut agent = binding(chromium_evidence());
    agent.actor = Actor::Agent;
    assert_eq!(
        certify(&authority, &agent).unwrap_err().field(),
        Some("presence")
    );
    // Evidence that passes its checks but makes the certificate larger than
    // a reader accepts.
    let mut authenticator = Authenticator::new(Algorithm::EdDsa);
    let challenge = challenge_for(&approves());
    let valid = authenticator.assert(RP, ORIGIN, &challenge);
    let mut padded: Value = serde_json::from_slice(&valid.client_data_json).unwrap();
    padded["padding"] = json!("x".repeat(64 * 1024));
    let client_data_json = serde_json::to_vec(&padded).unwrap();
    let large = PresenceEvidence {
        passkey: authenticator.passkey().clone(),
        credential_id: authenticator.credential_id().to_vec(),
        rp_id: RP.into(),
        approves: approves(),
        assertion: Assertion {
            signature: authenticator.sign_assertion(&valid.authenticator_data, &client_data_json),
            authenticator_data: valid.authenticator_data,
            client_data_json,
        },
    };
    assert!(matches!(
        certify(&authority, &binding(large)),
        Err(Invalid::Size(_))
    ));
}
