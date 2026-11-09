//! Session-key certificates (R3.03, ADR 0057): an issuing authority's
//! signed statement that a short-lived ed25519 key is bound to an operator
//! identity, for a declared actor, in a session, for a validity window.
//!
//! A certificate is an in-toto Statement v1 in the same DSSE envelope as
//! evidence, signed once by the authority, with the same PAE, canonical
//! form, duplicate-key and depth checks, and key rules (ADR 0055). Only its
//! subject and predicate differ:
//!
//! ```json
//! {"_type":"https://in-toto.io/Statement/v1",
//!  "subject":[{"name":"session-key","digest":{"sha256":"<the session key's key id>"}}],
//!  "predicateType":"urn:reef-vcs:session-key:v1",
//!  "predicate":{"publicKey":"MCowBQYDK2VwAyEA…",
//!               "identity":{"id":"idn_8f3a0c2d91e4b675","name":"alice","kind":"person"},
//!               "actor":"agent","agent":"builder-3","session":"ses_41c9e07ab2d35f18",
//!               "presence":"none","custody":"client",
//!               "notBefore":"2026-11-06T10:00:00.000Z","notAfter":"2026-11-06T11:00:00.000Z",
//!               "issuer":{"kind":"reef-daemon","keyId":"<the authority's key id>","host":"mbp.local"}}}
//! ```
//!
//! Every function here is pure: no clock, no database, no I/O. The caller
//! supplies the authorities it knows ([`check_certificate`]), the time the
//! evidence was first witnessed, and the revocations that apply
//! ([`status_at`]). A certificate says which identity a key is bound to and
//! what the requester declared it for; it never says the identity should be
//! trusted (R3.07).

use std::collections::BTreeMap;
use std::time::{Duration, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::envelope::{Envelope, decode_base64, encode_base64};
use crate::keys::{KeyId, PublicKey, SigningKey};
use crate::statement::{ResourceDescriptor, format_time, parse_time};
use crate::verify::{Opened, predicate_error, statement_value};
use crate::{Invalid, PAYLOAD_TYPE, STATEMENT_TYPE, pae};

/// The certificate's predicate type.
pub const SESSION_KEY_PREDICATE_TYPE: &str = "urn:reef-vcs:session-key:v1";

/// The subject's `name`, which must be exactly this.
pub const SUBJECT_NAME: &str = "session-key";

/// The DSSE payload type of a proof of possession: what a requester signs
/// with the session key to show it holds it.
pub const PROOF_PAYLOAD_TYPE: &str = "application/vnd.reef-vcs.session-key-proof";

/// The first line of a proof's payload.
pub const PROOF_PREFIX: &str = "reef-session-key-proof/v1\n";

/// The longest `agent`, in bytes: the API's `reef-agent` bound
/// (`reef_api::AGENT_MAX_BYTES`), which the daemon's tests compare.
pub const MAX_AGENT_BYTES: usize = 256;

/// The longest `issuer.host`, in bytes.
pub const MAX_HOST_BYTES: usize = 255;

/// A string enumeration of the predicate, with its JSON value.
macro_rules! vocabulary {
    ($(#[$doc:meta])* $name:ident { $($(#[$vdoc:meta])* $variant:ident = $text:literal,)* }) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name {
            $($(#[$vdoc])* $variant,)*
        }

        impl $name {
            /// Every value this reader knows.
            pub const ALL: &'static [$name] = &[$($name::$variant,)*];

            /// The value as the predicate writes it.
            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text,)*
                }
            }

            /// The value `text` names; `None` for one this reader does not
            /// know.
            pub fn parse(text: &str) -> Option<$name> {
                $name::ALL.iter().copied().find(|value| value.as_str() == text)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

vocabulary! {
    /// What an operator identity is.
    IdentityKind {
        /// A human operator.
        Person = "person",
        /// A CI pipeline.
        Ci = "ci",
    }
}

vocabulary! {
    /// What the requester declared a key is for. Declared, not proven.
    Actor {
        /// The person signs directly, such as a sign-off.
        Human = "human",
        /// An agent acting for the operator.
        Agent = "agent",
        /// An automated check runner.
        Executor = "executor",
    }
}

vocabulary! {
    /// How a human's presence was established when the key was issued.
    /// R3.03 issues only `none`; R3.43 adds `user_verified`. A value this
    /// reader does not know makes the certificate invalid for it.
    Presence {
        /// Not established: the daemon's user, or anything running as that
        /// user, asked for the key.
        None = "none",
    }
}

vocabulary! {
    /// Where the session key's private half lives.
    Custody {
        /// In the process that requested it, which generated it.
        Client = "client",
        /// In the daemon's memory (R3.04).
        Daemon = "daemon",
    }
}

vocabulary! {
    /// Who issues certificates. R4.10 adds `reef-store`.
    IssuerKind {
        /// A Reef daemon's authority key.
        ReefDaemon = "reef-daemon",
        /// Sigstore's Fulcio, against an OIDC identity (R3.42, ADR 0066):
        /// the key's certificate is a Sigstore bundle, checked by
        /// `crate::sigstore::check_sigstore_bundle`, never by
        /// [`check_certificate`].
        Sigstore = "sigstore",
    }
}

/// The prefixes of ids, which no name may start with, so a text names an
/// identity or a session by one rule: `idn_` and `ses_`.
pub const RESERVED_PREFIXES: [&str; 2] = ["idn_", "ses_"];

/// Whether `name` is an identity name: `[a-z0-9][a-z0-9._-]{0,62}`, the
/// remote tokens' pattern, not starting with a [`RESERVED_PREFIXES`] one.
pub fn is_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.len() <= 63
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-_".contains(&b))
        && !RESERVED_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// Whether `text` is `<prefix>` and 16 lowercase hex digits.
fn is_prefixed_id(text: &str, prefix: &str) -> bool {
    text.strip_prefix(prefix)
        .is_some_and(|rest| crate::is_lower_hex(rest, 16))
}

/// Whether `id` is an identity id: `idn_` and 16 lowercase hex digits.
pub fn is_identity_id(id: &str) -> bool {
    is_prefixed_id(id, "idn_")
}

/// Whether `id` is a session id: `ses_` and 16 lowercase hex digits.
pub fn is_session_id(id: &str) -> bool {
    is_prefixed_id(id, "ses_")
}

/// Why `agent` is not one, if it is not: visible ASCII and spaces, 1 to
/// [`MAX_AGENT_BYTES`] bytes.
pub fn agent_problem(agent: &str) -> Option<String> {
    if agent.is_empty() || agent.len() > MAX_AGENT_BYTES {
        return Some(format!(
            "{} bytes; an agent label is 1 to {MAX_AGENT_BYTES} bytes",
            agent.len()
        ));
    }
    (!agent.bytes().all(|b| b.is_ascii_graphic() || b == b' '))
        .then(|| "an agent label is visible ASCII".to_owned())
}

/// The identity a key is bound to, as the certificate names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityRef {
    /// `idn_` and 16 hex digits, never reused.
    pub id: String,
    /// The identity's name, never reused.
    pub name: String,
    /// `person` or `ci`.
    pub kind: IdentityKind,
}

/// Who issued a certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issuer {
    /// What kind of issuer.
    pub kind: IssuerKind,
    /// The authority key's id, which is the envelope's `keyid`.
    pub key_id: KeyId,
    /// The issuing host, for display only.
    pub host: String,
}

/// What a certificate binds: everything its predicate says, as values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    /// The session key.
    pub public_key: PublicKey,
    /// The identity it answers to.
    pub identity: IdentityRef,
    /// What the requester declared it for.
    pub actor: Actor,
    /// The issuing request's agent label, declared and unverified.
    pub agent: Option<String>,
    /// The session, `ses_` and 16 hex digits.
    pub session: String,
    /// How a human's presence was established.
    pub presence: Presence,
    /// Where the private key lives.
    pub custody: Custody,
    /// The start of the validity window, unix milliseconds.
    pub not_before_ms: i64,
    /// Its end, unix milliseconds, inclusive.
    pub not_after_ms: i64,
    /// Who issued it.
    pub issuer: Issuer,
}

/// The certificate's Statement, as JSON reads and writes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "evidence.SessionKeyStatement")]
pub struct SessionKeyStatement {
    /// `https://in-toto.io/Statement/v1`.
    #[serde(rename = "_type")]
    #[schemars(extend("const" = "https://in-toto.io/Statement/v1"))]
    pub type_: String,
    /// Exactly one: `{"name": "session-key", "digest": {"sha256": <the
    /// session key's id>}}`.
    #[schemars(length(min = 1, max = 1))]
    pub subject: Vec<ResourceDescriptor>,
    /// `urn:reef-vcs:session-key:v1`.
    #[serde(rename = "predicateType")]
    #[schemars(extend("const" = "urn:reef-vcs:session-key:v1"))]
    pub predicate_type: String,
    /// What the authority binds the key to.
    pub predicate: SessionKeyPredicate,
}

/// Reef's session-key predicate (R3.03, ADR 0057). Fields are only ever
/// added (ADR 0055's rules); a reader keeps the ones it does not know in
/// [`SessionKeyPredicate::unknown`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename = "evidence.SessionKeyPredicate")]
pub struct SessionKeyPredicate {
    /// The session key's DER `SubjectPublicKeyInfo`, standard base64: an
    /// ed25519 key that is neither small-order nor non-canonical, whose key
    /// id is the subject's digest.
    pub public_key: String,
    /// The operator identity the key answers to.
    pub identity: CertificateIdentity,
    /// What the requester declared the key for: `human` only with a
    /// `person` identity.
    #[schemars(extend("enum" = ["human", "agent", "executor"]))]
    pub actor: String,
    /// The issuing request's agent label: declared, never verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 256))]
    pub agent: Option<String>,
    /// The session, `ses_` and 16 lowercase hex digits; a session's
    /// successive keys share it.
    #[schemars(regex(pattern = r"^ses_[0-9a-f]{16}$"))]
    pub session: String,
    /// How a human's presence was established: `none` (not verified). A
    /// value a reader does not know makes the certificate invalid for it.
    #[schemars(extend("enum" = ["none"]))]
    pub presence: String,
    /// Where the private key lives: `client` (the requesting process) or
    /// `daemon`.
    #[schemars(extend("enum" = ["client", "daemon"]))]
    pub custody: String,
    /// The start of the validity window: RFC 3339 in UTC with `Z`.
    pub not_before: String,
    /// Its end, after `notBefore`. Validity is judged at the time the
    /// verifier first witnessed the evidence, not at a time it claims.
    pub not_after: String,
    /// Who issued the certificate.
    pub issuer: CertificateIssuer,
    /// Fields a later Reef added that this one does not know.
    #[serde(flatten)]
    pub unknown: BTreeMap<String, Value>,
}

/// A keyless session key's binding statement (R3.42, ADR 0066): what the
/// key Fulcio certified signs about itself, which Rekor logs. Its predicate
/// type is [`SIGSTORE_BINDING_PREDICATE_TYPE`]; `crate::sigstore` (feature
/// `sigstore`) checks it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "evidence.SigstoreBindingStatement")]
pub struct SigstoreBindingStatement {
    /// `https://in-toto.io/Statement/v1`.
    #[serde(rename = "_type")]
    #[schemars(extend("const" = "https://in-toto.io/Statement/v1"))]
    pub type_: String,
    /// Exactly one: `{"name": "session-key", "digest": {"sha256": <the
    /// certified key's id>}}`.
    #[schemars(length(min = 1, max = 1))]
    pub subject: Vec<ResourceDescriptor>,
    /// `urn:reef-vcs:sigstore-session-key:v1`.
    #[serde(rename = "predicateType")]
    #[schemars(extend("const" = "urn:reef-vcs:sigstore-session-key:v1"))]
    pub predicate_type: String,
    /// What the key declares about itself.
    pub predicate: SigstoreBindingPredicate,
}

/// A binding statement's predicate (R3.42). The identity is not here: the
/// verifier matches the Fulcio certificate's OIDC issuer and subject, and
/// the window is the certificate's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "evidence.SigstoreBindingPredicate")]
pub struct SigstoreBindingPredicate {
    /// What the key is for: `human` only for a `person` identity.
    #[schemars(extend("enum" = ["human", "agent", "executor"]))]
    pub actor: String,
    /// The signer's agent label: declared, never verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 256))]
    pub agent: Option<String>,
    /// The session, chosen by the client for its first key and repeated by
    /// its renewals: `ses_` and 16 lowercase hex digits.
    #[schemars(regex(pattern = r"^ses_[0-9a-f]{16}$"))]
    pub session: String,
    /// `none`: an OIDC login is not a verification of each use.
    #[schemars(extend("enum" = ["none"]))]
    pub presence: String,
    /// `client`: the key lives in the process Fulcio certified it for.
    #[schemars(extend("enum" = ["client"]))]
    pub custody: String,
}

/// The binding statement's predicate type (R3.42).
pub const SIGSTORE_BINDING_PREDICATE_TYPE: &str = "urn:reef-vcs:sigstore-session-key:v1";

/// The identity a certificate names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "evidence.CertificateIdentity")]
pub struct CertificateIdentity {
    /// `idn_` and 16 lowercase hex digits.
    #[schemars(regex(pattern = r"^idn_[0-9a-f]{16}$"))]
    pub id: String,
    /// The identity's name: `[a-z0-9][a-z0-9._-]{0,62}`.
    #[schemars(regex(pattern = r"^[a-z0-9][a-z0-9._-]{0,62}$"))]
    pub name: String,
    /// `person` or `ci`.
    #[schemars(extend("enum" = ["person", "ci"]))]
    pub kind: String,
}

/// The issuer a certificate names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename = "evidence.CertificateIssuer")]
pub struct CertificateIssuer {
    /// `reef-daemon`.
    #[schemars(extend("enum" = ["reef-daemon"]))]
    pub kind: String,
    /// The authority key's id, which must be the envelope's `keyid`.
    #[schemars(regex(pattern = r"^[0-9a-f]{64}$"))]
    pub key_id: String,
    /// The issuing host, for display.
    pub host: String,
}

/// `ms` as the predicate writes a time.
fn time_text(field: &str, ms: i64) -> Result<String, Invalid> {
    let ms = u64::try_from(ms).map_err(|_| predicate_error(field, "before 1970"))?;
    format_time(field, UNIX_EPOCH + Duration::from_millis(ms))
}

impl Binding {
    /// The binding's Statement.
    pub fn statement(&self) -> Result<SessionKeyStatement, Invalid> {
        Ok(SessionKeyStatement {
            type_: STATEMENT_TYPE.to_owned(),
            subject: vec![ResourceDescriptor {
                name: Some(SUBJECT_NAME.to_owned()),
                uri: None,
                digest: BTreeMap::from([("sha256".to_owned(), self.public_key.id().to_string())]),
                media_type: None,
                annotations: None,
            }],
            predicate_type: SESSION_KEY_PREDICATE_TYPE.to_owned(),
            predicate: SessionKeyPredicate {
                public_key: encode_base64(self.public_key.spki()),
                identity: CertificateIdentity {
                    id: self.identity.id.clone(),
                    name: self.identity.name.clone(),
                    kind: self.identity.kind.as_str().to_owned(),
                },
                actor: self.actor.as_str().to_owned(),
                agent: self.agent.clone(),
                session: self.session.clone(),
                presence: self.presence.as_str().to_owned(),
                custody: self.custody.as_str().to_owned(),
                not_before: time_text("notBefore", self.not_before_ms)?,
                not_after: time_text("notAfter", self.not_after_ms)?,
                issuer: CertificateIssuer {
                    kind: self.issuer.kind.as_str().to_owned(),
                    key_id: self.issuer.key_id.to_string(),
                    host: self.issuer.host.clone(),
                },
                unknown: BTreeMap::new(),
            },
        })
    }

    /// The payload: the statement as compact JSON in the order of the
    /// module documentation, so one binding is always the same bytes.
    pub fn payload(&self) -> Result<Vec<u8>, Invalid> {
        // Strings and maps of strings always serialize.
        Ok(serde_json::to_vec(&self.statement()?).unwrap_or_default())
    }
}

/// Issues a certificate: `authority` signs `binding`, whose issuer must name
/// it. A binding that [`check_certificate`] would refuse is refused here, so
/// an authority never signs one.
pub fn certify(authority: &SigningKey, binding: &Binding) -> Result<Envelope, Invalid> {
    let key_id = authority.public_key().id();
    if &binding.issuer.key_id != key_id {
        return Err(predicate_error(
            "issuer",
            format!(
                "keyId {} is not the signing authority's {key_id}",
                binding.issuer.key_id
            ),
        ));
    }
    let payload = binding.payload()?;
    parse_certificate(&payload, key_id)?;
    Ok(authority.sign(PAYLOAD_TYPE, &payload))
}

/// A revocation: from when signatures stop counting, and when it was made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revocation {
    /// When it was made, unix milliseconds.
    pub revoked_at_ms: i64,
    /// From when signatures stop counting, never after `revoked_at_ms`.
    pub invalid_from_ms: i64,
    /// Why, as the revoker said.
    pub reason: Option<String>,
}

/// An issuing authority a verifier knows: its public key, its kind, and its
/// revocation, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authority {
    /// The authority's public key.
    pub key: PublicKey,
    /// What kind of issuer it is.
    pub kind: IssuerKind,
    /// Its revocation, which applies to everything it issued.
    pub revocation: Option<Revocation>,
}

/// The revocations that apply to a certified key beside its authority's:
/// the key's own, its session's, its identity's, and, for a keyless key
/// (R3.42), the OIDC match it was bound through.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Revocations {
    /// The key's.
    pub key: Option<Revocation>,
    /// Its session's.
    pub session: Option<Revocation>,
    /// Its identity's.
    pub identity: Option<Revocation>,
    /// The binding's: the OIDC match a keyless key was bound to its
    /// identity through (R3.42); `None` for a daemon-issued key.
    pub binding: Option<Revocation>,
}

/// The earliest of `revocations` and `authority`'s, by `invalid_from`.
pub fn earliest_revocation<'a>(
    revocations: &'a Revocations,
    authority: Option<&'a Revocation>,
) -> Option<&'a Revocation> {
    [
        revocations.key.as_ref(),
        revocations.session.as_ref(),
        revocations.identity.as_ref(),
        revocations.binding.as_ref(),
        authority,
    ]
    .into_iter()
    .flatten()
    .min_by_key(|revocation| revocation.invalid_from_ms)
}

/// A certificate that verifies against a known authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Certified {
    /// What it binds.
    pub binding: Binding,
    /// SHA-256 of [`Certified::canonical`], lowercase hex. Reported, never a
    /// key: the session key's id names a certificate.
    pub id: String,
    /// The canonical envelope.
    pub canonical: Vec<u8>,
    /// The payload, as signed.
    pub payload: Vec<u8>,
    /// The authority it verified against.
    pub authority: Authority,
}

impl Certified {
    /// The earliest applicable revocation, by `invalid_from`: the key's, its
    /// session's, its identity's or its authority's.
    pub fn revocation<'a>(&'a self, revocations: &'a Revocations) -> Option<&'a Revocation> {
        earliest_revocation(revocations, self.authority.revocation.as_ref())
    }
}

/// Checks a certificate in ADR 0055's order: the size and the envelope; the
/// authority found by `keyid` through `authorities`; the signature over the
/// PAE; and only then the statement and the predicate.
pub fn check_certificate(
    envelope: &[u8],
    authorities: &dyn Fn(&KeyId) -> Option<Authority>,
) -> Result<Certified, Invalid> {
    let opened = Opened::parse(envelope)?;
    let unknown = || Invalid::UnknownAuthority {
        key_id: opened.keyid.clone(),
    };
    let key_id = KeyId::parse(&opened.keyid).ok_or_else(unknown)?;
    let authority = authorities(&key_id).ok_or_else(unknown)?;
    opened.verify(&authority.key)?;
    let binding = parse_certificate(&opened.payload, authority.key.id())?;
    if binding.issuer.kind != authority.kind {
        return Err(predicate_error(
            "issuer",
            format!(
                "kind {} is not the authority's, {}",
                binding.issuer.kind, authority.kind
            ),
        ));
    }
    let canonical = opened.canonical(&authority.key);
    Ok(Certified {
        binding,
        id: crate::sha256_hex(&canonical),
        canonical,
        payload: opened.payload,
        authority,
    })
}

/// What a verifier reports about the key that signed evidence (the API's
/// `IdentityStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IdentityStatus {
    /// No certificate for the key: a bare key.
    Unbound,
    /// The certificate verifies; the evidence was first witnessed within
    /// its window and before every applicable `invalid_from`.
    Bound,
    /// The certificate verifies, but the evidence was first witnessed
    /// outside its window.
    Expired,
    /// The certificate verifies, but the evidence was first witnessed at or
    /// after an applicable `invalid_from`.
    Revoked,
    /// The certificate does not verify.
    Invalid,
}

impl IdentityStatus {
    /// The status's name: `unbound`, `bound`, `expired`, `revoked` or
    /// `invalid`.
    pub fn as_str(self) -> &'static str {
        match self {
            IdentityStatus::Unbound => "unbound",
            IdentityStatus::Bound => "bound",
            IdentityStatus::Expired => "expired",
            IdentityStatus::Revoked => "revoked",
            IdentityStatus::Invalid => "invalid",
        }
    }
}

/// What a verifier reports for evidence signed by `certified`'s key, first
/// witnessed at `witnessed_at_ms`: `Revoked` at or after the earliest
/// applicable `invalid_from`, else `Expired` outside `[notBefore,
/// notAfter]`, else `Bound`. A revocation whose `invalid_from` is after the
/// witness time leaves it `Bound`; [`Certified::revocation`] reports it.
pub fn status_at(
    certified: &Certified,
    witnessed_at_ms: i64,
    revocations: &Revocations,
) -> IdentityStatus {
    status_of(
        &certified.binding,
        certified.authority.revocation.as_ref(),
        witnessed_at_ms,
        revocations,
    )
}

/// [`status_at`] for a binding verified by any issuer: `binding`'s window,
/// `revocations` and its authority's revocation, if any. A keyless key's
/// binding (R3.42) is judged here, with its Fulcio window.
pub fn status_of(
    binding: &Binding,
    authority: Option<&Revocation>,
    witnessed_at_ms: i64,
    revocations: &Revocations,
) -> IdentityStatus {
    if earliest_revocation(revocations, authority)
        .is_some_and(|revocation| witnessed_at_ms >= revocation.invalid_from_ms)
    {
        return IdentityStatus::Revoked;
    }
    if witnessed_at_ms < binding.not_before_ms || witnessed_at_ms > binding.not_after_ms {
        return IdentityStatus::Expired;
    }
    IdentityStatus::Bound
}

/// What a proof of possession is for: the issuing authority it is
/// addressed to, and the identity, actor and session the request names,
/// each as sent (empty for none).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProofFor<'a> {
    /// The key id of the authority the request is sent to: the audience, so
    /// a proof made for one daemon's authority is no good at another's, or
    /// after a rotation.
    pub authority: &'a KeyId,
    /// The identity named; empty for the default.
    pub identity: &'a str,
    /// The declared actor.
    pub actor: Actor,
    /// The session continued; empty for a new one.
    pub session: &'a str,
}

/// The proof of possession's payload, which the requester signs with the
/// session key inside the PAE of [`PROOF_PAYLOAD_TYPE`]: [`PROOF_PREFIX`],
/// then the authority's key id, the session key's id, the identity, the
/// actor and the session, each followed by a newline. Binding the audience,
/// the identity, the actor and the session stops a proof being replayed for
/// another authority or another binding.
pub fn proof_payload(key_id: &KeyId, proof: &ProofFor<'_>) -> Vec<u8> {
    format!(
        "{PROOF_PREFIX}{authority}\n{key_id}\n{identity}\n{actor}\n{session}\n",
        authority = proof.authority,
        identity = proof.identity,
        actor = proof.actor.as_str(),
        session = proof.session,
    )
    .into_bytes()
}

/// `key`'s proof of possession for `proof`.
pub fn sign_proof(key: &SigningKey, proof: &ProofFor<'_>) -> Vec<u8> {
    let payload = proof_payload(key.public_key().id(), proof);
    key.sign_bytes(&pae(PROOF_PAYLOAD_TYPE, &payload))
}

/// Whether `signature` is `key`'s proof of possession for `proof`.
pub fn verify_proof(key: &PublicKey, proof: &ProofFor<'_>, signature: &[u8]) -> bool {
    let payload = proof_payload(key.id(), proof);
    key.verify(&pae(PROOF_PAYLOAD_TYPE, &payload), signature)
}

/// The binding in a certificate's payload, signed by the authority
/// `key_id`, checked field by field.
fn parse_certificate(payload: &[u8], key_id: &KeyId) -> Result<Binding, Invalid> {
    let value = statement_value(payload)?;
    let Value::Object(root) = &value else {
        return Err(Invalid::Statement("it is not a JSON object".into()));
    };
    match root.get("predicateType") {
        Some(Value::String(t)) if t == SESSION_KEY_PREDICATE_TYPE => {}
        Some(Value::String(t)) => {
            return Err(Invalid::Statement(format!(
                "predicateType is {t:?}, not {SESSION_KEY_PREDICATE_TYPE:?}"
            )));
        }
        _ => {
            return Err(Invalid::Statement(
                "predicateType is missing or not a string".into(),
            ));
        }
    }
    let digest = subject_digest(root.get("subject"))?;
    let Some(Value::Object(predicate)) = root.get("predicate") else {
        return Err(Invalid::Predicate {
            field: String::new(),
            message: "the predicate is missing or not a JSON object".into(),
        });
    };
    let binding = binding_of(predicate)?;
    if digest != binding.public_key.id().as_str() {
        return Err(Invalid::Subject(format!(
            "the subject's sha256 {digest} is not publicKey's key id {}",
            binding.public_key.id()
        )));
    }
    if &binding.issuer.key_id != key_id {
        return Err(predicate_error(
            "issuer",
            format!(
                "keyId {} is not the envelope's keyid {key_id}",
                binding.issuer.key_id
            ),
        ));
    }
    Ok(binding)
}

/// The subject's `sha256`: exactly one subject, named `session-key`, whose
/// digest is only a `sha256` of 64 lowercase hex digits.
pub(crate) fn subject_digest(subject: Option<&Value>) -> Result<&str, Invalid> {
    let bad = |message: String| Err(Invalid::Subject(message));
    let Some(Value::Array(subjects)) = subject else {
        return bad("subject is missing or not an array".into());
    };
    let [Value::Object(only)] = subjects.as_slice() else {
        return bad(format!(
            "{} subjects; a certificate is about exactly one key",
            subjects.len()
        ));
    };
    match only.get("name") {
        Some(Value::String(name)) if name == SUBJECT_NAME => {}
        _ => return bad(format!("the subject's name must be {SUBJECT_NAME:?}")),
    }
    let Some(Value::Object(digest)) = only.get("digest") else {
        return bad("the subject has no digest object".into());
    };
    if let Some(other) = digest.keys().find(|key| *key != "sha256") {
        return bad(format!(
            "the subject's digest has {other:?}; it has only sha256"
        ));
    }
    match digest.get("sha256") {
        Some(Value::String(sha256)) if crate::is_lower_hex(sha256, 64) => Ok(sha256),
        _ => bad("the subject's sha256 is not 64 lowercase hex digits".into()),
    }
}

/// A string field of `object`, or the error for `field`.
pub(crate) fn string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    field: &str,
) -> Result<&'a str, Invalid> {
    match object.get(key) {
        Some(Value::String(text)) => Ok(text),
        Some(_) => Err(predicate_error(field, format!("{key} is not a string"))),
        None => Err(predicate_error(field, format!("{key} is required"))),
    }
}

/// A predicate field naming one value of a vocabulary.
pub(crate) fn known<T>(
    predicate: &Map<String, Value>,
    field: &str,
    parse: fn(&str) -> Option<T>,
    known: &[&str],
) -> Result<T, Invalid> {
    let text = string(predicate, field, field)?;
    parse(text).ok_or_else(|| {
        predicate_error(
            field,
            format!(
                "{text:?} is not one this reader knows: {}",
                known.join(", ")
            ),
        )
    })
}

pub(crate) fn names<T: Copy>(all: &[T], name: fn(T) -> &'static str) -> Vec<&'static str> {
    all.iter().map(|value| name(*value)).collect()
}

/// The predicate's fields as a binding.
fn binding_of(predicate: &Map<String, Value>) -> Result<Binding, Invalid> {
    let public_key = {
        let text = string(predicate, "publicKey", "publicKey")?;
        let der = decode_base64(text).ok_or_else(|| predicate_error("publicKey", "not base64"))?;
        PublicKey::from_spki(&der)
            .map_err(|error| predicate_error("publicKey", error.to_string()))?
    };
    let identity = identity_of(predicate)?;
    let actor = known(
        predicate,
        "actor",
        Actor::parse,
        &names(Actor::ALL, Actor::as_str),
    )?;
    if actor == Actor::Human && identity.kind == IdentityKind::Ci {
        return Err(predicate_error(
            "actor",
            "human is never certified for a ci identity",
        ));
    }
    let agent = match predicate.get("agent") {
        None => None,
        Some(Value::String(agent)) => {
            if let Some(problem) = agent_problem(agent) {
                return Err(predicate_error("agent", problem));
            }
            Some(agent.clone())
        }
        Some(_) => return Err(predicate_error("agent", "not a string")),
    };
    let session = string(predicate, "session", "session")?;
    if !is_session_id(session) {
        return Err(predicate_error(
            "session",
            format!("{session:?} is not ses_ and 16 lowercase hex digits"),
        ));
    }
    let presence = known(
        predicate,
        "presence",
        Presence::parse,
        &names(Presence::ALL, Presence::as_str),
    )?;
    let custody = known(
        predicate,
        "custody",
        Custody::parse,
        &names(Custody::ALL, Custody::as_str),
    )?;
    let time = |field: &str| -> Result<i64, Invalid> {
        let text = string(predicate, field, field)?;
        parse_time(text).ok_or_else(|| {
            predicate_error(
                field,
                "an RFC 3339 time in UTC ending in Z, such as 2026-11-06T10:00:00.000Z",
            )
        })
    };
    let not_before_ms = time("notBefore")?;
    let not_after_ms = time("notAfter")?;
    if not_after_ms <= not_before_ms {
        return Err(predicate_error("notAfter", "not after notBefore"));
    }
    let issuer = issuer_of(predicate)?;
    Ok(Binding {
        public_key,
        identity,
        actor,
        agent,
        session: session.to_owned(),
        presence,
        custody,
        not_before_ms,
        not_after_ms,
        issuer,
    })
}

/// The predicate's `identity`.
fn identity_of(predicate: &Map<String, Value>) -> Result<IdentityRef, Invalid> {
    match predicate.get("identity") {
        Some(Value::Object(identity)) => {
            let id = string(identity, "id", "identity")?;
            if !is_identity_id(id) {
                return Err(predicate_error(
                    "identity",
                    format!("id {id:?} is not idn_ and 16 lowercase hex digits"),
                ));
            }
            let name = string(identity, "name", "identity")?;
            if !is_name(name) {
                return Err(predicate_error(
                    "identity",
                    format!("name {name:?} is not [a-z0-9][a-z0-9._-]{{0,62}}"),
                ));
            }
            let kind = string(identity, "kind", "identity")?;
            let kind = IdentityKind::parse(kind).ok_or_else(|| {
                predicate_error(
                    "identity",
                    format!("kind {kind:?} is not one this reader knows: person, ci"),
                )
            })?;
            Ok(IdentityRef {
                id: id.to_owned(),
                name: name.to_owned(),
                kind,
            })
        }
        Some(_) => Err(predicate_error("identity", "not a JSON object")),
        None => Err(predicate_error("identity", "required")),
    }
}

/// The predicate's `issuer`.
fn issuer_of(predicate: &Map<String, Value>) -> Result<Issuer, Invalid> {
    match predicate.get("issuer") {
        Some(Value::Object(issuer)) => {
            let kind = string(issuer, "kind", "issuer")?;
            // A keyless key's certificate is a Sigstore bundle, never this
            // predicate (R3.42), so `sigstore` is not an issuer kind here.
            let kind = IssuerKind::parse(kind)
                .filter(|kind| *kind != IssuerKind::Sigstore)
                .ok_or_else(|| {
                    predicate_error(
                        "issuer",
                        format!("kind {kind:?} is not one this reader knows: reef-daemon"),
                    )
                })?;
            let key_id = string(issuer, "keyId", "issuer")?;
            let key_id = KeyId::parse(key_id)
                .ok_or_else(|| predicate_error("issuer", "keyId is not 64 lowercase hex digits"))?;
            let host = string(issuer, "host", "issuer")?;
            if host.len() > MAX_HOST_BYTES || host.chars().any(char::is_control) {
                return Err(predicate_error(
                    "issuer",
                    format!("host is at most {MAX_HOST_BYTES} bytes with no control characters"),
                ));
            }
            Ok(Issuer {
                kind,
                key_id,
                host: host.to_owned(),
            })
        }
        Some(_) => Err(predicate_error("issuer", "not a JSON object")),
        None => Err(predicate_error("issuer", "required")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authority() -> SigningKey {
        SigningKey::from_seed(&[3; 32]).unwrap()
    }

    fn binding(authority: &SigningKey) -> Binding {
        Binding {
            public_key: SigningKey::from_seed(&[4; 32])
                .unwrap()
                .public_key()
                .clone(),
            identity: IdentityRef {
                id: "idn_8f3a0c2d91e4b675".into(),
                name: "alice".into(),
                kind: IdentityKind::Person,
            },
            actor: Actor::Agent,
            agent: Some("builder-3".into()),
            session: "ses_41c9e07ab2d35f18".into(),
            presence: Presence::None,
            custody: Custody::Client,
            not_before_ms: 1_793_959_200_000,
            not_after_ms: 1_793_962_800_000,
            issuer: Issuer {
                kind: IssuerKind::ReefDaemon,
                key_id: authority.public_key().id().clone(),
                host: "mbp.local".into(),
            },
        }
    }

    #[test]
    fn vocabularies_round_trip() {
        for actor in Actor::ALL {
            assert_eq!(Actor::parse(actor.as_str()), Some(*actor));
        }
        assert_eq!(IssuerKind::ReefDaemon.as_str(), "reef-daemon");
        assert_eq!(Presence::parse("user_verified"), None);
        assert!(is_name("ci-main") && is_name("a") && !is_name("-a") && !is_name("A"));
        assert!(is_name(&"a".repeat(63)) && !is_name(&"a".repeat(64)));
        assert!(!is_name("idn_0123456789abcdef") && !is_name("ses_x") && is_name("idn-x"));
        assert!(is_session_id("ses_0123456789abcdef") && !is_session_id("ses_0123"));
        assert!(is_identity_id("idn_0123456789abcdef") && !is_identity_id("ses_0123456789abcdef"));
        assert_eq!(agent_problem("builder 3"), None);
        assert!(agent_problem("tab\there").is_some());
        assert!(agent_problem(&"x".repeat(MAX_AGENT_BYTES + 1)).is_some());
    }

    #[test]
    fn certify_refuses_what_check_would() {
        let authority = authority();
        let mut wrong = binding(&authority);
        wrong.issuer.key_id = wrong.public_key.id().clone();
        assert_eq!(
            certify(&authority, &wrong).unwrap_err().field(),
            Some("issuer")
        );
        let mut human_ci = binding(&authority);
        human_ci.identity.kind = IdentityKind::Ci;
        human_ci.actor = Actor::Human;
        assert_eq!(
            certify(&authority, &human_ci).unwrap_err().field(),
            Some("actor")
        );
        let mut empty = binding(&authority);
        empty.not_after_ms = empty.not_before_ms;
        assert_eq!(
            certify(&authority, &empty).unwrap_err().field(),
            Some("notAfter")
        );
    }

    #[test]
    fn the_proof_payload_is_pinned() {
        let key = SigningKey::from_seed(&[4; 32]).unwrap();
        let authority = authority().public_key().id().clone();
        let other = SigningKey::from_seed(&[5; 32])
            .unwrap()
            .public_key()
            .id()
            .clone();
        let for_ = |identity, actor, session| ProofFor {
            authority: &authority,
            identity,
            actor,
            session,
        };
        let payload = proof_payload(key.public_key().id(), &for_("alice", Actor::Executor, ""));
        assert_eq!(
            String::from_utf8(payload).unwrap(),
            format!(
                "reef-session-key-proof/v1\n{authority}\n{}\nalice\nexecutor\n\n",
                key.public_key().id()
            )
        );
        let session = "ses_0123456789abcdef";
        let proof = sign_proof(&key, &for_("", Actor::Agent, session));
        assert!(verify_proof(
            key.public_key(),
            &for_("", Actor::Agent, session),
            &proof
        ));
        for wrong in [
            for_("bob", Actor::Agent, session),
            for_("", Actor::Human, session),
            for_("", Actor::Agent, ""),
            ProofFor {
                authority: &other,
                ..for_("", Actor::Agent, session)
            },
        ] {
            assert!(!verify_proof(key.public_key(), &wrong, &proof), "{wrong:?}");
        }
    }
}
