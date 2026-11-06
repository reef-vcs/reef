//! Reef's evidence objects (R3.01, ADR 0055): a claim about a tree, as an
//! in-toto Statement v1 with Reef's predicate, signed once with ed25519 in
//! a DSSE envelope.
//!
//! - [`statement`]: the Statement, the predicate's fields, and [`Claim`],
//!   which builds one and signs it.
//! - [`envelope`]: the DSSE envelope, its pre-authentication encoding
//!   ([`pae`]), and the canonical form an evidence id is the SHA-256 of.
//! - [`keys`]: ed25519 keys as PKCS#8 and `SubjectPublicKeyInfo`, key ids,
//!   and the checks that close the gap between ring and strict verification.
//! - [`verify`]: [`Verified::check`], the one checking order the daemon (at
//!   add and at every read), the CLI and executors share.
//! - [`session`]: session-key certificates (R3.03, ADR 0057): a second
//!   predicate type in the same envelope, by which an issuing authority
//!   binds a short-lived key to an operator identity, and what a verifier
//!   reports for evidence that key signed.
//!
//! A signature that verifies says that the holder of a key signed the
//! claim. It does not say who holds the key, that anyone should believe it,
//! or that the claim is true; policy decides that (R3.07).
//!
//! The crate depends on no Reef crate but `reef-error`, and on no jj-lib,
//! gRPC, SQLite or tokio, so an executor or the phase-four store can read
//! the format without the client.

use reef_error::{Classify, ErrorKind};

pub mod envelope;
pub mod keys;
pub mod session;
pub mod statement;
pub mod verify;

pub use envelope::{Envelope, Signature, pae};
pub use keys::{KeyError, KeyId, PublicKey, SigningKey};
pub use statement::{
    Artifact, Claim, Outcome, Predicate, ResourceDescriptor, Statement, TreeDigest,
};
pub use verify::{Index, Verified};

/// The format of the evidence objects this crate writes and checks: a
/// stored row's `format` column. Format 1 is fixed (ADR 0055): Statement
/// v1, [`PAYLOAD_TYPE`], one signature, a `gitTree` subject,
/// [`PREDICATE_TYPE`], SHA-256 ids over the canonical envelope, and SPKI key
/// ids.
pub const FORMAT: u32 = 1;

/// Reef's v1 predicate type.
pub const PREDICATE_TYPE: &str = "urn:reef-vcs:evidence:v1";

/// The DSSE `payloadType` of an in-toto Statement.
pub const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";

/// The Statement's `_type`.
pub const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";

/// The largest envelope accepted, as received: 64 KiB. Logs belong in
/// artifacts.
pub const MAX_ENVELOPE_BYTES: usize = 64 * 1024;

/// How deep the envelope's JSON, and the payload's, may nest: 32 levels.
pub const MAX_DEPTH: usize = 32;

/// The most artifacts a predicate lists.
pub const MAX_ARTIFACTS: usize = 64;

/// The longest `target`, in bytes.
pub const MAX_TARGET_BYTES: usize = 512;

/// Why an envelope is not evidence. Each variant but [`Invalid::Signature`]
/// is one value of the daemon's `INVALID_EVIDENCE` reason's `check`
/// metadata ([`Invalid::check`]); [`Invalid::Signature`] is its own reason,
/// `SIGNATURE_INVALID`. The message says what was found.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Invalid {
    /// Over [`MAX_ENVELOPE_BYTES`].
    #[error("the envelope is {0} bytes; evidence is at most {MAX_ENVELOPE_BYTES} bytes")]
    Size(usize),
    /// Not a DSSE envelope as JSON: not JSON, nested too deep, a duplicate
    /// key, a missing field, or base64 that does not decode.
    #[error("the envelope is not a DSSE envelope: {0}")]
    Envelope(String),
    /// A `payloadType` other than [`PAYLOAD_TYPE`].
    #[error("the payload type is {0:?}; evidence is {PAYLOAD_TYPE:?}")]
    PayloadType(String),
    /// Not exactly one signature.
    #[error("the envelope has {0} signatures; Reef evidence has exactly one")]
    Signatures(usize),
    /// The public key is not an ed25519 `SubjectPublicKeyInfo`, or is a
    /// small-order or non-canonical point.
    #[error("the public key is not usable: {0}")]
    PublicKey(String),
    /// The signature's `keyid` is not the public key's id.
    #[error("the signature's keyid is {found:?}; the public key's id is {expected}")]
    KeyId {
        /// The envelope's `keyid`, empty when it has none.
        found: String,
        /// The id of the key supplied.
        expected: String,
    },
    /// The signature does not verify over the payload with the key.
    #[error("the signature does not verify with key {key_id}")]
    Signature {
        /// The key's id.
        key_id: String,
    },
    /// The payload is not an in-toto Statement v1 as JSON.
    #[error("the payload is not an in-toto Statement v1: {0}")]
    Statement(String),
    /// Not exactly one subject with a `gitTree` digest of lowercase hex.
    #[error("the subject is not one git tree: {0}")]
    Subject(String),
    /// A `predicateType` other than [`PREDICATE_TYPE`].
    #[error("the predicate type is {0:?}; this Reef reads {PREDICATE_TYPE:?}")]
    PredicateType(String),
    /// A predicate field that is missing or out of its bounds.
    #[error("predicate field {field}: {message}")]
    Predicate {
        /// The field, as the JSON names it (`kind`, `startedOn`, …); empty
        /// when the predicate is not an object.
        field: String,
        /// What is wrong with it.
        message: String,
    },
    /// A session-key certificate whose `keyid` names no authority the
    /// verifier knows (R3.03).
    #[error("the certificate's keyid {key_id:?} names no known issuing authority")]
    UnknownAuthority {
        /// The envelope's `keyid`.
        key_id: String,
    },
}

impl Invalid {
    /// The `check` metadata value of `INVALID_EVIDENCE`; `None` for
    /// [`Invalid::Signature`], which is `SIGNATURE_INVALID`.
    pub fn check(&self) -> Option<&'static str> {
        Some(match self {
            Invalid::Size(_) => "size",
            Invalid::Envelope(_) => "envelope",
            Invalid::PayloadType(_) => "payload_type",
            Invalid::Signatures(_) => "signatures",
            Invalid::PublicKey(_) => "public_key",
            Invalid::KeyId { .. } => "key_id",
            Invalid::Signature { .. } => return None,
            Invalid::Statement(_) => "statement",
            Invalid::Subject(_) => "subject",
            Invalid::PredicateType(_) => "predicate_type",
            Invalid::Predicate { .. } => "predicate",
            Invalid::UnknownAuthority { .. } => "authority",
        })
    }

    /// The predicate field, for [`Invalid::Predicate`].
    pub fn field(&self) -> Option<&str> {
        match self {
            Invalid::Predicate { field, .. } if !field.is_empty() => Some(field),
            _ => None,
        }
    }

    /// What the format wants, in one sentence, for an error's hint.
    pub fn hint(&self) -> &'static str {
        match self {
            Invalid::Size(_) => {
                "Keep the envelope under 64 KiB: put logs and reports in artifacts, recorded by digest."
            }
            Invalid::Envelope(_) => {
                "Send a DSSE envelope as JSON, {\"payload\", \"payloadType\", \"signatures\"}, with base64 payload and signature, no duplicate keys, and at most 32 levels of nesting."
            }
            Invalid::PayloadType(_) => {
                "Set payloadType to application/vnd.in-toto+json: evidence is an in-toto Statement."
            }
            Invalid::Signatures(_) => {
                "Sign once: an evidence object has one attestor, and a countersignature is a separate object."
            }
            Invalid::PublicKey(_) => {
                "Send the signer's ed25519 public key as DER SubjectPublicKeyInfo, as `reef evidence key generate` writes it in <path>.pub."
            }
            Invalid::KeyId { .. } => {
                "Set the signature's keyid to the SHA-256 of the public key's DER SubjectPublicKeyInfo, in lowercase hex."
            }
            Invalid::Signature { .. } => {
                "Sign the DSSE pre-authentication encoding of the payload, PAE(payloadType, payload), with the private key of the public key sent."
            }
            Invalid::Statement(_) => {
                "The payload is an in-toto Statement v1 as UTF-8 JSON, with _type https://in-toto.io/Statement/v1, no duplicate keys, and at most 32 levels of nesting."
            }
            Invalid::Subject(_) => {
                "Name exactly one subject, {\"name\": \"tree\", \"digest\": {\"gitTree\": <lowercase hex tree id>}}, and no other digest."
            }
            Invalid::PredicateType(_) => "Set predicateType to urn:reef-vcs:evidence:v1.",
            Invalid::Predicate { .. } => {
                "See docs/evidence.md for the predicate's fields: kind, result (pass, fail or error), startedOn and finishedOn in RFC 3339 UTC, and the optional target, artifacts and annotations."
            }
            Invalid::UnknownAuthority { .. } => {
                "A session key's certificate verifies only against an authority the verifier holds: `reef identity authority show` prints this daemon's."
            }
        }
    }
}

impl Classify for Invalid {
    fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidArgument
    }
}

/// Lowercase hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Whether `text` is `len` lowercase hex digits.
pub(crate) fn is_lower_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// SHA-256 of `bytes`, in lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

/// SHA-256 of everything `reader` yields, in lowercase hex, read 64 KiB at
/// a time: an artifact's digest without holding the file.
pub fn sha256_hex_of(mut reader: impl std::io::Read) -> std::io::Result<String> {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = vec![0; 64 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(hex(context.finish().as_ref())),
            Ok(n) => context.update(&buffer[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}
