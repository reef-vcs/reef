//! A software authenticator for tests (feature `testing`): it registers and
//! asserts with an ES256, `EdDSA` or RS256 key held by ring, writing the
//! bytes a browser and a platform authenticator would, so the daemon's and
//! the CLI's tests drive passkey ceremonies without a browser.
//!
//! ES256 and `EdDSA` keys are new for each authenticator. ring cannot make
//! RSA keys, so every RS256 authenticator holds the one committed 2048-bit
//! key (`tests/fixtures/webauthn/rs256.pk8`) and has the same fingerprint.
#![allow(
    clippy::expect_used,
    reason = "a test authenticator: ring refusing its own keys is a broken test"
)]

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::{SecureRandom as _, SystemRandom};
use ring::signature::{
    ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair as _, RSA_PKCS1_SHA256,
    RsaKeyPair,
};

use super::{
    Algorithm, Assertion, FLAG_AT, FLAG_UP, FLAG_UV, NULL_PARAMETERS, OID_EC_PUBLIC_KEY,
    OID_RSA_ENCRYPTION, P256_PARAMETERS, PasskeyKey, TAG_BIT_STRING, TAG_OID, TAG_SEQUENCE,
    rsa_integers, sha256,
};

/// The committed RSA key every RS256 authenticator holds.
const RS256_PKCS8: &[u8] = include_bytes!("../../tests/fixtures/webauthn/rs256.pk8");

/// The AAGUID the authenticator reports.
pub const AAGUID: [u8; 16] = *b"reef-test-authnr";

enum Key {
    Es256(EcdsaKeyPair),
    EdDsa(Ed25519KeyPair),
    Rs256(RsaKeyPair),
}

/// A passkey on a software authenticator.
pub struct Authenticator {
    key: Key,
    passkey: PasskeyKey,
    credential_id: Vec<u8>,
    sign_count: Option<u32>,
    user_verified: bool,
}

/// What a registration returns, as a browser's `PublicKeyCredential` gives
/// it (`getPublicKey()`, `getPublicKeyAlgorithm()`,
/// `getAuthenticatorData()`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    /// `rawId`.
    pub credential_id: Vec<u8>,
    /// The DER `SubjectPublicKeyInfo`.
    pub public_key: Vec<u8>,
    /// The COSE algorithm.
    pub algorithm: i64,
    /// The authenticator data, with attested credential data: the AAGUID,
    /// the credential id and the COSE key.
    pub authenticator_data: Vec<u8>,
    /// `clientDataJSON`, type `webauthn.create`.
    pub client_data_json: Vec<u8>,
}

impl Authenticator {
    /// An authenticator with a passkey of `algorithm`, which verifies the
    /// user and counts its signatures from 0.
    pub fn new(algorithm: Algorithm) -> Authenticator {
        let random = SystemRandom::new();
        let (key, spki) = match algorithm {
            Algorithm::Es256 => {
                let document =
                    EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &random)
                        .expect("an ES256 key");
                let pair = EcdsaKeyPair::from_pkcs8(
                    &ECDSA_P256_SHA256_ASN1_SIGNING,
                    document.as_ref(),
                    &random,
                )
                .expect("its own ES256 key");
                let algorithm = [&der(TAG_OID, OID_EC_PUBLIC_KEY)[..], P256_PARAMETERS].concat();
                let spki = spki(&algorithm, pair.public_key().as_ref());
                (Key::Es256(pair), spki)
            }
            Algorithm::EdDsa => {
                let document = Ed25519KeyPair::generate_pkcs8(&random).expect("an EdDSA key");
                let pair = Ed25519KeyPair::from_pkcs8(document.as_ref()).expect("its own key");
                let spki = [&crate::keys::SPKI_PREFIX[..], pair.public_key().as_ref()].concat();
                (Key::EdDsa(pair), spki)
            }
            Algorithm::Rs256 => {
                let pair = RsaKeyPair::from_pkcs8(RS256_PKCS8).expect("the committed RSA key");
                let algorithm = [&der(TAG_OID, OID_RSA_ENCRYPTION)[..], NULL_PARAMETERS].concat();
                let spki = spki(&algorithm, pair.public().as_ref());
                (Key::Rs256(pair), spki)
            }
        };
        let passkey = PasskeyKey::from_spki(&spki, algorithm.cose()).expect("a supported key");
        let mut credential_id = vec![0; 16];
        random.fill(&mut credential_id).expect("random bytes");
        Authenticator {
            key,
            passkey,
            credential_id,
            sign_count: Some(0),
            user_verified: true,
        }
    }

    /// The passkey's public key.
    pub fn passkey(&self) -> &PasskeyKey {
        &self.passkey
    }

    /// The credential id.
    pub fn credential_id(&self) -> &[u8] {
        &self.credential_id
    }

    /// Whether the authenticator verifies the user: `false` leaves the UV
    /// flag unset, as CDP's `WebAuthn.setUserVerified(false)` does.
    pub fn set_user_verified(&mut self, verified: bool) {
        self.user_verified = verified;
    }

    /// The sign count the next ceremony increments; `None` for an
    /// authenticator that keeps none and reports 0, as synced passkeys do.
    pub fn set_sign_count(&mut self, count: Option<u32>) {
        self.sign_count = count;
    }

    /// Registers the passkey at `rp_id`, from a page at `origin`, over the
    /// daemon's `challenge`, with attestation `none`.
    pub fn register(&mut self, rp_id: &str, origin: &str, challenge: &[u8; 32]) -> Registration {
        let mut authenticator_data = self.authenticator_data(rp_id, FLAG_AT);
        authenticator_data.extend_from_slice(&AAGUID);
        let len = u16::try_from(self.credential_id.len()).expect("a short credential id");
        authenticator_data.extend_from_slice(&len.to_be_bytes());
        authenticator_data.extend_from_slice(&self.credential_id);
        authenticator_data.extend_from_slice(&self.cose_key());
        Registration {
            credential_id: self.credential_id.clone(),
            public_key: self.passkey.spki().to_vec(),
            algorithm: self.passkey.algorithm().cose(),
            authenticator_data,
            client_data_json: client_data_json("webauthn.create", challenge, origin),
        }
    }

    /// An assertion at `rp_id`, from a page at `origin`, over `challenge`.
    pub fn assert(&mut self, rp_id: &str, origin: &str, challenge: &[u8; 32]) -> Assertion {
        let authenticator_data = self.authenticator_data(rp_id, 0);
        let client_data_json = client_data_json("webauthn.get", challenge, origin);
        let signature = self.sign_assertion(&authenticator_data, &client_data_json);
        Assertion {
            authenticator_data,
            client_data_json,
            signature,
        }
    }

    /// The passkey's signature over `authenticator_data ‖
    /// SHA-256(client_data_json)`, for a test that builds its own bytes.
    pub fn sign_assertion(&self, authenticator_data: &[u8], client_data_json: &[u8]) -> Vec<u8> {
        let message = [authenticator_data, &sha256(client_data_json)].concat();
        let random = SystemRandom::new();
        match &self.key {
            Key::Es256(pair) => pair
                .sign(&random, &message)
                .expect("an ES256 signature")
                .as_ref()
                .to_vec(),
            Key::EdDsa(pair) => pair.sign(&message).as_ref().to_vec(),
            Key::Rs256(pair) => {
                let mut signature = vec![0; pair.public().modulus_len()];
                pair.sign(&RSA_PKCS1_SHA256, &random, &message, &mut signature)
                    .expect("an RS256 signature");
                signature
            }
        }
    }

    /// The RP ID hash, the flags (UP, UV unless turned off, and `extra`) and
    /// the next sign count.
    fn authenticator_data(&mut self, rp_id: &str, extra: u8) -> Vec<u8> {
        let uv = if self.user_verified { FLAG_UV } else { 0 };
        let count = match &mut self.sign_count {
            Some(count) => {
                *count += 1;
                *count
            }
            None => 0,
        };
        let mut data = sha256(rp_id.as_bytes()).to_vec();
        data.push(FLAG_UP | uv | extra);
        data.extend_from_slice(&count.to_be_bytes());
        data
    }

    /// The public key as a COSE key in CTAP2's canonical CBOR.
    fn cose_key(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match &self.key {
            Key::Es256(pair) => {
                let (x, y) = pair.public_key().as_ref()[1..].split_at(32);
                cbor_head(5, 5, &mut out);
                for (label, value) in [(1, 2), (3, -7), (-1, 1)] {
                    cbor_int(label, &mut out);
                    cbor_int(value, &mut out);
                }
                cbor_int(-2, &mut out);
                cbor_bytes(x, &mut out);
                cbor_int(-3, &mut out);
                cbor_bytes(y, &mut out);
            }
            Key::EdDsa(pair) => {
                cbor_head(5, 4, &mut out);
                for (label, value) in [(1, 1), (3, -8), (-1, 6)] {
                    cbor_int(label, &mut out);
                    cbor_int(value, &mut out);
                }
                cbor_int(-2, &mut out);
                cbor_bytes(pair.public_key().as_ref(), &mut out);
            }
            Key::Rs256(pair) => {
                let (n, e) = rsa_integers(pair.public().as_ref()).expect("an RSAPublicKey");
                cbor_head(5, 4, &mut out);
                for (label, value) in [(1, 3), (3, -257)] {
                    cbor_int(label, &mut out);
                    cbor_int(value, &mut out);
                }
                cbor_int(-1, &mut out);
                cbor_bytes(n, &mut out);
                cbor_int(-2, &mut out);
                cbor_bytes(e, &mut out);
            }
        }
        out
    }
}

/// `clientDataJSON` as Chromium writes it: `type`, `challenge` (unpadded
/// base64url), `origin`, `crossOrigin` false.
pub fn client_data_json(ceremony: &str, challenge: &[u8; 32], origin: &str) -> Vec<u8> {
    format!(
        r#"{{"type":{},"challenge":"{}","origin":{},"crossOrigin":false}}"#,
        serde_json::Value::from(ceremony),
        URL_SAFE_NO_PAD.encode(challenge),
        serde_json::Value::from(origin),
    )
    .into_bytes()
}

/// A DER `SubjectPublicKeyInfo` of the algorithm identifier's contents and
/// the key's bits.
fn spki(algorithm: &[u8], key: &[u8]) -> Vec<u8> {
    let bits = [&[0][..], key].concat();
    der(
        TAG_SEQUENCE,
        &[der(TAG_SEQUENCE, algorithm), der(TAG_BIT_STRING, &bits)].concat(),
    )
}

/// One DER element.
fn der(tag: u8, contents: &[u8]) -> Vec<u8> {
    let len = contents.len();
    let mut out = vec![tag];
    match u8::try_from(len) {
        Ok(short) if short < 0x80 => out.push(short),
        Ok(long) => out.extend_from_slice(&[0x81, long]),
        Err(_) => {
            out.push(0x82);
            out.extend_from_slice(&u16::try_from(len).expect("under 64 KiB").to_be_bytes());
        }
    }
    out.extend_from_slice(contents);
    out
}

fn cbor_head(major: u8, value: u64, out: &mut Vec<u8>) {
    let major = major << 5;
    match value {
        0..=23 => out.push(major | u8::try_from(value).expect("small")),
        24..=0xff => out.extend_from_slice(&[major | 0x18, u8::try_from(value).expect("a byte")]),
        0x100..=0xffff => {
            out.push(major | 0x19);
            out.extend_from_slice(&u16::try_from(value).expect("two bytes").to_be_bytes());
        }
        _ => {
            out.push(major | 0x1a);
            out.extend_from_slice(&u32::try_from(value).expect("four bytes").to_be_bytes());
        }
    }
}

fn cbor_int(value: i64, out: &mut Vec<u8>) {
    match u64::try_from(value) {
        Ok(positive) => cbor_head(0, positive, out),
        Err(_) => cbor_head(1, (-1 - value).unsigned_abs(), out),
    }
}

fn cbor_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    cbor_head(2, bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}
