//! A local Sigstore for tests (R3.42): a Fulcio CA, a CT log and a Rekor
//! log, whose keys live in memory, that build **complete** keyless bundles
//! (a leaf with a real RFC 6962 SCT, a Rekor v1 `dsse` entry with its SET,
//! inclusion proof and signed checkpoint) and the `trusted_root.json` that
//! verifies them, with knobs that break one check at a time. Reef's
//! verifier never skips a check, so nothing here does either.
//!
//! [`fake`] serves the same over HTTP on loopback: Fulcio's
//! `/api/v2/signingCert`, Rekor's `/api/v1/log/entries`, and a GitHub
//! Actions OIDC token endpoint, for the signing client's tests.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test fixture: a panic is the failure report"
)]

pub mod fake;

use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use der::{Decode as _, Encode as _};
use rcgen::{
    BasicConstraints, CertificateParams, CustomExtension, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
    PKCS_ED25519, PublicKeyData, SanType, SerialNumber, SignatureAlgorithm,
};
use ring::digest::{SHA256, digest};
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _};
use serde_json::{Value, json};
use time::OffsetDateTime;

/// Fulcio's OIDC issuer extension (V2, a DER `UTF8String`).
const OID_ISSUER_V2: &[u64] = &[1, 3, 6, 1, 4, 1, 57264, 1, 8];
/// Fulcio's deprecated OIDC issuer extension (the raw string).
const OID_ISSUER_V1: &[u64] = &[1, 3, 6, 1, 4, 1, 57264, 1, 1];
/// Fulcio's source repository URI claim.
const OID_SOURCE_REPOSITORY_URI: &[u64] = &[1, 3, 6, 1, 4, 1, 57264, 1, 12];
/// RFC 6962's embedded SCT list.
const OID_SCT_LIST: &[u64] = &[1, 3, 6, 1, 4, 1, 11129, 2, 4, 2];
/// The DSSE payload type of an in-toto statement.
pub const IN_TOTO: &str = "application/vnd.in-toto+json";

/// A certificate's subject: an email (a person's login) or a URI (a CI
/// workflow).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum San {
    /// An `rfc822Name`.
    Email(String),
    /// A `uniformResourceIdentifier`.
    Uri(String),
}

impl San {
    /// The value.
    pub fn as_str(&self) -> &str {
        match self {
            San::Email(value) | San::Uri(value) => value,
        }
    }
}

/// What a leaf certificate says, and how it may be broken.
#[derive(Clone, Debug)]
pub struct Leaf {
    /// The OIDC issuer.
    pub issuer: String,
    /// The subject.
    pub subject: San,
    /// `notBefore`, unix seconds.
    pub not_before: i64,
    /// Its lifetime in seconds (Fulcio's is 600).
    pub lifetime: i64,
    /// An SCT that verifies (`true`), or one signed by another key.
    pub good_sct: bool,
    /// Whether it carries an SCT at all.
    pub with_sct: bool,
}

impl Leaf {
    /// A leaf for `issuer` and `subject`, valid from 30 seconds ago for
    /// Fulcio's 10 minutes, with a good SCT.
    pub fn new(issuer: &str, subject: San) -> Leaf {
        Leaf {
            issuer: issuer.to_owned(),
            subject,
            not_before: now() - 30,
            lifetime: 600,
            good_sct: true,
            with_sct: true,
        }
    }
}

/// How a log entry may be broken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// The integrated time, unix seconds; `None` for now.
    pub integrated_time: Option<i64>,
    /// A SET signed by another key.
    pub bad_set: bool,
    /// A checkpoint signed by another key.
    pub bad_checkpoint: bool,
    /// An inclusion proof whose root is not the leaf's.
    pub bad_proof: bool,
}

/// Unix seconds now.
pub fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// The SHA-256 of `bytes`.
fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0; 32];
    out.copy_from_slice(digest(&SHA256, bytes).as_ref());
    out
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, b| {
        let _ = write!(text, "{b:02x}");
        text
    })
}

/// A P-256 key with its SPKI, for the CT log and Rekor.
struct LogKey {
    pair: EcdsaKeyPair,
    spki: Vec<u8>,
}

impl LogKey {
    fn generate(rng: &SystemRandom) -> LogKey {
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, rng)
            .expect("a P-256 key");
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), rng)
            .expect("the P-256 key parses");
        // SEQUENCE { SEQUENCE { id-ecPublicKey, prime256v1 }, BIT STRING { point } }
        let mut spki = vec![
            0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06,
            0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
        ];
        spki.extend_from_slice(pair.public_key().as_ref());
        LogKey { pair, spki }
    }

    fn sign(&self, rng: &SystemRandom, message: &[u8]) -> Vec<u8> {
        self.pair
            .sign(rng, message)
            .expect("P-256 signs")
            .as_ref()
            .to_vec()
    }

    fn id(&self) -> [u8; 32] {
        sha256(&self.spki)
    }
}

/// A local Sigstore: a Fulcio CA, a CT log and a Rekor log.
pub struct TestSigstore {
    rng: SystemRandom,
    ca_key: KeyPair,
    ca_params: CertificateParams,
    ca_der: Vec<u8>,
    ct: LogKey,
    rekor: LogKey,
    stranger: LogKey,
    next_index: AtomicU64,
    next_serial: AtomicU64,
}

impl std::fmt::Debug for TestSigstore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestSigstore").finish_non_exhaustive()
    }
}

impl Default for TestSigstore {
    fn default() -> Self {
        TestSigstore::new()
    }
}

impl TestSigstore {
    /// A new one, with fresh keys.
    pub fn new() -> TestSigstore {
        let rng = SystemRandom::new();
        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a CA key");
        let mut ca_params = CertificateParams::default();
        let mut name = DistinguishedName::new();
        name.push(DnType::OrganizationName, "reef-testing");
        name.push(DnType::CommonName, "test-fulcio");
        ca_params.distinguished_name = name;
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        ca_params.not_before = OffsetDateTime::from_unix_timestamp(1_600_000_000).expect("2020");
        ca_params.not_after = OffsetDateTime::from_unix_timestamp(4_102_444_800).expect("2100");
        ca_params.serial_number = Some(SerialNumber::from(vec![1]));
        let ca_der = ca_params
            .self_signed(&ca_key)
            .expect("the CA certificate")
            .der()
            .to_vec();
        TestSigstore {
            ct: LogKey::generate(&rng),
            rekor: LogKey::generate(&rng),
            stranger: LogKey::generate(&rng),
            rng,
            ca_key,
            ca_params,
            ca_der,
            next_index: AtomicU64::new(1),
            next_serial: AtomicU64::new(2),
        }
    }

    /// The CA certificate's DER.
    pub fn ca_der(&self) -> &[u8] {
        &self.ca_der
    }

    /// The `trusted_root.json` that verifies this Sigstore's bundles.
    pub fn trusted_root(&self) -> String {
        let log = |key: &LogKey, url: &str| {
            json!({
                "baseUrl": url,
                "hashAlgorithm": "SHA2_256",
                "publicKey": {
                    "rawBytes": B64.encode(&key.spki),
                    "keyDetails": "PKIX_ECDSA_P256_SHA_256",
                    "validFor": {"start": "2020-01-01T00:00:00Z"}
                },
                "logId": {"keyId": B64.encode(key.id())}
            })
        };
        json!({
            "mediaType": "application/vnd.dev.sigstore.trustedroot+json;version=0.1",
            "tlogs": [log(&self.rekor, "https://rekor.test")],
            "certificateAuthorities": [{
                "subject": {"organization": "reef-testing", "commonName": "test-fulcio"},
                "uri": "https://fulcio.test",
                "certChain": {"certificates": [{"rawBytes": B64.encode(&self.ca_der)}]},
                "validFor": {"start": "2020-01-01T00:00:00Z"}
            }],
            "ctlogs": [log(&self.ct, "https://ctfe.test")],
            "timestampAuthorities": []
        })
        .to_string()
    }

    /// A leaf certificate for the key `spki` (a DER
    /// `SubjectPublicKeyInfo`), as Fulcio would issue it.
    pub fn certify(&self, spki: &[u8], leaf: &Leaf) -> Vec<u8> {
        let key = SubjectKey::of(spki);
        let issuer = Issuer::from_params(&self.ca_params, &self.ca_key);
        let serial = self.next_serial.fetch_add(1, Ordering::Relaxed);
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.serial_number = Some(SerialNumber::from(serial.to_be_bytes().to_vec()));
        params.not_before =
            OffsetDateTime::from_unix_timestamp(leaf.not_before).expect("notBefore");
        params.not_after =
            OffsetDateTime::from_unix_timestamp(leaf.not_before + leaf.lifetime).expect("notAfter");
        params.subject_alt_names = vec![match &leaf.subject {
            San::Email(email) => SanType::Rfc822Name(email.clone().try_into().expect("ASCII")),
            San::Uri(uri) => SanType::URI(uri.clone().try_into().expect("ASCII")),
        }];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::CodeSigning];
        params.use_authority_key_identifier_extension = true;
        params.custom_extensions = vec![
            CustomExtension::from_oid_content(OID_ISSUER_V1, leaf.issuer.as_bytes().to_vec()),
            CustomExtension::from_oid_content(OID_ISSUER_V2, utf8_string(&leaf.issuer)),
            CustomExtension::from_oid_content(
                OID_SOURCE_REPOSITORY_URI,
                utf8_string("https://github.com/reef-vcs/reef"),
            ),
        ];
        let precert = params
            .signed_by(&key, &issuer)
            .expect("the precertificate")
            .der()
            .to_vec();
        if !leaf.with_sct {
            return precert;
        }
        let sct = self.sct(&precert, leaf.good_sct);
        params
            .custom_extensions
            .push(CustomExtension::from_oid_content(OID_SCT_LIST, sct));
        params
            .signed_by(&key, &issuer)
            .expect("the certificate")
            .der()
            .to_vec()
    }

    /// The SCT list extension's content for a certificate whose TBS, without
    /// the SCT list, is `precert`'s: an OCTET STRING of the TLS list.
    fn sct(&self, precert: &[u8], good: bool) -> Vec<u8> {
        let parsed = x509_cert::Certificate::from_der(precert).expect("the precertificate parses");
        let tbs = parsed.tbs_certificate.to_der().expect("its TBS encodes");
        let issuer_key_hash = sha256(&ca_spki(&self.ca_der));
        let timestamp = u64::try_from(now() * 1000).expect("after 1970");
        let mut signed = vec![0, 0]; // v1, certificate_timestamp
        signed.extend_from_slice(&timestamp.to_be_bytes());
        signed.extend_from_slice(&1u16.to_be_bytes()); // precert_entry
        signed.extend_from_slice(&issuer_key_hash);
        let len = u32::try_from(tbs.len()).expect("small");
        signed.extend_from_slice(&len.to_be_bytes()[1..]); // u24
        signed.extend_from_slice(&tbs);
        signed.extend_from_slice(&0u16.to_be_bytes()); // no extensions
        let log_key = if good { &self.ct } else { &self.stranger };
        let signature = log_key.sign(&self.rng, &signed);
        let mut sct = vec![0]; // v1
        sct.extend_from_slice(&self.ct.id());
        sct.extend_from_slice(&timestamp.to_be_bytes());
        sct.extend_from_slice(&0u16.to_be_bytes());
        sct.extend_from_slice(&[4, 3]); // sha256, ecdsa
        sct.extend_from_slice(&u16::try_from(signature.len()).expect("small").to_be_bytes());
        sct.extend_from_slice(&signature);
        let mut list = Vec::new();
        let sct_len = u16::try_from(sct.len()).expect("small");
        list.extend_from_slice(&(sct_len + 2).to_be_bytes());
        list.extend_from_slice(&sct_len.to_be_bytes());
        list.extend_from_slice(&sct);
        der::asn1::OctetString::new(list)
            .expect("an octet string")
            .to_der()
            .expect("it encodes")
    }

    /// Logs a DSSE `envelope` (its JSON) signed by `leaf`'s key, as Rekor v1
    /// does for a `dsse` 0.0.1 entry; the response's one entry, as Rekor
    /// returns it (`{uuid: {body, integratedTime, logID, logIndex,
    /// verification}}`).
    pub fn log(&self, envelope: &str, leaf: &[u8], entry: Entry) -> Value {
        let parsed: Value = serde_json::from_str(envelope).expect("an envelope");
        let payload = B64
            .decode(parsed["payload"].as_str().expect("a payload"))
            .expect("base64");
        let signature = parsed["signatures"][0]["sig"]
            .as_str()
            .expect("a signature")
            .to_owned();
        let pem = pem("CERTIFICATE", leaf);
        let body = json!({
            "apiVersion": "0.0.1",
            "kind": "dsse",
            "spec": {
                "envelopeHash": {"algorithm": "sha256", "value": hex(&sha256(envelope.as_bytes()))},
                "payloadHash": {"algorithm": "sha256", "value": hex(&sha256(&payload))},
                "signatures": [{"signature": signature, "verifier": B64.encode(pem)}]
            }
        });
        let body = B64.encode(body.to_string());
        let index = self.next_index.fetch_add(1, Ordering::Relaxed);
        let integrated_time = entry.integrated_time.unwrap_or_else(now);
        let log_id = hex(&self.rekor.id());
        let set_payload = json!({
            "body": body,
            "integratedTime": integrated_time,
            "logID": log_id,
            "logIndex": index,
        })
        .to_string();
        let set_key = if entry.bad_set {
            &self.stranger
        } else {
            &self.rekor
        };
        let set = set_key.sign(&self.rng, set_payload.as_bytes());
        // A tree of one leaf: its root is the leaf's hash.
        let mut leaf_input = vec![0];
        leaf_input.extend_from_slice(&B64.decode(&body).expect("base64"));
        let mut root = sha256(&leaf_input);
        if entry.bad_proof {
            root[0] ^= 1;
        }
        let note = format!("rekor.test - 1\n1\n{}\n", B64.encode(root));
        let checkpoint_key = if entry.bad_checkpoint {
            &self.stranger
        } else {
            &self.rekor
        };
        let mut signature = self.rekor.id()[..4].to_vec();
        signature.extend_from_slice(&checkpoint_key.sign(&self.rng, note.as_bytes()));
        let checkpoint = format!("{note}\n\u{2014} rekor.test {}\n", B64.encode(signature));
        json!({
            hex(&sha256(body.as_bytes())): {
                "body": body,
                "integratedTime": integrated_time,
                "logID": log_id,
                "logIndex": index,
                "verification": {
                    "signedEntryTimestamp": B64.encode(set),
                    "inclusionProof": {
                        "checkpoint": checkpoint,
                        "hashes": [],
                        "logIndex": 0,
                        "rootHash": hex(&root),
                        "treeSize": 1
                    }
                }
            }
        })
    }

    /// A Sigstore bundle v0.3 of `envelope` (its JSON), certified by `leaf`
    /// and logged as `rekor` (a [`TestSigstore::log`] response).
    pub fn bundle(leaf: &[u8], rekor: &Value, envelope: &str) -> String {
        let (_, entry) = rekor
            .as_object()
            .and_then(|map| map.iter().next())
            .expect("one entry");
        let envelope: Value = serde_json::from_str(envelope).expect("an envelope");
        let proof = &entry["verification"]["inclusionProof"];
        let hex_b64 = |text: &Value| B64.encode(decode_hex(text.as_str().expect("hex")));
        json!({
            "mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json",
            "verificationMaterial": {
                "certificate": {"rawBytes": B64.encode(leaf)},
                "tlogEntries": [{
                    "logIndex": entry["logIndex"].to_string(),
                    "logId": {"keyId": hex_b64(&entry["logID"])},
                    "kindVersion": {"kind": "dsse", "version": "0.0.1"},
                    "integratedTime": entry["integratedTime"].to_string(),
                    "inclusionPromise": {"signedEntryTimestamp": entry["verification"]["signedEntryTimestamp"]},
                    "inclusionProof": {
                        "logIndex": proof["logIndex"].to_string(),
                        "rootHash": hex_b64(&proof["rootHash"]),
                        "treeSize": proof["treeSize"].to_string(),
                        "hashes": proof["hashes"].as_array().expect("hashes").iter().map(hex_b64).collect::<Vec<_>>(),
                        "checkpoint": {"envelope": proof["checkpoint"]}
                    },
                    "canonicalizedBody": entry["body"]
                }]
            },
            "dsseEnvelope": envelope
        })
        .to_string()
    }

    /// The whole flow for a key: a leaf, the envelope of `payload` signed
    /// by `sign` (over the DSSE PAE), its log entry, and the bundle.
    pub fn keyless_bundle(
        &self,
        spki: &[u8],
        sign: &dyn Fn(&[u8]) -> Vec<u8>,
        payload: &[u8],
        leaf: &Leaf,
        entry: Entry,
    ) -> String {
        let certificate = self.certify(spki, leaf);
        let envelope = envelope(payload, sign, &hex(&sha256(spki)));
        let rekor = self.log(&envelope, &certificate, entry);
        TestSigstore::bundle(&certificate, &rekor, &envelope)
    }
}

/// A leaf's key as rcgen takes it: an ed25519 or P-256 SPKI's key bits.
struct SubjectKey {
    bits: Vec<u8>,
    algorithm: &'static SignatureAlgorithm,
}

impl SubjectKey {
    fn of(spki: &[u8]) -> SubjectKey {
        match spki.len() {
            44 => SubjectKey {
                bits: spki[12..].to_vec(),
                algorithm: &PKCS_ED25519,
            },
            91 => SubjectKey {
                bits: spki[26..].to_vec(),
                algorithm: &PKCS_ECDSA_P256_SHA256,
            },
            n => panic!("the kit certifies ed25519 or P-256 keys, not a {n}-byte SPKI"),
        }
    }
}

impl PublicKeyData for SubjectKey {
    fn der_bytes(&self) -> &[u8] {
        &self.bits
    }

    fn algorithm(&self) -> &'static SignatureAlgorithm {
        self.algorithm
    }
}

/// A DSSE envelope's JSON for `payload`, signed by `sign` over its PAE.
pub fn envelope(payload: &[u8], sign: &dyn Fn(&[u8]) -> Vec<u8>, keyid: &str) -> String {
    let mut pae = format!("DSSEv1 {} {IN_TOTO} {} ", IN_TOTO.len(), payload.len()).into_bytes();
    pae.extend_from_slice(payload);
    json!({
        "payload": B64.encode(payload),
        "payloadType": IN_TOTO,
        "signatures": [{"keyid": keyid, "sig": B64.encode(sign(&pae))}]
    })
    .to_string()
}

/// PEM of `der` under `label`.
pub fn pem(label: &str, der: &[u8]) -> String {
    let body = B64.encode(der);
    let mut text = format!("-----BEGIN {label}-----\n");
    for chunk in body.as_bytes().chunks(64) {
        text.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        text.push('\n');
    }
    text.push_str("-----END ");
    text.push_str(label);
    text.push_str("-----\n");
    text
}

fn decode_hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

/// A DER `UTF8String` of `text`.
fn utf8_string(text: &str) -> Vec<u8> {
    der::asn1::Utf8StringRef::new(text)
        .expect("UTF-8")
        .to_der()
        .expect("it encodes")
}

/// A certificate's DER `SubjectPublicKeyInfo`.
fn ca_spki(certificate: &[u8]) -> Vec<u8> {
    x509_cert::Certificate::from_der(certificate)
        .expect("a certificate")
        .tbs_certificate
        .subject_public_key_info
        .to_der()
        .expect("it encodes")
}
