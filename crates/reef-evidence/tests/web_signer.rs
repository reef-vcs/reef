//! The web UI's signer (R3.43 test 3): `web/src/sign/` writes the proof of
//! possession, the in-toto statement, the DSSE envelope and the passkey
//! challenges byte for byte as this crate does. `tests/fixtures/web-signer.json`
//! holds this crate's bytes for a fixed key; `web/src/sign/sign.test.ts`
//! checks the TypeScript against the same file. Here each stored value is
//! checked by this crate's own verifiers, and `fixture_is_current` fails when
//! the file drifts from this crate; `REEF_UPDATE_FIXTURES=1` rewrites it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests: a panic is the failure report"
)]

use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use reef_evidence::session::{Actor, ProofFor, sign_proof, verify_proof};
use reef_evidence::webauthn::{
    ApprovalFor, Approves, PasskeyChange, approval_challenge, passkey_change_challenge,
};
use reef_evidence::{
    Claim, KeyId, Outcome, PAYLOAD_TYPE, PublicKey, SigningKey, TreeDigest, Verified,
};
use serde_json::{Map, Value, json};

const SEED: [u8; 32] = [0x5e; 32];
const AUTHORITY_SEED: [u8; 32] = [0xa1; 32];
const IDENTITY: &str = "alice";
const IDENTITY_ID: &str = "idn_8f3a0c2d91e4b675";
const SESSION: &str = "ses_41c9e07ab2d35f18";
const TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const CHANGE: &str = "8d1f0e7c6b5a49382716f5e4d3c2b1a0";
const PASSKEY: &str = "pk_0123456789abcdef";
/// 2026-11-19T19:07:46.105Z, a time with milliseconds.
const STARTED_MS: u64 = 1_795_115_266_105;

fn path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/web-signer.json")
}

fn key() -> SigningKey {
    SigningKey::from_seed(&SEED).unwrap()
}

fn authority() -> KeyId {
    SigningKey::from_seed(&AUTHORITY_SEED)
        .unwrap()
        .public_key()
        .id()
        .clone()
}

/// Sign off's claim, with no target, and one whose target needs escaping.
fn claims() -> [Claim; 2] {
    let at = |ms| UNIX_EPOCH + Duration::from_millis(ms);
    let claim = |target: Option<&str>| Claim {
        kind: "signoff".to_owned(),
        result: Outcome::Pass,
        target: target.map(str::to_owned),
        started_on: at(STARTED_MS),
        finished_on: at(STARTED_MS + 1),
        artifacts: Vec::new(),
        annotations: Map::new(),
    };
    [claim(None), claim(Some("web/src/sign \"ü\" \\ ✓"))]
}

fn approves(statement: &[u8]) -> Approves {
    Approves {
        statement: reef_evidence::sha256_hex(statement),
        change: CHANGE.to_owned(),
    }
}

/// The fixture, as this crate writes each value.
fn fixture() -> Value {
    let key = key();
    let authority = authority();
    let tree = TreeDigest::git(TREE).unwrap();
    let proof = sign_proof(
        &key,
        &ProofFor {
            authority: &authority,
            identity: IDENTITY,
            actor: Actor::Human,
            session: "",
        },
    );
    let statements: Vec<Value> = claims()
        .iter()
        .map(|claim| {
            let payload = claim.payload(&tree).unwrap();
            json!({
                "target": claim.target,
                "payload": String::from_utf8(payload.clone()).unwrap(),
                "sha256": reef_evidence::sha256_hex(&payload),
                "envelope": String::from_utf8(key.sign(PAYLOAD_TYPE, &payload).to_bytes()).unwrap(),
            })
        })
        .collect();
    let first = claims()[0].payload(&tree).unwrap();
    let challenge = approval_challenge(&ApprovalFor {
        authority: &authority,
        key: key.public_key().id(),
        identity: IDENTITY_ID,
        session: SESSION,
        approves: &approves(&first),
    });
    let removal = passkey_change_challenge(PasskeyChange::Remove { passkey: PASSKEY });
    json!({
        "inputs": {
            "seed": STANDARD.encode(SEED),
            "authority": authority.as_str(),
            "identity": IDENTITY,
            "identityId": IDENTITY_ID,
            "session": SESSION,
            "tree": TREE,
            "kind": "signoff",
            "result": "pass",
            "startedMs": STARTED_MS,
            "finishedMs": STARTED_MS + 1,
            "change": CHANGE,
            "passkey": PASSKEY,
        },
        "publicKey": STANDARD.encode(key.public_key().spki()),
        "keyId": key.public_key().id().as_str(),
        "proof": STANDARD.encode(proof),
        "statements": statements,
        "approvalChallenge": URL_SAFE_NO_PAD.encode(challenge),
        "removalChallenge": URL_SAFE_NO_PAD.encode(removal),
    })
}

fn stored() -> Value {
    let bytes = std::fs::read(path()).unwrap_or_else(|error| {
        panic!(
            "{}: {error}; run with REEF_UPDATE_FIXTURES=1",
            path().display()
        )
    });
    serde_json::from_slice(&bytes).unwrap()
}

fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value[field]
        .as_str()
        .unwrap_or_else(|| panic!("{field} is not a string"))
}

#[test]
fn fixture_is_current() {
    let made = fixture();
    if std::env::var_os("REEF_UPDATE_FIXTURES").is_some() {
        let mut bytes = serde_json::to_vec_pretty(&made).unwrap();
        bytes.push(b'\n');
        std::fs::write(path(), bytes).unwrap();
    }
    assert!(
        stored() == made,
        "web-signer.json is stale; run with REEF_UPDATE_FIXTURES=1"
    );
}

#[test]
fn the_proof_verifies() {
    let stored = stored();
    let public =
        PublicKey::from_spki(&STANDARD.decode(text(&stored, "publicKey")).unwrap()).unwrap();
    assert_eq!(public.id().as_str(), text(&stored, "keyId"));
    let proof = STANDARD.decode(text(&stored, "proof")).unwrap();
    let authority = authority();
    let mut proof_for = ProofFor {
        authority: &authority,
        identity: IDENTITY,
        actor: Actor::Human,
        session: "",
    };
    assert!(verify_proof(&public, &proof_for, &proof));
    proof_for.identity = "";
    assert!(
        !verify_proof(&public, &proof_for, &proof),
        "the proof binds the identity as sent"
    );
}

#[test]
fn statements_are_claim_payloads_and_envelopes_verify() {
    let stored = stored();
    let spki = STANDARD.decode(text(&stored, "publicKey")).unwrap();
    let tree = TreeDigest::git(TREE).unwrap();
    let statements = stored["statements"].as_array().unwrap();
    assert_eq!(statements.len(), claims().len());
    for (claim, statement) in claims().iter().zip(statements) {
        let payload = text(statement, "payload").as_bytes();
        assert_eq!(payload, claim.payload(&tree).unwrap().as_slice());
        assert_eq!(
            text(statement, "sha256"),
            reef_evidence::sha256_hex(payload)
        );
        let envelope = text(statement, "envelope").as_bytes();
        let verified = Verified::check(envelope, &spki).unwrap();
        assert_eq!(verified.payload, payload);
        assert_eq!(verified.canonical, envelope, "the envelope is canonical");
    }
}

#[test]
fn challenges_are_the_derived_ones() {
    let stored = stored();
    let key = key();
    let first = text(&stored["statements"][0], "payload").as_bytes();
    let approval = approval_challenge(&ApprovalFor {
        authority: &authority(),
        key: key.public_key().id(),
        identity: IDENTITY_ID,
        session: SESSION,
        approves: &approves(first),
    });
    assert_eq!(
        URL_SAFE_NO_PAD
            .decode(text(&stored, "approvalChallenge"))
            .unwrap(),
        approval
    );
    let removal = passkey_change_challenge(PasskeyChange::Remove { passkey: PASSKEY });
    assert_eq!(
        URL_SAFE_NO_PAD
            .decode(text(&stored, "removalChallenge"))
            .unwrap(),
        removal
    );
}
