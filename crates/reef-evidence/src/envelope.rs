//! The DSSE v1 envelope, its pre-authentication encoding, and its
//! canonical form.
//!
//! An envelope as received may use standard or URL-safe base64, with or
//! without padding, extra whitespace, and fields DSSE says consumers
//! ignore. The signature covers [`pae`] of the payload, not the envelope's
//! encoding, so Reef stores every envelope in one canonical form, which its
//! evidence id is the SHA-256 of:
//!
//! ```json
//! {"payload":"<standard base64>","payloadType":"application/vnd.in-toto+json","signatures":[{"keyid":"<key id>","sig":"<standard base64>"}]}
//! ```
//!
//! compact, keys in that order, padded standard base64, and no trailing
//! newline.

use base64::Engine as _;
use base64::alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A DSSE envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename = "evidence.Envelope")]
pub struct Envelope {
    /// The payload, base64: an in-toto Statement as UTF-8 JSON.
    pub payload: String,
    /// `application/vnd.in-toto+json`.
    #[schemars(extend("const" = "application/vnd.in-toto+json"))]
    pub payload_type: String,
    /// Exactly one signature.
    #[schemars(length(min = 1, max = 1))]
    pub signatures: Vec<Signature>,
}

/// One signature of an envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "evidence.Signature")]
pub struct Signature {
    /// The signer's key id: the SHA-256 of its DER `SubjectPublicKeyInfo`, in
    /// lowercase hex. A hint, which must equal the key sent with the
    /// envelope; verification uses the key, never the hint.
    pub keyid: String,
    /// The ed25519 signature of `PAE(payloadType, payload)`, base64.
    pub sig: String,
}

impl Envelope {
    /// An envelope of `payload` with one signature, in canonical form.
    pub fn new(payload_type: &str, payload: &[u8], keyid: &str, sig: &[u8]) -> Envelope {
        Envelope {
            payload: STANDARD.encode(payload),
            payload_type: payload_type.to_owned(),
            signatures: vec![Signature {
                keyid: keyid.to_owned(),
                sig: STANDARD.encode(sig),
            }],
        }
    }

    /// The envelope as compact JSON in field order: the canonical bytes, for
    /// an envelope built by [`Envelope::new`].
    pub fn to_bytes(&self) -> Vec<u8> {
        // Strings only: serialization cannot fail.
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// The payload's bytes; `None` when it is not base64.
    pub fn payload_bytes(&self) -> Option<Vec<u8>> {
        decode_base64(&self.payload)
    }
}

/// The DSSE v1 pre-authentication encoding:
/// `"DSSEv1" SP LEN(type) SP type SP LEN(body) SP body`, each LEN the
/// decimal byte length with no leading zeros. This is what is signed.
pub fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload_type.len() + payload.len() + 32);
    out.extend_from_slice(b"DSSEv1 ");
    out.extend_from_slice(payload_type.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload_type.as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload);
    out
}

const LENIENT: GeneralPurposeConfig = GeneralPurposeConfig::new()
    .with_decode_padding_mode(DecodePaddingMode::Indifferent)
    .with_decode_allow_trailing_bits(false);
const STANDARD_ANY_PADDING: GeneralPurpose = GeneralPurpose::new(&alphabet::STANDARD, LENIENT);
const URL_SAFE_ANY_PADDING: GeneralPurpose = GeneralPurpose::new(&alphabet::URL_SAFE, LENIENT);

/// `text` decoded as standard base64, else URL-safe, with or without
/// padding, as DSSE requires verifiers to accept. Whitespace, trailing
/// garbage and non-zero trailing bits are refused, so a string has at most
/// one decoding.
pub(crate) fn decode_base64(text: &str) -> Option<Vec<u8>> {
    STANDARD_ANY_PADDING
        .decode(text)
        .or_else(|_| URL_SAFE_ANY_PADDING.decode(text))
        .ok()
}

/// `bytes` as padded standard base64.
pub(crate) fn encode_base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decodes_one_way() {
        let bytes = [0xfb, 0xff, 0x01];
        assert_eq!(decode_base64("+/8B").unwrap(), bytes);
        assert_eq!(decode_base64("-_8B").unwrap(), bytes);
        assert_eq!(decode_base64("AQ==").unwrap(), [1]);
        assert_eq!(decode_base64("AQ").unwrap(), [1]);
        // Trailing bits, whitespace, garbage.
        assert_eq!(decode_base64("AR=="), None);
        assert_eq!(decode_base64("AQ== "), None);
        assert_eq!(decode_base64("AQ==AQ"), None);
        assert_eq!(decode_base64("+_8B"), None, "two alphabets mixed");
    }
}
