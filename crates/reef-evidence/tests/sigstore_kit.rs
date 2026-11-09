//! R3.42 tests 2 and 3: bundles built by a local Sigstore (`reef-testing`'s
//! kit: a Fulcio CA, a CT log and a Rekor log, nothing skipped) check back
//! to their fields and bind for `status_of`, and each broken check is
//! refused at its step.
#![cfg(feature = "sigstore")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    reason = "tests: a panic is the failure report, and a refusal's text is printed"
)]

use reef_evidence::session::{
    Actor, IdentityKind, IdentityRef, IdentityStatus, IssuerKind, Revocations, status_of,
};
use reef_evidence::sigstore::{
    BINDING_PREDICATE_TYPE, Step, TrustedRoot, binding_payload, check_sigstore_bundle,
};
use reef_evidence::{KeyId, SigningKey};
use reef_testing::sigstore::{Entry, Leaf, San, TestSigstore};
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _};

const ISSUER: &str = "https://token.actions.githubusercontent.com";
const SUBJECT: &str =
    "https://github.com/reef-vcs/reef/.github/workflows/checks.yml@refs/heads/main";

fn new_key() -> SigningKey {
    let (pem, _) = SigningKey::generate().unwrap();
    SigningKey::from_pkcs8_pem(&pem).unwrap()
}

fn leaf() -> Leaf {
    Leaf::new(ISSUER, San::Uri(SUBJECT.into()))
}

fn payload(key: &SigningKey) -> Vec<u8> {
    binding_payload(
        key.public_key().id(),
        Actor::Executor,
        Some("ci-7"),
        "ses_0123456789abcdef",
    )
}

/// A bundle for `key` with `payload`, `leaf` and `entry`, from `kit`.
fn bundle(
    kit: &TestSigstore,
    key: &SigningKey,
    payload: &[u8],
    leaf: &Leaf,
    entry: Entry,
) -> String {
    kit.keyless_bundle(
        key.public_key().spki(),
        &|message| key.sign_bytes(message),
        payload,
        leaf,
        entry,
    )
}

fn root(kit: &TestSigstore) -> TrustedRoot {
    TrustedRoot::from_json(kit.trusted_root().as_bytes(), "the test kit").unwrap()
}

fn step(kit: &TestSigstore, bundle: &str) -> Step {
    let error = check_sigstore_bundle(bundle.as_bytes(), &root(kit)).unwrap_err();
    eprintln!("{error}");
    error.step
}

#[test]
fn check_and_bind() {
    let kit = TestSigstore::new();
    let key = new_key();
    let bundle = bundle(&kit, &key, &payload(&key), &leaf(), Entry::default());
    let certified = check_sigstore_bundle(bundle.as_bytes(), &root(&kit)).unwrap();
    assert_eq!(certified.public_key, *key.public_key());
    assert_eq!(certified.actor, Actor::Executor);
    assert_eq!(certified.agent.as_deref(), Some("ci-7"));
    assert_eq!(certified.session, "ses_0123456789abcdef");
    assert_eq!(certified.oidc_issuer, ISSUER);
    assert_eq!(certified.oidc_subject, SUBJECT);
    assert_eq!(certified.not_after_ms - certified.not_before_ms, 600_000);
    assert_eq!(certified.ca.host, "fulcio.test");
    assert_eq!(certified.ca.algorithm, "ecdsa-p256");
    assert_eq!(certified.id, reef_evidence::sha256_hex(bundle.as_bytes()));
    assert_eq!(
        certified
            .ci_claims
            .get("source_repository_uri")
            .map(String::as_str),
        Some("https://github.com/reef-vcs/reef")
    );
    let binding = certified.bind(IdentityRef {
        id: "idn_0123456789abcdef".into(),
        name: "ci-main".into(),
        kind: IdentityKind::Ci,
    });
    assert_eq!(binding.issuer.kind, IssuerKind::Sigstore);
    assert_eq!(binding.issuer.key_id, certified.ca.key_id);
    let none = Revocations::default();
    for (at, expect) in [
        (binding.not_before_ms, IdentityStatus::Bound),
        (binding.not_after_ms, IdentityStatus::Bound),
        (binding.not_after_ms + 1, IdentityStatus::Expired),
        (binding.not_before_ms - 1, IdentityStatus::Expired),
    ] {
        assert_eq!(status_of(&binding, None, at, &none), expect);
    }
    // A revoked binding (the OIDC match) applies like any revocation.
    let revoked = Revocations {
        binding: Some(reef_evidence::session::Revocation {
            revoked_at_ms: binding.not_before_ms + 10,
            invalid_from_ms: binding.not_before_ms + 5,
            reason: None,
        }),
        ..Revocations::default()
    };
    assert_eq!(
        status_of(&binding, None, binding.not_before_ms + 5, &revoked),
        IdentityStatus::Revoked
    );
    // An email subject (a person's login) reads back too.
    let mut person = leaf();
    person.issuer = "https://accounts.google.com".into();
    person.subject = San::Email("alice@example.com".into());
    let other = bundle_for(&kit, &person);
    let certified = check_sigstore_bundle(other.as_bytes(), &root(&kit)).unwrap();
    assert_eq!(certified.oidc_subject, "alice@example.com");
    assert_eq!(certified.oidc_issuer, "https://accounts.google.com");
}

fn stranger_root_tlogs(stranger: &TestSigstore) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(&stranger.trusted_root()).unwrap()["tlogs"].clone()
}

fn bundle_for(kit: &TestSigstore, leaf: &Leaf) -> String {
    let key = new_key();
    bundle(kit, &key, &payload(&key), leaf, Entry::default())
}

#[test]
#[expect(clippy::too_many_lines, reason = "one refusal per check, in order")]
fn refusals() {
    let kit = TestSigstore::new();
    let key = new_key();
    let good = payload(&key);
    // A chain to a CA the root does not hold.
    let stranger = TestSigstore::new();
    let foreign = bundle(&stranger, &key, &good, &leaf(), Entry::default());
    // Its log is unknown too, and the signing time is established first.
    assert!(matches!(step(&kit, &foreign), Step::Tlog | Step::Chain));
    // The same CA with an unknown log: the log is refused.
    let mut root_without_log: serde_json::Value =
        serde_json::from_str(&kit.trusted_root()).unwrap();
    root_without_log["tlogs"] = stranger_root_tlogs(&stranger);
    let mixed = TrustedRoot::from_json(root_without_log.to_string().as_bytes(), "mixed").unwrap();
    let local = bundle(&kit, &key, &good, &leaf(), Entry::default());
    assert_eq!(
        check_sigstore_bundle(local.as_bytes(), &mixed)
            .unwrap_err()
            .step,
        Step::Tlog
    );
    // The same log with an unknown CA: the chain is refused.
    let mut root_without_ca: serde_json::Value = serde_json::from_str(&kit.trusted_root()).unwrap();
    root_without_ca["certificateAuthorities"] = serde_json::from_str::<serde_json::Value>(
        &stranger.trusted_root(),
    )
    .unwrap()["certificateAuthorities"]
        .clone();
    let other_ca =
        TrustedRoot::from_json(root_without_ca.to_string().as_bytes(), "other ca").unwrap();
    assert_eq!(
        check_sigstore_bundle(local.as_bytes(), &other_ca)
            .unwrap_err()
            .step,
        Step::Chain
    );
    // A bad or missing SCT.
    let mut bad_sct = leaf();
    bad_sct.good_sct = false;
    assert_eq!(
        step(
            &kit,
            &bundle(&kit, &new_key(), &good, &bad_sct, Entry::default())
        ),
        Step::Sct
    );
    let mut no_sct = leaf();
    no_sct.with_sct = false;
    assert_eq!(step(&kit, &bundle_for(&kit, &no_sct)), Step::Sct);
    // The log entry: its proof, checkpoint and SET.
    for entry in [
        Entry {
            bad_proof: true,
            ..Entry::default()
        },
        Entry {
            bad_checkpoint: true,
            ..Entry::default()
        },
        Entry {
            bad_set: true,
            ..Entry::default()
        },
    ] {
        let key = new_key();
        assert_eq!(
            step(&kit, &bundle(&kit, &key, &payload(&key), &leaf(), entry)),
            Step::Tlog,
            "{entry:?}"
        );
    }
    // Logged before the leaf was valid.
    let early = leaf();
    let entry = Entry {
        integrated_time: Some(early.not_before - 120),
        ..Entry::default()
    };
    let key2 = new_key();
    assert_eq!(
        step(&kit, &bundle(&kit, &key2, &payload(&key2), &early, entry)),
        Step::Time
    );
    // The envelope signed by another key than the certified one.
    let signer = new_key();
    let certified = new_key();
    let swapped = kit.keyless_bundle(
        certified.public_key().spki(),
        &|message| signer.sign_bytes(message),
        &payload(&certified),
        &leaf(),
        Entry::default(),
    );
    assert_eq!(step(&kit, &swapped), Step::Signature);
    // A subject that is not the certified key's id.
    let other_id = KeyId::of(b"another key");
    let wrong_subject = binding_payload(&other_id, Actor::Executor, None, "ses_0123456789abcdef");
    let key3 = new_key();
    assert_eq!(
        step(
            &kit,
            &bundle(&kit, &key3, &wrong_subject, &leaf(), Entry::default())
        ),
        Step::Key
    );
    // A payload that is not the binding statement.
    let key4 = new_key();
    let statement = |predicate_type: &str, predicate: &str| {
        format!(
            r#"{{"_type":"https://in-toto.io/Statement/v1","subject":[{{"name":"session-key","digest":{{"sha256":"{}"}}}}],"predicateType":"{predicate_type}","predicate":{predicate}}}"#,
            key4.public_key().id()
        )
        .into_bytes()
    };
    let other_type = statement("urn:reef-vcs:evidence:v1", "{}");
    assert_eq!(
        step(
            &kit,
            &bundle(&kit, &key4, &other_type, &leaf(), Entry::default())
        ),
        Step::Statement
    );
    for predicate in [
        r#"{"actor":"executor","session":"ses_0123456789abcdef","presence":"user_verified","custody":"client"}"#,
        r#"{"actor":"executor","session":"ses_0123456789abcdef","presence":"none","custody":"daemon"}"#,
        r#"{"actor":"robot","session":"ses_0123456789abcdef","presence":"none","custody":"client"}"#,
        r#"{"actor":"executor","session":"ses_x","presence":"none","custody":"client"}"#,
    ] {
        let payload = statement(BINDING_PREDICATE_TYPE, predicate);
        assert_eq!(
            step(
                &kit,
                &bundle(&kit, &key4, &payload, &leaf(), Entry::default())
            ),
            Step::Statement,
            "{predicate}"
        );
    }
    let duplicate = statement(
        BINDING_PREDICATE_TYPE,
        r#"{"actor":"executor","actor":"human","session":"ses_0123456789abcdef","presence":"none","custody":"client"}"#,
    );
    let duplicate_step = step(
        &kit,
        &bundle(&kit, &key4, &duplicate, &leaf(), Entry::default()),
    );
    assert!(
        matches!(duplicate_step, Step::Statement | Step::Json),
        "{duplicate_step}"
    );
    // A P-256 leaf: verifies as Sigstore, refused as Reef's key.
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
    let p256 =
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).unwrap();
    let mut spki = vec![
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ];
    spki.extend_from_slice(p256.public_key().as_ref());
    let p256_payload = binding_payload(
        &KeyId::of(&spki),
        Actor::Executor,
        None,
        "ses_0123456789abcdef",
    );
    let p256_bundle = kit.keyless_bundle(
        &spki,
        &|message| p256.sign(&rng, message).unwrap().as_ref().to_vec(),
        &p256_payload,
        &leaf(),
        Entry::default(),
    );
    assert_eq!(step(&kit, &p256_bundle), Step::Key);
    // Not a bundle.
    assert_eq!(step(&kit, "{}"), Step::Json);
    assert_eq!(step(&kit, "not json"), Step::Json);
}
