//! Verified human presence (R3.43, ADR 0085): the `WebAuthn` evidence a
//! `user_verified` session-key certificate carries, and its checks.
//!
//! A passkey's assertion is checked as a `WebAuthn` relying party checks one,
//! from its bytes alone:
//!
//! 1. `clientDataJSON` is a JSON object whose `type` is `webauthn.get`,
//!    whose `challenge` is the expected one in unpadded base64url, and whose
//!    `origin` has the RP ID as its host, over `https`, or `http` for
//!    `localhost` only, and is not a cross-origin frame's.
//! 2. `authenticatorData` starts with SHA-256 of the RP ID, and its flags
//!    say the user was present (UP) and verified (UV).
//! 3. The signature verifies over `authenticatorData ‖
//!    SHA-256(clientDataJSON)` with the passkey's public key, by its COSE
//!    algorithm: ES256 (-7), `EdDSA` (-8) or RS256 (-257).
//!
//! The challenge of a sign-off's assertion is derived ([`approval_challenge`])
//! from the certificate's own fields, so any verifier checks it with nothing
//! but the certificate. What a stateless reader cannot know (that the
//! passkey is registered and not removed, the listener's port, the sign
//! count) is the daemon's to check at approval.
//!
//! Public keys travel as DER `SubjectPublicKeyInfo`, as a browser's
//! `getPublicKey()` returns them, so nothing here reads CBOR. A passkey's
//! fingerprint is the SHA-256 of that DER, a [`KeyId`] like Reef's own.
//! Every function is pure.

#[cfg(feature = "testing")]
pub mod testing;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{
    ECDSA_P256_SHA256_ASN1, ED25519, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey,
    VerificationAlgorithm,
};
use serde_json::Value;

use crate::keys::{KeyId, PublicKey};
use crate::pae;

/// The DSSE payload type of a sign-off approval's challenge.
pub const APPROVAL_PAYLOAD_TYPE: &str = "application/vnd.reef-vcs.presence-approval";

/// The first line of an approval challenge's payload.
pub const APPROVAL_PREFIX: &str = "reef-presence-approval/v1\n";

/// The DSSE payload type of a passkey change's challenge: a later add, or a
/// removal, approved by an existing passkey.
pub const PASSKEY_CHANGE_PAYLOAD_TYPE: &str = "application/vnd.reef-vcs.passkey-change";

/// The `type` of presence evidence: a `WebAuthn` assertion.
pub const EVIDENCE_TYPE: &str = "webauthn";

/// The longest credential id, in bytes, as `WebAuthn` bounds it.
pub const MAX_CREDENTIAL_ID_BYTES: usize = 1023;

/// The `type` of an assertion's client data.
const GET: &str = "webauthn.get";

/// The `type` of a registration's client data.
const CREATE: &str = "webauthn.create";

/// User present.
const FLAG_UP: u8 = 0x01;
/// User verified.
const FLAG_UV: u8 = 0x04;
/// Attested credential data follows the sign count.
const FLAG_AT: u8 = 0x40;
/// Extensions follow.
const FLAG_ED: u8 = 0x80;

/// The RP ID hash, the flags and the sign count.
const AUTHENTICATOR_DATA_LEN: usize = 37;

/// DER object identifiers, as their contents bytes.
const OID_EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_RSA_ENCRYPTION: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
/// The algorithm parameters, as whole DER elements: the P-256 curve's OID,
/// and RSA's NULL.
const P256_PARAMETERS: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const NULL_PARAMETERS: &[u8] = &[0x05, 0x00];

const TAG_INTEGER: u8 = 0x02;
const TAG_BIT_STRING: u8 = 0x03;
const TAG_OID: u8 = 0x06;
const TAG_SEQUENCE: u8 = 0x30;

/// A passkey's signature algorithm, by its COSE identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Algorithm {
    /// ECDSA with P-256 and SHA-256, DER signatures: COSE -7.
    Es256,
    /// Ed25519: COSE -8.
    EdDsa,
    /// RSASSA-PKCS1-v1_5 with SHA-256, 2048 to 8192 bits: COSE -257.
    Rs256,
}

impl Algorithm {
    /// Every algorithm this reader verifies.
    pub const ALL: &'static [Algorithm] = &[Algorithm::Es256, Algorithm::EdDsa, Algorithm::Rs256];

    /// The COSE algorithm identifier.
    pub fn cose(self) -> i64 {
        match self {
            Algorithm::Es256 => -7,
            Algorithm::EdDsa => -8,
            Algorithm::Rs256 => -257,
        }
    }

    /// The algorithm `cose` identifies; `None` for one this reader does not
    /// verify.
    pub fn from_cose(cose: i64) -> Option<Algorithm> {
        Algorithm::ALL
            .iter()
            .copied()
            .find(|algorithm| algorithm.cose() == cose)
    }

    /// Its JOSE name: `ES256`, `EdDSA` or `RS256`.
    pub fn name(self) -> &'static str {
        match self {
            Algorithm::Es256 => "ES256",
            Algorithm::EdDsa => "EdDSA",
            Algorithm::Rs256 => "RS256",
        }
    }

    fn verification(self) -> &'static dyn VerificationAlgorithm {
        match self {
            Algorithm::Es256 => &ECDSA_P256_SHA256_ASN1,
            Algorithm::EdDsa => &ED25519,
            Algorithm::Rs256 => &RSA_PKCS1_2048_8192_SHA256,
        }
    }
}

impl std::fmt::Display for Algorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.name(), self.cose())
    }
}

/// The check an assertion failed: the daemon's `PASSKEY_ASSERTION_INVALID`
/// `check` metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Check {
    /// `clientDataJSON` is not a JSON object, or its `type` is not the
    /// ceremony's.
    Type,
    /// The client data's `challenge` is not the expected one.
    Challenge,
    /// The client data's `origin` is not the RP ID's, or is a cross-origin
    /// frame's.
    Origin,
    /// `authenticatorData` does not start with SHA-256 of the RP ID, is
    /// not the length its flags say, or the RP ID is not a domain.
    Rp,
    /// The user present or user verified flag is not set.
    Flags,
    /// The signature does not verify.
    Signature,
    /// The COSE algorithm is not ES256, `EdDSA` or RS256, or the public key
    /// is not one of it.
    Algorithm,
    /// The fingerprint is not the public key's.
    Passkey,
}

impl Check {
    /// The check's name: `type`, `challenge`, `origin`, `rp`, `flags`,
    /// `signature`, `algorithm` or `passkey`.
    pub fn as_str(self) -> &'static str {
        match self {
            Check::Type => "type",
            Check::Challenge => "challenge",
            Check::Origin => "origin",
            Check::Rp => "rp",
            Check::Flags => "flags",
            Check::Signature => "signature",
            Check::Algorithm => "algorithm",
            Check::Passkey => "passkey",
        }
    }
}

impl std::fmt::Display for Check {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a passkey, an assertion or a registration is refused: the check, and
/// what was found.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the passkey's {check} check failed: {message}")]
pub struct AssertionError {
    /// The check that failed.
    pub check: Check,
    /// What was found.
    pub message: String,
}

fn refuse<T>(check: Check, message: impl Into<String>) -> Result<T, AssertionError> {
    Err(AssertionError {
        check,
        message: message.into(),
    })
}

/// A passkey's public key: a DER `SubjectPublicKeyInfo` of a supported
/// algorithm, with its fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasskeyKey {
    algorithm: Algorithm,
    spki: Vec<u8>,
    /// What ring verifies with: the uncompressed point, the 32-byte key, or
    /// the DER `RSAPublicKey`.
    raw: Vec<u8>,
    fingerprint: KeyId,
}

impl PasskeyKey {
    /// The key whose DER `SubjectPublicKeyInfo` is `spki`, for the COSE
    /// algorithm `cose`: a P-256 point for ES256, an ed25519 key that is
    /// neither small-order nor non-canonical for `EdDSA`, an RSA key of 2048
    /// to 8192 bits for RS256. Anything else fails the `algorithm` check.
    pub fn from_spki(spki: &[u8], cose: i64) -> Result<PasskeyKey, AssertionError> {
        let Some(algorithm) = Algorithm::from_cose(cose) else {
            return refuse(
                Check::Algorithm,
                format!("COSE algorithm {cose} is not ES256 (-7), EdDSA (-8) or RS256 (-257)"),
            );
        };
        let not = |what: &str| {
            refuse(
                Check::Algorithm,
                format!(
                    "{} bytes that are not {what}, as {algorithm} needs",
                    spki.len()
                ),
            )
        };
        let raw = match algorithm {
            Algorithm::EdDsa => match PublicKey::from_spki(spki) {
                Ok(key) => key.spki()[crate::keys::SPKI_PREFIX.len()..].to_vec(),
                Err(_) => return not("a usable ed25519 SubjectPublicKeyInfo"),
            },
            Algorithm::Es256 => match spki_parts(spki) {
                Some((OID_EC_PUBLIC_KEY, P256_PARAMETERS, key))
                    if key.len() == 65 && key[0] == 0x04 =>
                {
                    key.to_vec()
                }
                _ => return not("an uncompressed P-256 SubjectPublicKeyInfo"),
            },
            Algorithm::Rs256 => match spki_parts(spki) {
                Some((OID_RSA_ENCRYPTION, NULL_PARAMETERS, key))
                    if rsa_modulus_bits(key).is_some_and(|bits| (2048..=8192).contains(&bits)) =>
                {
                    key.to_vec()
                }
                _ => return not("an RSA SubjectPublicKeyInfo of 2048 to 8192 bits"),
            },
        };
        Ok(PasskeyKey {
            algorithm,
            spki: spki.to_vec(),
            raw,
            fingerprint: KeyId::of(spki),
        })
    }

    /// The algorithm.
    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// The DER `SubjectPublicKeyInfo`.
    pub fn spki(&self) -> &[u8] {
        &self.spki
    }

    /// The fingerprint: SHA-256 of the DER, lowercase hex, which a policy
    /// pins.
    pub fn fingerprint(&self) -> &KeyId {
        &self.fingerprint
    }

    /// Whether `signature` is this key's over `message`.
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        UnparsedPublicKey::new(self.algorithm.verification(), &self.raw)
            .verify(message, signature)
            .is_ok()
    }
}

/// What a `WebAuthn` assertion (`navigator.credentials.get`) returns, as its
/// bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assertion {
    /// `response.authenticatorData`.
    pub authenticator_data: Vec<u8>,
    /// `response.clientDataJSON`.
    pub client_data_json: Vec<u8>,
    /// `response.signature`.
    pub signature: Vec<u8>,
}

/// What a verified assertion says beyond its checks, for the daemon's own:
/// the exact origin (whose port is the listener's) and the sign count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asserted {
    /// The client data's `origin`, as the browser wrote it.
    pub origin: String,
    /// The authenticator's sign count: 0 for one that keeps none, as synced
    /// passkeys do.
    pub sign_count: u32,
}

/// Checks `assertion` by `key` for the RP ID `rp_id` over `challenge`, in
/// the module's order: client data, authenticator data, signature.
pub fn check_assertion(
    key: &PasskeyKey,
    rp_id: &str,
    challenge: &[u8; 32],
    assertion: &Assertion,
) -> Result<Asserted, AssertionError> {
    let origin = client_data(&assertion.client_data_json, GET, challenge, rp_id)?;
    let data = parse_authenticator_data(&assertion.authenticator_data, rp_id)?;
    if data.flags & FLAG_AT != 0 {
        return refuse(
            Check::Rp,
            "an assertion's authenticatorData carries no attested credential",
        );
    }
    if data.flags & FLAG_ED == 0 && assertion.authenticator_data.len() != AUTHENTICATOR_DATA_LEN {
        return refuse(
            Check::Rp,
            format!(
                "authenticatorData is {} bytes; with no extensions it is {AUTHENTICATOR_DATA_LEN}",
                assertion.authenticator_data.len()
            ),
        );
    }
    let mut message = assertion.authenticator_data.clone();
    message.extend_from_slice(&sha256(&assertion.client_data_json));
    if !key.verify(&message, &assertion.signature) {
        return refuse(
            Check::Signature,
            format!(
                "the signature does not verify with passkey {}",
                key.fingerprint()
            ),
        );
    }
    Ok(Asserted {
        origin,
        sign_count: data.sign_count,
    })
}

/// What a registration (`navigator.credentials.create`, attestation
/// `none`) returned, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registered {
    /// The client data's `origin`.
    pub origin: String,
    /// The authenticator's sign count.
    pub sign_count: u32,
    /// The authenticator's AAGUID; all zero when it gives none.
    pub aaguid: [u8; 16],
    /// The credential id the authenticator data names.
    pub credential_id: Vec<u8>,
}

/// Checks a registration's `clientDataJSON` (`webauthn.create`, the
/// daemon's `challenge`, the RP ID's origin) and `authenticatorData` (the
/// RP ID hash, UP and UV, and attested credential data, whose AAGUID and
/// credential id it returns). The public key is the browser's
/// `getPublicKey()`, checked by [`PasskeyKey::from_spki`]; with attestation
/// `none` nothing signs it.
pub fn check_registration(
    rp_id: &str,
    challenge: &[u8; 32],
    client_data_json: &[u8],
    authenticator_data: &[u8],
) -> Result<Registered, AssertionError> {
    let origin = client_data(client_data_json, CREATE, challenge, rp_id)?;
    let data = parse_authenticator_data(authenticator_data, rp_id)?;
    let attested = (data.flags & FLAG_AT != 0)
        .then(|| authenticator_data.get(AUTHENTICATOR_DATA_LEN..))
        .flatten()
        .and_then(|rest| {
            let (aaguid, rest) = rest.split_first_chunk::<16>()?;
            let (len, rest) = rest.split_first_chunk::<2>()?;
            let id = rest.get(..usize::from(u16::from_be_bytes(*len)))?;
            Some((*aaguid, id.to_vec()))
        });
    let Some((aaguid, credential_id)) = attested else {
        return refuse(
            Check::Rp,
            "a registration's authenticatorData has no attested credential data",
        );
    };
    if credential_id.is_empty() || credential_id.len() > MAX_CREDENTIAL_ID_BYTES {
        return refuse(
            Check::Rp,
            format!(
                "a credential id of {} bytes; WebAuthn's are 1 to {MAX_CREDENTIAL_ID_BYTES}",
                credential_id.len()
            ),
        );
    }
    Ok(Registered {
        origin,
        sign_count: data.sign_count,
        aaguid,
        credential_id,
    })
}

/// What one sign-off approves: an evidence statement and the change it is
/// added for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approves {
    /// SHA-256 of the statement payload the session key will sign,
    /// lowercase hex.
    pub statement: String,
    /// The change the evidence is added for: a full change id in lowercase
    /// hex.
    pub change: String,
}

impl Approves {
    /// Why it is not one, if it is not: a statement digest of 64 lowercase
    /// hex digits and a change id of 32.
    pub fn problem(&self) -> Option<&'static str> {
        if !crate::is_lower_hex(&self.statement, 64) {
            return Some("approves.statement is not 64 lowercase hex digits");
        }
        (!crate::is_lower_hex(&self.change, 32))
            .then_some("approves.change is not a full change id in 32 lowercase hex digits")
    }
}

/// What a sign-off approval binds: the issuing authority, the session key,
/// the identity, the session, and what it approves. The actor is always
/// `human`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApprovalFor<'a> {
    /// The issuing authority's key id.
    pub authority: &'a KeyId,
    /// The session key's id.
    pub key: &'a KeyId,
    /// The identity's id, `idn_` and 16 hex digits.
    pub identity: &'a str,
    /// The session's id, `ses_` and 16 hex digits.
    pub session: &'a str,
    /// What the touch approves.
    pub approves: &'a Approves,
}

/// The approval challenge's payload: [`APPROVAL_PREFIX`], then the
/// authority's key id, the session key's id, the identity id, `human`, the
/// session, the statement's SHA-256 and the change, each followed by a
/// newline.
pub fn approval_payload(approval: &ApprovalFor<'_>) -> Vec<u8> {
    format!(
        "{APPROVAL_PREFIX}{authority}\n{key}\n{identity}\nhuman\n{session}\n{statement}\n{change}\n",
        authority = approval.authority,
        key = approval.key,
        identity = approval.identity,
        session = approval.session,
        statement = approval.approves.statement,
        change = approval.approves.change,
    )
    .into_bytes()
}

/// The `WebAuthn` challenge of a sign-off's assertion: SHA-256 of the
/// [`pae`] of [`APPROVAL_PAYLOAD_TYPE`] and [`approval_payload`]. Derived,
/// so a verifier recomputes it from the certificate.
pub fn approval_challenge(approval: &ApprovalFor<'_>) -> [u8; 32] {
    sha256(&pae(APPROVAL_PAYLOAD_TYPE, &approval_payload(approval)))
}

/// A change to an identity's passkeys that one of its existing passkeys
/// approves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasskeyChange<'a> {
    /// Adding the passkey this registration challenge was issued for.
    Add {
        /// The registration's challenge.
        registration_challenge: &'a [u8; 32],
    },
    /// Removing a passkey, by its id (`pk_` and 16 hex digits).
    Remove {
        /// The passkey's id.
        passkey: &'a str,
    },
}

/// The passkey change challenge's payload: `add\n<the registration
/// challenge, unpadded base64url>\n` or `remove\n<passkey id>\n`.
pub fn passkey_change_payload(change: PasskeyChange<'_>) -> Vec<u8> {
    match change {
        PasskeyChange::Add {
            registration_challenge,
        } => format!("add\n{}\n", URL_SAFE_NO_PAD.encode(registration_challenge)),
        PasskeyChange::Remove { passkey } => format!("remove\n{passkey}\n"),
    }
    .into_bytes()
}

/// The `WebAuthn` challenge of an assertion approving `change`: SHA-256 of
/// the [`pae`] of [`PASSKEY_CHANGE_PAYLOAD_TYPE`] and
/// [`passkey_change_payload`]. Checked with [`check_assertion`].
pub fn passkey_change_challenge(change: PasskeyChange<'_>) -> [u8; 32] {
    sha256(&pae(
        PASSKEY_CHANGE_PAYLOAD_TYPE,
        &passkey_change_payload(change),
    ))
}

/// A `user_verified` certificate's evidence: the passkey, what its one
/// assertion approves, and the assertion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresenceEvidence {
    /// The passkey's public key, whose fingerprint the predicate's
    /// `passkey` names.
    pub passkey: PasskeyKey,
    /// The passkey's credential id, for display and lookup; the assertion
    /// does not sign it.
    pub credential_id: Vec<u8>,
    /// The RP ID: `localhost` for the web listener's.
    pub rp_id: String,
    /// What the assertion approves.
    pub approves: Approves,
    /// The assertion.
    pub assertion: Assertion,
}

impl PresenceEvidence {
    /// Checks the assertion over the challenge derived from the
    /// certificate's authority, key, identity id and session, and this
    /// evidence's `approves`.
    pub fn check(
        &self,
        authority: &KeyId,
        key: &KeyId,
        identity: &str,
        session: &str,
    ) -> Result<Asserted, AssertionError> {
        let challenge = approval_challenge(&ApprovalFor {
            authority,
            key,
            identity,
            session,
            approves: &self.approves,
        });
        check_assertion(&self.passkey, &self.rp_id, &challenge, &self.assertion)
    }
}

/// Whether `rp_id` is a domain as `WebAuthn`'s RP IDs are: lowercase labels
/// of letters, digits and `-`, joined by dots, at most 253 bytes.
pub fn is_rp_id(rp_id: &str) -> bool {
    rp_id.len() <= 253
        && rp_id.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// The client data's origin, once its type, challenge and origin pass.
fn client_data(
    json: &[u8],
    ceremony: &str,
    challenge: &[u8; 32],
    rp_id: &str,
) -> Result<String, AssertionError> {
    let Ok(Value::Object(data)) = serde_json::from_slice::<Value>(json) else {
        return refuse(Check::Type, "clientDataJSON is not a JSON object");
    };
    match data.get("type") {
        Some(Value::String(found)) if found == ceremony => {}
        found => {
            return refuse(
                Check::Type,
                format!("the client data's type is {found:?}, not {ceremony:?}"),
            );
        }
    }
    let expected = URL_SAFE_NO_PAD.encode(challenge);
    match data.get("challenge") {
        Some(Value::String(found)) if *found == expected => {}
        found => {
            return refuse(
                Check::Challenge,
                format!("the client data's challenge is {found:?}, not {expected:?}"),
            );
        }
    }
    let Some(Value::String(origin)) = data.get("origin") else {
        return refuse(Check::Origin, "the client data has no origin");
    };
    if let Some(problem) = origin_problem(origin, rp_id) {
        return refuse(Check::Origin, format!("{origin:?}: {problem}"));
    }
    if data.get("crossOrigin") == Some(&Value::Bool(true)) {
        return refuse(Check::Origin, "the ceremony ran in a cross-origin frame");
    }
    Ok(origin.clone())
}

/// Why `origin` is not one of `rp_id`'s, if it is not: its host must be
/// the RP ID, with an optional port, over `https`, or `http` for
/// `localhost`, which browsers treat as a secure context.
fn origin_problem(origin: &str, rp_id: &str) -> Option<String> {
    let Some((scheme, authority)) = origin.split_once("://") else {
        return Some("not scheme://host".into());
    };
    let host = match authority.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit())
                && port.parse::<u16>().is_ok_and(|port| port != 0) =>
        {
            host
        }
        Some(_) => return Some("its port is not 1 to 65535".into()),
        None => authority,
    };
    if host != rp_id {
        return Some(format!("its host is not the RP ID {rp_id:?}"));
    }
    match scheme {
        "https" => None,
        "http" if host == "localhost" => None,
        _ => Some("a scheme other than https, or http for localhost".into()),
    }
}

/// The parts of `authenticatorData` a check reads.
struct AuthenticatorData {
    flags: u8,
    sign_count: u32,
}

/// `bytes` as authenticator data for `rp_id`, with UP and UV set.
fn parse_authenticator_data(
    bytes: &[u8],
    rp_id: &str,
) -> Result<AuthenticatorData, AssertionError> {
    if !is_rp_id(rp_id) {
        return refuse(Check::Rp, format!("the RP ID {rp_id:?} is not a domain"));
    }
    let Some((rp_hash, rest)) = bytes.split_first_chunk::<32>() else {
        return refuse(
            Check::Rp,
            format!(
                "authenticatorData is {} bytes; it is at least {AUTHENTICATOR_DATA_LEN}",
                bytes.len()
            ),
        );
    };
    let Some(([flags], rest)) = rest.split_first_chunk::<1>() else {
        return refuse(Check::Rp, "authenticatorData has no flags");
    };
    let Some((count, _)) = rest.split_first_chunk::<4>() else {
        return refuse(Check::Rp, "authenticatorData has no sign count");
    };
    if *rp_hash != sha256(rp_id.as_bytes()) {
        return refuse(
            Check::Rp,
            format!("authenticatorData's RP ID hash is not SHA-256 of {rp_id:?}"),
        );
    }
    if flags & FLAG_UP == 0 {
        return refuse(Check::Flags, "the user present flag is not set");
    }
    if flags & FLAG_UV == 0 {
        return refuse(
            Check::Flags,
            "the user verified flag is not set: the authenticator did not verify the person",
        );
    }
    Ok(AuthenticatorData {
        flags: *flags,
        sign_count: u32::from_be_bytes(*count),
    })
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = [0; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// One DER element of `input`: its tag, its contents and what follows. A
/// length must be definite, minimal and at most two bytes.
fn element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = match first {
        0..=0x7f => (usize::from(first), rest),
        0x81 => {
            let (&len, rest) = rest.split_first()?;
            (len >= 0x80).then_some((usize::from(len), rest))?
        }
        0x82 => {
            let (len, rest) = rest.split_first_chunk::<2>()?;
            let len = u16::from_be_bytes(*len);
            (len >= 0x100).then_some((usize::from(len), rest))?
        }
        _ => return None,
    };
    let (contents, rest) = rest.split_at_checked(len)?;
    Some((tag, contents, rest))
}

/// The contents of `input` when it is exactly one element tagged `tag`.
fn only(input: &[u8], tag: u8) -> Option<&[u8]> {
    match element(input)? {
        (found, contents, []) if found == tag => Some(contents),
        _ => None,
    }
}

/// A `SubjectPublicKeyInfo`'s algorithm OID, its parameters' DER (empty
/// when it has none), and its key bits.
fn spki_parts(spki: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let info = only(spki, TAG_SEQUENCE)?;
    let (TAG_SEQUENCE, algorithm, key) = element(info)? else {
        return None;
    };
    let (TAG_OID, oid, parameters) = element(algorithm)? else {
        return None;
    };
    let [0, key @ ..] = only(key, TAG_BIT_STRING)? else {
        return None;
    };
    Some((oid, parameters, key))
}

/// The bit length of a DER `RSAPublicKey`'s modulus, when it is
/// `SEQUENCE { INTEGER n, INTEGER e }` with positive, minimal integers and
/// an odd exponent of at least 3.
fn rsa_modulus_bits(key: &[u8]) -> Option<usize> {
    let (n, e) = rsa_integers(key)?;
    let odd = e.last().is_some_and(|b| b & 1 == 1);
    let at_least_3 = e.len() > 1 || e.first().is_some_and(|&b| b >= 3);
    let (&top, _) = n.split_first()?;
    let unused = usize::try_from(top.leading_zeros()).ok()?;
    (odd && at_least_3).then(|| n.len() * 8 - unused)
}

/// A DER `RSAPublicKey`'s modulus and exponent, big-endian with no leading
/// zero.
fn rsa_integers(key: &[u8]) -> Option<(&[u8], &[u8])> {
    let sequence = only(key, TAG_SEQUENCE)?;
    let (TAG_INTEGER, n, rest) = element(sequence)? else {
        return None;
    };
    let e = only(rest, TAG_INTEGER)?;
    Some((positive(n)?, positive(e)?))
}

/// A minimal positive DER integer's magnitude.
fn positive(integer: &[u8]) -> Option<&[u8]> {
    match integer {
        [0, rest @ ..] if rest.first().is_some_and(|b| b & 0x80 != 0) => Some(rest),
        [first, ..] if first & 0x80 == 0 && *first != 0 => Some(integer),
        _ => None,
    }
}
