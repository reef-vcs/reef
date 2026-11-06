//! ed25519 keys, through ring: private keys as PKCS#8, public keys as DER
//! `SubjectPublicKeyInfo`, and key ids as the SHA-256 of that DER.
//!
//! ring's verification rejects non-canonical signatures but accepts
//! small-order and non-canonically encoded public keys, which strict
//! verification (ed25519-dalek's `verify_strict`) refuses. A small-order key
//! verifies signatures that no private key made, so [`PublicKey::from_spki`]
//! refuses them before anything is verified.

use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey};

use crate::Invalid;
use crate::envelope::{Envelope, encode_base64};

/// The DER of an ed25519 `SubjectPublicKeyInfo` up to the key:
/// `SEQUENCE { SEQUENCE { OID 1.3.101.112 } BIT STRING { 0x00, <32 bytes> } }`.
pub const SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// The length of an ed25519 `SubjectPublicKeyInfo`'s DER.
pub const SPKI_LEN: usize = SPKI_PREFIX.len() + 32;

/// The DER of an ed25519 PKCS#8 v1 private key up to its 32-byte seed, as
/// OpenSSL 3 writes it: `SEQUENCE { INTEGER 0, SEQUENCE { OID 1.3.101.112 },
/// OCTET STRING { OCTET STRING <32 bytes> } }`.
pub const PKCS8_V1_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// The eight points of small order on edwards25519, in their canonical
/// encodings: the identity, the point of order 2, the two of order 4 and
/// the four of order 8. These are the torsion points listed by
/// curve25519-dalek (`EIGHT_TORSION`) and in libsodium's blocklist.
pub const SMALL_ORDER: [[u8; 32]; 8] = [
    // The identity (0, 1).
    hex32("0100000000000000000000000000000000000000000000000000000000000000"),
    // Order 8.
    hex32("c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a"),
    // Order 4, x negative.
    hex32("0000000000000000000000000000000000000000000000000000000000000080"),
    // Order 8.
    hex32("26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05"),
    // Order 2, (0, -1).
    hex32("ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f"),
    // Order 8.
    hex32("26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc85"),
    // Order 4, x positive.
    hex32("0000000000000000000000000000000000000000000000000000000000000000"),
    // Order 8.
    hex32("c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac03fa"),
];

/// 32 bytes from 64 hex digits, at compile time.
const fn hex32(text: &str) -> [u8; 32] {
    const fn digit(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("not a lowercase hex digit"),
        }
    }
    let bytes = text.as_bytes();
    assert!(bytes.len() == 64, "not 64 hex digits");
    let mut out = [0; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = digit(bytes[2 * i]) << 4 | digit(bytes[2 * i + 1]);
        i += 1;
    }
    out
}

/// Whether the y coordinate of an encoded point, its low 255 bits, is at
/// least p = 2^255 - 19: a non-canonical encoding of y - p.
fn y_is_non_canonical(point: &[u8; 32]) -> bool {
    point[31] & 0x7f == 0x7f && point[1..31].iter().all(|&b| b == 0xff) && point[0] >= 0xed
}

/// Whether the point is (0, 1) or (0, -1) with the sign bit set: x is 0,
/// which has no negative, so the encoding is non-canonical.
fn sign_is_non_canonical(point: &[u8; 32]) -> bool {
    let mut positive = *point;
    positive[31] &= 0x7f;
    point[31] & 0x80 != 0 && (positive == SMALL_ORDER[0] || positive == SMALL_ORDER[4])
}

/// The length of a key id: 64 lowercase hex digits.
pub const KEY_ID_LEN: usize = 64;

/// A key id: the SHA-256 of a DER `SubjectPublicKeyInfo`, in lowercase hex,
/// as `openssl pkey -pubin -outform DER | shasum -a 256` prints it and as
/// sigstore recommends for a key hint.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyId(String);

impl KeyId {
    /// The id of the key whose DER `SubjectPublicKeyInfo` is `spki_der`.
    pub fn of(spki_der: &[u8]) -> KeyId {
        KeyId(crate::sha256_hex(spki_der))
    }

    /// The id, 64 lowercase hex digits.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for KeyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An ed25519 public key that is neither small-order nor non-canonical.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    raw: [u8; 32],
    spki: Vec<u8>,
    id: KeyId,
}

impl PublicKey {
    /// The key whose DER `SubjectPublicKeyInfo` is `der`; [`Invalid::PublicKey`]
    /// for another algorithm, malformed DER, or a small-order or
    /// non-canonical point.
    pub fn from_spki(der: &[u8]) -> Result<PublicKey, Invalid> {
        let Some(raw) = der.strip_prefix(&SPKI_PREFIX[..]) else {
            return Err(Invalid::PublicKey(format!(
                "{} bytes that are not an ed25519 SubjectPublicKeyInfo; Reef evidence is signed with ed25519",
                der.len()
            )));
        };
        let raw: [u8; 32] = raw.try_into().map_err(|_| {
            Invalid::PublicKey(format!(
                "an ed25519 SubjectPublicKeyInfo is {SPKI_LEN} bytes, not {}",
                der.len()
            ))
        })?;
        PublicKey::from_raw(raw)
    }

    /// The key whose 32-byte encoding is `raw`.
    pub fn from_raw(raw: [u8; 32]) -> Result<PublicKey, Invalid> {
        if SMALL_ORDER.contains(&raw) {
            return Err(Invalid::PublicKey("a small-order point".into()));
        }
        if y_is_non_canonical(&raw) || sign_is_non_canonical(&raw) {
            return Err(Invalid::PublicKey("a non-canonical point encoding".into()));
        }
        let mut spki = SPKI_PREFIX.to_vec();
        spki.extend_from_slice(&raw);
        let id = KeyId::of(&spki);
        Ok(PublicKey { raw, spki, id })
    }

    /// The key in a PEM `PUBLIC KEY` block, as `<path>.pub` holds it.
    pub fn from_pem(text: &str) -> Result<PublicKey, KeyError> {
        let der = pem_decode(text, "PUBLIC KEY")?;
        PublicKey::from_spki(&der).map_err(KeyError::Unusable)
    }

    /// The algorithm: `ed25519`.
    pub fn algorithm(&self) -> &'static str {
        "ed25519"
    }

    /// The DER `SubjectPublicKeyInfo`.
    pub fn spki(&self) -> &[u8] {
        &self.spki
    }

    /// The key's id.
    pub fn id(&self) -> &KeyId {
        &self.id
    }

    /// The key as a PEM `PUBLIC KEY` block.
    pub fn to_pem(&self) -> String {
        pem_encode("PUBLIC KEY", &self.spki)
    }

    /// Whether `sig` is this key's signature of `message`.
    pub fn verify(&self, message: &[u8], sig: &[u8]) -> bool {
        UnparsedPublicKey::new(&ED25519, &self.raw)
            .verify(message, sig)
            .is_ok()
    }
}

/// A key file that cannot be used to sign or verify.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum KeyError {
    /// No PEM block of the expected label.
    #[error("{0}")]
    Pem(String),
    /// The DER is not an ed25519 key ring accepts.
    #[error("not an ed25519 PKCS#8 private key: {0}")]
    Rejected(String),
    /// The key's public half is refused, or it does not verify its own
    /// signature.
    #[error(transparent)]
    Unusable(Invalid),
    /// The system's random number generator failed.
    #[error("generate a key: the system's random number generator failed")]
    Random,
}

impl reef_error::Classify for KeyError {
    fn kind(&self) -> reef_error::ErrorKind {
        match self {
            KeyError::Random => reef_error::ErrorKind::Internal,
            _ => reef_error::ErrorKind::InvalidArgument,
        }
    }
}

/// An ed25519 private key that signs evidence.
#[derive(Debug)]
pub struct SigningKey {
    pair: Ed25519KeyPair,
    public: PublicKey,
}

impl SigningKey {
    /// A new key: its PKCS#8 v2 PEM (`PRIVATE KEY`) and its public key's
    /// SPKI PEM (`PUBLIC KEY`).
    pub fn generate() -> Result<(String, String), KeyError> {
        let document =
            Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).map_err(|_| KeyError::Random)?;
        let key = SigningKey::from_pkcs8_der(document.as_ref())?;
        Ok((
            pem_encode("PRIVATE KEY", document.as_ref()),
            key.public.to_pem(),
        ))
    }

    /// The key in a PEM `PRIVATE KEY` block: PKCS#8 v1, as OpenSSL 3's
    /// `genpkey -algorithm ed25519` writes it, or v2, as
    /// [`SigningKey::generate`] does.
    pub fn from_pkcs8_pem(text: &str) -> Result<SigningKey, KeyError> {
        let der = pem_decode(text, "PRIVATE KEY")?;
        SigningKey::from_pkcs8_der(&der)
    }

    /// The key in PKCS#8 v1 or v2 DER. A v2 key's public key must match its
    /// private key; either way the key signs and verifies once before it is
    /// returned.
    pub fn from_pkcs8_der(der: &[u8]) -> Result<SigningKey, KeyError> {
        let pair = Ed25519KeyPair::from_pkcs8_maybe_unchecked(der)
            .map_err(|error| KeyError::Rejected(error.to_string()))?;
        SigningKey::from_pair(pair)
    }

    /// The key with this 32-byte seed: for tests and for tools that keep
    /// their seed elsewhere.
    pub fn from_seed(seed: &[u8; 32]) -> Result<SigningKey, KeyError> {
        let pair = Ed25519KeyPair::from_seed_unchecked(seed)
            .map_err(|error| KeyError::Rejected(error.to_string()))?;
        SigningKey::from_pair(pair)
    }

    fn from_pair(pair: Ed25519KeyPair) -> Result<SigningKey, KeyError> {
        let raw: [u8; 32] = pair
            .public_key()
            .as_ref()
            .try_into()
            .map_err(|_| KeyError::Rejected("its public key is not 32 bytes".into()))?;
        let public = PublicKey::from_raw(raw).map_err(KeyError::Unusable)?;
        let probe = b"reef-evidence key check";
        if !public.verify(probe, pair.sign(probe).as_ref()) {
            return Err(KeyError::Rejected(
                "it does not verify its own signature".into(),
            ));
        }
        Ok(SigningKey { pair, public })
    }

    /// The public key.
    pub fn public_key(&self) -> &PublicKey {
        &self.public
    }

    /// The ed25519 signature of `message`.
    pub fn sign_bytes(&self, message: &[u8]) -> Vec<u8> {
        self.pair.sign(message).as_ref().to_vec()
    }

    /// `payload` signed in a canonical envelope: one signature over
    /// [`crate::pae`]`(payload_type, payload)`, with this key's id. ed25519
    /// is deterministic, so the same payload gives the same envelope.
    pub fn sign(&self, payload_type: &str, payload: &[u8]) -> Envelope {
        let sig = self.sign_bytes(&crate::pae(payload_type, payload));
        Envelope::new(payload_type, payload, self.public.id.as_str(), &sig)
    }
}

/// `der` as a PEM block labelled `label`, lines of 64.
pub fn pem_encode(label: &str, der: &[u8]) -> String {
    let body = encode_base64(der);
    let mut text = format!("-----BEGIN {label}-----\n");
    for line in body.as_bytes().chunks(64) {
        text.push_str(&String::from_utf8_lossy(line));
        text.push('\n');
    }
    text.push_str("-----END ");
    text.push_str(label);
    text.push_str("-----\n");
    text
}

/// The DER of the first PEM block labelled `label` in `text`.
pub fn pem_decode(text: &str, label: &str) -> Result<Vec<u8>, KeyError> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let Some(start) = text.find(&begin) else {
        let found = text
            .split("-----BEGIN ")
            .nth(1)
            .and_then(|rest| rest.split("-----").next());
        return Err(KeyError::Pem(match found {
            Some(other) => format!(
                "a PEM {other} block, not {label}; Reef reads ed25519 keys as PKCS#8 PEM (PRIVATE KEY) and SubjectPublicKeyInfo PEM (PUBLIC KEY)"
            ),
            None => format!("no PEM {label} block"),
        }));
    };
    let body = &text[start + begin.len()..];
    let Some(stop) = body.find(&end) else {
        return Err(KeyError::Pem(format!(
            "the PEM {label} block has no end line"
        )));
    };
    let base64: String = body[..stop]
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    crate::envelope::decode_base64(&base64)
        .ok_or_else(|| KeyError::Pem(format!("the PEM {label} block is not base64")))
}
