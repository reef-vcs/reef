//! Sigstore keyless session keys (R3.42, ADR 0066), behind the `sigstore`
//! feature, which the daemon enables and the client does not.
//!
//! A keyless session key is an ed25519 key that Sigstore's Fulcio certified
//! against an OIDC identity. The key signs a **binding statement** about
//! itself, an in-toto Statement v1 in a DSSE envelope, which Rekor logs:
//!
//! ```json
//! {"_type":"https://in-toto.io/Statement/v1",
//!  "subject":[{"name":"session-key","digest":{"sha256":"<the key's id>"}}],
//!  "predicateType":"urn:reef-vcs:sigstore-session-key:v1",
//!  "predicate":{"actor":"executor","agent":"reef-executor@ci-7",
//!               "session":"ses_41c9e07ab2d35f18","presence":"none","custody":"client"}}
//! ```
//!
//! The Sigstore bundle (v0.3: the Fulcio leaf, the transparency log entry
//! with its inclusion proof, and the envelope) is what a verifier checks:
//! [`check_sigstore_bundle`] verifies it offline against a [`TrustedRoot`],
//! through `sigstore-verify` with Reef's ring provider ([`provider`]), and
//! then reads the statement. The certificate names the OIDC issuer and
//! subject; which operator identity that is, is the verifier's own record
//! (the daemon's OIDC matches), so [`SigstoreCertified::bind`] takes the
//! identity and yields the R3.03 [`Binding`] that
//! [`crate::session::status_of`] judges, with Fulcio's window.
//!
//! Nothing here touches the network or a clock of its own; `sigstore-verify`
//! reads the current time only to refuse an integrated time in the future
//! and to pick the trusted root's keys valid now.

pub mod provider;

use std::collections::BTreeMap;

use der::{Decode as _, Encode as _};
use serde_json::{Map, Value};
use sigstore_types::{Bundle, SignatureContent};
use sigstore_verify::{CertificateError, Error as VerifyError, TransparencyLogError};
use sigstore_verify::{VerificationPolicy, Verifier};

use crate::keys::{KeyId, PublicKey};
use crate::session::{
    Actor, Binding, Custody, IdentityRef, Issuer, IssuerKind, Presence, agent_problem,
    is_session_id, known, names, string, subject_digest,
};
use crate::verify::statement_value;
use crate::{Invalid, PAYLOAD_TYPE, STATEMENT_TYPE};

/// The binding statement's predicate type.
pub const BINDING_PREDICATE_TYPE: &str = crate::session::SIGSTORE_BINDING_PREDICATE_TYPE;

/// The largest bundle accepted, as received: 64 KiB. A Rekor v1 bundle of a
/// binding statement is about 10 KiB.
pub const MAX_BUNDLE_BYTES: usize = 64 * 1024;

/// The public-good trusted root Reef ships (`crates/reef-evidence/sigstore/
/// trusted_root.json`), vendored from Sigstore's root-signing repository
/// (`scripts/sigstore-root.sh`).
pub const EMBEDDED_TRUSTED_ROOT: &str = include_str!("../../sigstore/trusted_root.json");

/// The bundle media types accepted.
/// v0.3 only: a v0.2 bundle may carry intermediate certificates of its
/// own, and Reef's signer writes v0.3.
const MEDIA_TYPES: [&str; 2] = [
    "application/vnd.dev.sigstore.bundle.v0.3+json",
    "application/vnd.dev.sigstore.bundle+json;version=0.3",
];

/// The step of [`check_sigstore_bundle`] a bundle failed at: the API's
/// `INVALID_SIGSTORE_BUNDLE` `step` metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Step {
    /// Over [`MAX_BUNDLE_BYTES`].
    Size,
    /// Not a Sigstore bundle as JSON, an unsupported media type, or not a
    /// DSSE envelope with a certificate.
    Json,
    /// The certificate does not chain to a Fulcio CA of the trusted root
    /// valid at the signing time, or is not a Fulcio certificate.
    Chain,
    /// The certificate's SCT does not verify against a CT log of the root.
    Sct,
    /// The transparency log entry: no entry, an inclusion proof, checkpoint
    /// or SET that does not verify, an unknown log, or an entry that is not
    /// the bundle's.
    Tlog,
    /// The signing time: an integrated time or timestamp outside the
    /// certificate's validity, or in the future.
    Time,
    /// The envelope's signature does not verify with the certificate's key.
    Signature,
    /// The payload is not the binding statement.
    Statement,
    /// The certificate's key is not a usable ed25519 key, or the statement's
    /// subject is not its key id.
    Key,
}

impl Step {
    /// The step's name: the `step` metadata value.
    pub fn as_str(self) -> &'static str {
        match self {
            Step::Size => "size",
            Step::Json => "json",
            Step::Chain => "chain",
            Step::Sct => "sct",
            Step::Tlog => "tlog",
            Step::Time => "time",
            Step::Signature => "signature",
            Step::Statement => "statement",
            Step::Key => "key",
        }
    }

    /// What the step wants, in one sentence, for an error's hint.
    pub fn hint(self) -> &'static str {
        match self {
            Step::Size => {
                "A keyless bundle is at most 64 KiB; a binding statement's is about 10 KiB."
            }
            Step::Json => {
                "Send a Sigstore bundle v0.3 as JSON whose content is the DSSE envelope of the binding statement and whose verification material is the Fulcio certificate."
            }
            Step::Chain => {
                "The certificate must come from a Fulcio CA in this daemon's trusted root ([sigstore] trusted_root, or the public-good root Reef ships)."
            }
            Step::Sct => {
                "The certificate's SCT must verify against a CT log in this daemon's trusted root."
            }
            Step::Tlog => {
                "Log the binding statement in a Rekor log of this daemon's trusted root and include the entry with its inclusion proof."
            }
            Step::Time => {
                "Log the binding statement while the Fulcio certificate is valid (10 minutes), and check the signing host's clock."
            }
            Step::Signature => {
                "Sign the binding statement's DSSE pre-authentication encoding with the certified key."
            }
            Step::Statement => {
                "The envelope's payload is the binding statement, urn:reef-vcs:sigstore-session-key:v1; docs/identity.md gives its fields."
            }
            Step::Key => {
                "Have Fulcio certify a fresh ed25519 key, and name that key's id (the SHA-256 of its SubjectPublicKeyInfo) as the statement's one subject."
            }
        }
    }
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a bundle is not a keyless session key's.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the Sigstore bundle fails at {step}: {problem}")]
pub struct BundleError {
    /// The step it failed at.
    pub step: Step,
    /// What was found.
    pub problem: String,
}

impl BundleError {
    fn new(step: Step, problem: impl Into<String>) -> BundleError {
        BundleError {
            step,
            problem: problem.into(),
        }
    }
}

impl reef_error::Classify for BundleError {
    fn kind(&self) -> reef_error::ErrorKind {
        reef_error::ErrorKind::InvalidArgument
    }
}

/// Why a trusted root cannot be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the Sigstore trusted root from {source_name} is not usable: {problem}")]
pub struct RootError {
    /// Where it came from.
    pub source_name: String,
    /// What is wrong.
    pub problem: String,
}

/// A Fulcio CA of a trusted root: what a keyless key's authority is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FulcioCa {
    /// The CA certificate's DER `SubjectPublicKeyInfo`.
    pub public_key: Vec<u8>,
    /// SHA-256 of it: the keyless key's authority key id.
    pub key_id: KeyId,
    /// The CA's algorithm, as Reef names it (`ecdsa-p384`, `ecdsa-p256`,
    /// `ed25519`, `rsa`, or the OID).
    pub algorithm: String,
    /// The CA's host, from the trusted root's `uri`, for display.
    pub host: String,
    subject: Vec<u8>,
}

/// The trusted root a verifier checks bundles against: the public-good root
/// Reef ships, or one parsed from a `trusted_root.json`.
pub struct TrustedRoot {
    verifier: Verifier,
    cas: Vec<FulcioCa>,
    digest: String,
    source: String,
}

impl std::fmt::Debug for TrustedRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustedRoot")
            .field("source", &self.source)
            .field("digest", &self.digest)
            .field("cas", &self.cas.len())
            .finish_non_exhaustive()
    }
}

impl TrustedRoot {
    /// [`EMBEDDED_TRUSTED_ROOT`].
    pub fn embedded() -> Result<TrustedRoot, RootError> {
        TrustedRoot::from_json(
            EMBEDDED_TRUSTED_ROOT.as_bytes(),
            "the embedded public-good root",
        )
    }

    /// A `trusted_root.json`'s bytes; `source` names it in errors and logs.
    pub fn from_json(json: &[u8], source: &str) -> Result<TrustedRoot, RootError> {
        provider::install();
        let bad = |problem: String| RootError {
            source_name: source.to_owned(),
            problem,
        };
        let text = std::str::from_utf8(json).map_err(|_| bad("not UTF-8".into()))?;
        let root = sigstore_trust_root::TrustedRoot::from_json(text)
            .map_err(|error| bad(error.to_string()))?;
        if root.certificate_authorities.is_empty() {
            return Err(bad("it has no Fulcio certificate authority".into()));
        }
        if root.tlogs.is_empty() {
            return Err(bad("it has no transparency log".into()));
        }
        let mut cas = Vec::new();
        for authority in &root.certificate_authorities {
            let host = host_of(&authority.uri);
            for certificate in &authority.cert_chain.certificates {
                let parsed = x509_cert::Certificate::from_der(certificate.raw_bytes.as_bytes())
                    .map_err(|error| bad(format!("a CA certificate does not parse: {error}")))?;
                let tbs = &parsed.tbs_certificate;
                let public_key = tbs
                    .subject_public_key_info
                    .to_der()
                    .map_err(|error| bad(error.to_string()))?;
                cas.push(FulcioCa {
                    key_id: KeyId::of(&public_key),
                    public_key,
                    algorithm: algorithm_name(&tbs.subject_public_key_info),
                    host: host.clone(),
                    subject: tbs
                        .subject
                        .to_der()
                        .map_err(|error| bad(error.to_string()))?,
                });
            }
        }
        let verifier = Verifier::new(&root).map_err(|error| bad(error.to_string()))?;
        Ok(TrustedRoot {
            verifier,
            cas,
            digest: crate::sha256_hex(json),
            source: source.to_owned(),
        })
    }

    /// SHA-256 of the root's JSON, lowercase hex.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Where it came from.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Its Fulcio CAs, every certificate of every chain.
    pub fn fulcio_cas(&self) -> &[FulcioCa] {
        &self.cas
    }
}

/// The host of a URI, for display: `https://fulcio.sigstore.dev` gives
/// `fulcio.sigstore.dev`.
fn host_of(uri: &str) -> String {
    let rest = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    host.chars()
        .filter(|c| !c.is_control())
        .take(crate::session::MAX_HOST_BYTES)
        .collect()
}

/// Reef's name for a `SubjectPublicKeyInfo`'s algorithm.
fn algorithm_name(spki: &x509_cert::spki::SubjectPublicKeyInfoOwned) -> String {
    const EC: &str = "1.2.840.10045.2.1";
    const P256: &str = "1.2.840.10045.3.1.7";
    const P384: &str = "1.3.132.0.34";
    const ED25519: &str = "1.3.101.112";
    const RSA: &str = "1.2.840.113549.1.1.1";
    let oid = spki.algorithm.oid.to_string();
    let curve = spki
        .algorithm
        .parameters
        .as_ref()
        .and_then(|any| any.decode_as::<der::asn1::ObjectIdentifier>().ok())
        .map(|oid| oid.to_string());
    match (oid.as_str(), curve.as_deref()) {
        (EC, Some(P256)) => "ecdsa-p256".into(),
        (EC, Some(P384)) => "ecdsa-p384".into(),
        (ED25519, _) => "ed25519".into(),
        (RSA, _) => "rsa".into(),
        _ => oid,
    }
}

/// A bundle that verifies against a trusted root, and the binding statement
/// it carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigstoreCertified {
    /// The certified key: the Fulcio leaf's, ed25519.
    pub public_key: PublicKey,
    /// The declared actor.
    pub actor: Actor,
    /// The declared agent label.
    pub agent: Option<String>,
    /// The session, named by the client.
    pub session: String,
    /// `none`.
    pub presence: Presence,
    /// `client`.
    pub custody: Custody,
    /// The certificate's OIDC issuer (Fulcio's `1.3.6.1.4.1.57264.1.8`,
    /// else `.1.1`).
    pub oidc_issuer: String,
    /// The certificate's subject: its SAN, an email or a URI.
    pub oidc_subject: String,
    /// Fulcio's CI claims (`.1.9` to `.1.24`), by name, for display.
    pub ci_claims: BTreeMap<String, String>,
    /// The start of the leaf's validity, unix milliseconds.
    pub not_before_ms: i64,
    /// Its end, unix milliseconds.
    pub not_after_ms: i64,
    /// When the log (or a timestamp authority) witnessed the statement,
    /// unix milliseconds: the earliest verified time.
    pub integrated_time_ms: i64,
    /// The log's id, lowercase hex.
    pub log_id: String,
    /// The entry's index in the log.
    pub log_index: u64,
    /// The Fulcio CA that issued the leaf.
    pub ca: FulcioCa,
    /// SHA-256 of the bundle's bytes, lowercase hex.
    pub id: String,
}

impl SigstoreCertified {
    /// The R3.03 binding for the identity the verifier matched to the
    /// certificate's issuer and subject: issuer kind `sigstore`, the CA's key
    /// id, Fulcio's window.
    pub fn bind(&self, identity: IdentityRef) -> Binding {
        Binding {
            public_key: self.public_key.clone(),
            identity,
            actor: self.actor,
            agent: self.agent.clone(),
            session: self.session.clone(),
            presence: self.presence,
            custody: self.custody,
            not_before_ms: self.not_before_ms,
            not_after_ms: self.not_after_ms,
            issuer: Issuer {
                kind: IssuerKind::Sigstore,
                key_id: self.ca.key_id.clone(),
                host: self.ca.host.clone(),
            },
        }
    }
}

/// The step a `sigstore-verify` error belongs to.
fn step_of(error: &VerifyError) -> Step {
    match error {
        VerifyError::Certificate(CertificateError::Sct(_)) => Step::Sct,
        // The chain is checked at the signing time: a leaf not valid then is
        // a signing time outside its window (webpki reports it inside the
        // chain error).
        VerifyError::Certificate(CertificateError::ChainInvalid(message))
            if message.contains("CertNotValidYet") || message.contains("CertExpired") =>
        {
            Step::Time
        }
        VerifyError::TransparencyLog(TransparencyLogError::IntegratedTime(_))
        | VerifyError::Timestamp(_)
        | VerifyError::Certificate(
            CertificateError::NotYetValid { .. } | CertificateError::Expired { .. },
        ) => Step::Time,
        VerifyError::Certificate(_)
        | VerifyError::TrustedCertificate(_)
        | VerifyError::IdentityMismatch { .. }
        | VerifyError::IssuerMismatch { .. } => Step::Chain,
        VerifyError::TransparencyLog(_) => Step::Tlog,
        VerifyError::SignatureInvalid(_) | VerifyError::Crypto(_) => Step::Signature,
        VerifyError::ArtifactMismatch(_) => Step::Key,
        _ => Step::Json,
    }
}

/// Checks a keyless session key's bundle in this order: the size; the
/// bundle's JSON and media type, a DSSE envelope and a certificate; then,
/// through `sigstore-verify`, the certificate chain to a Fulcio CA of `root`
/// valid at the signing time, its SCT, the transparency log entry
/// (inclusion proof, checkpoint, SET), the signing time within the leaf's
/// validity, the envelope's signature by the leaf's key, and the statement's
/// subject against the leaf's key id; and only then the leaf's key (ed25519)
/// and the binding statement's predicate.
#[expect(clippy::too_many_lines, reason = "the checks, in their order")]
pub fn check_sigstore_bundle(
    bundle: &[u8],
    root: &TrustedRoot,
) -> Result<SigstoreCertified, BundleError> {
    provider::install();
    if bundle.len() > MAX_BUNDLE_BYTES {
        return Err(BundleError::new(
            Step::Size,
            format!(
                "{} bytes; a bundle is at most {MAX_BUNDLE_BYTES}",
                bundle.len()
            ),
        ));
    }
    let json = |problem: String| BundleError::new(Step::Json, problem);
    let text = std::str::from_utf8(bundle).map_err(|_| json("not UTF-8".into()))?;
    let media_type = serde_json::from_str::<Value>(text)
        .map_err(|error| json(format!("not JSON: {error}")))?
        .get("mediaType")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_default();
    if !MEDIA_TYPES.contains(&media_type.as_str()) {
        return Err(json(format!(
            "media type {media_type:?} is not a Sigstore bundle v0.3"
        )));
    }
    let parsed = Bundle::from_json(text).map_err(|error| json(error.to_string()))?;
    let SignatureContent::DsseEnvelope(envelope) = &parsed.content else {
        return Err(json("its content is not a DSSE envelope".into()));
    };
    if envelope.payload_type != PAYLOAD_TYPE {
        return Err(json(format!(
            "the envelope's payload type is {:?}, not {PAYLOAD_TYPE:?}",
            envelope.payload_type
        )));
    }
    if parsed.verification_material.tlog_entries.is_empty() {
        return Err(BundleError::new(
            Step::Tlog,
            "it has no transparency log entry",
        ));
    }
    let leaf_der = parsed
        .signing_certificate()
        .ok_or_else(|| json("it has no signing certificate".into()))?;
    let leaf = x509_cert::Certificate::from_der(leaf_der.as_bytes())
        .map_err(|error| BundleError::new(Step::Chain, format!("the certificate: {error}")))?;
    let spki = leaf
        .tbs_certificate
        .subject_public_key_info
        .to_der()
        .map_err(|error| BundleError::new(Step::Chain, error.to_string()))?;
    // The statement's subject is the key id, the SHA-256 of the SPKI, so the
    // SPKI is the "artifact" the attestation binds.
    let result = root
        .verifier
        .verify(
            spki.as_slice(),
            &parsed,
            &VerificationPolicy::any_identity(),
        )
        .map_err(|error| BundleError::new(step_of(&error), error.to_string()))?;
    if !result.tlog_verified() {
        return Err(BundleError::new(
            Step::Tlog,
            "no transparency log entry verified",
        ));
    }
    let info = result
        .certificate()
        .ok_or_else(|| BundleError::new(Step::Chain, "no certificate was verified"))?;
    let public_key = PublicKey::from_spki(&spki).map_err(|error| {
        BundleError::new(
            Step::Key,
            format!("the certificate's key is not a usable ed25519 key: {error}"),
        )
    })?;
    let oidc_issuer = result
        .issuer()
        .filter(|issuer| !issuer.is_empty())
        .ok_or_else(|| BundleError::new(Step::Chain, "the certificate names no OIDC issuer"))?
        .to_owned();
    let oidc_subject = result
        .identity()
        .map(|san| san.as_str().to_owned())
        .filter(|subject| !subject.is_empty())
        .ok_or_else(|| BundleError::new(Step::Chain, "the certificate has no email or URI SAN"))?;
    let times: Vec<i64> = result
        .integrated_time()
        .into_iter()
        .chain(result.verified_timestamps().iter().copied())
        .map(jiff::Timestamp::as_millisecond)
        .collect();
    let integrated_time_ms = times
        .into_iter()
        .min()
        .ok_or_else(|| BundleError::new(Step::Time, "no signing time was verified"))?;
    let entry = &parsed.verification_material.tlog_entries[0];
    let ca = issuing_ca(&leaf, root)?;
    let statement = binding_statement(envelope.payload.as_bytes(), &public_key)?;
    Ok(SigstoreCertified {
        public_key,
        actor: statement.actor,
        agent: statement.agent,
        session: statement.session,
        presence: statement.presence,
        custody: statement.custody,
        oidc_issuer,
        oidc_subject,
        ci_claims: ci_claims(&info.ci_claims),
        not_before_ms: info.not_before.as_millisecond(),
        not_after_ms: info.not_after.as_millisecond(),
        integrated_time_ms,
        log_id: crate::hex(entry.log_id.key_id.as_bytes()),
        log_index: entry.log_index.get(),
        ca,
        id: crate::sha256_hex(bundle),
    })
}

/// The CA of `root` that issued `leaf`: the one whose subject is the leaf's
/// issuer and whose key verifies the leaf's signature (`sigstore-verify`
/// already built the path; this names its first step).
fn issuing_ca(leaf: &x509_cert::Certificate, root: &TrustedRoot) -> Result<FulcioCa, BundleError> {
    let issuer = leaf
        .tbs_certificate
        .issuer
        .to_der()
        .map_err(|error| BundleError::new(Step::Chain, error.to_string()))?;
    let tbs = leaf
        .tbs_certificate
        .to_der()
        .map_err(|error| BundleError::new(Step::Chain, error.to_string()))?;
    let signature = leaf.signature.raw_bytes();
    let scheme = match leaf.signature_algorithm.oid.to_string().as_str() {
        "1.2.840.10045.4.3.2" => Some(sigstore_crypto::SigningScheme::EcdsaP256Sha256),
        "1.2.840.10045.4.3.3" => Some(sigstore_crypto::SigningScheme::EcdsaP384Sha384),
        "1.3.101.112" => Some(sigstore_crypto::SigningScheme::Ed25519),
        _ => None,
    };
    let named: Vec<&FulcioCa> = root.cas.iter().filter(|ca| ca.subject == issuer).collect();
    // `sigstore-verify` built and checked the path already: one CA with the
    // leaf's issuer name is it; several (a root reissued under one name)
    // are told apart by the leaf's signature.
    if let [only] = named.as_slice() {
        return Ok((*only).clone());
    }
    named
        .into_iter()
        .find(|ca| {
            // An algorithm Reef cannot check cannot tell two CAs apart.
            let Some(scheme) = scheme else {
                return false;
            };
            spki_key_bytes(&ca.public_key).is_some_and(|key| {
                sigstore_crypto::CryptoProvider::verify_raw(
                    &provider::RingProvider,
                    scheme_for(scheme, &ca.algorithm),
                    &key,
                    &tbs,
                    signature,
                )
                .is_ok()
            })
        })
        .cloned()
        .ok_or_else(|| {
            BundleError::new(
                Step::Chain,
                "no Fulcio CA of the trusted root issued the certificate",
            )
        })
}

/// `scheme` adjusted to the CA's curve: an ECDSA signature's hash comes
/// from the leaf's algorithm, its curve from the CA's key.
fn scheme_for(
    scheme: sigstore_crypto::SigningScheme,
    ca_algorithm: &str,
) -> sigstore_crypto::SigningScheme {
    use sigstore_crypto::SigningScheme as S;
    match (scheme, ca_algorithm) {
        (S::EcdsaP256Sha256, "ecdsa-p384") => S::EcdsaP384Sha256,
        (S::EcdsaP384Sha384, "ecdsa-p256") => S::EcdsaP256Sha384,
        (scheme, _) => scheme,
    }
}

/// The `subjectPublicKey` bits of a DER `SubjectPublicKeyInfo`.
fn spki_key_bytes(spki: &[u8]) -> Option<Vec<u8>> {
    let parsed = x509_cert::spki::SubjectPublicKeyInfoOwned::from_der(spki).ok()?;
    parsed.subject_public_key.as_bytes().map(<[u8]>::to_vec)
}

/// Fulcio's CI claims, by name, without the empty ones.
fn ci_claims(claims: &sigstore_crypto::FulcioCiClaims) -> BTreeMap<String, String> {
    [
        ("build_signer_uri", &claims.build_signer_uri),
        ("build_signer_digest", &claims.build_signer_digest),
        ("runner_environment", &claims.runner_environment),
        ("source_repository_uri", &claims.source_repository_uri),
        ("source_repository_digest", &claims.source_repository_digest),
        ("source_repository_ref", &claims.source_repository_ref),
        (
            "source_repository_identifier",
            &claims.source_repository_identifier,
        ),
        (
            "source_repository_owner_uri",
            &claims.source_repository_owner_uri,
        ),
        (
            "source_repository_owner_identifier",
            &claims.source_repository_owner_identifier,
        ),
        ("build_config_uri", &claims.build_config_uri),
        ("build_config_digest", &claims.build_config_digest),
        ("build_trigger", &claims.build_trigger),
        ("run_invocation_uri", &claims.run_invocation_uri),
        (
            "source_repository_visibility_at_signing",
            &claims.source_repository_visibility_at_signing,
        ),
        ("deployment_environment", &claims.deployment_environment),
        ("token_subject", &claims.token_subject),
    ]
    .into_iter()
    .filter_map(|(name, value)| {
        value
            .as_ref()
            .filter(|value| !value.is_empty())
            .map(|value| (name.to_owned(), value.clone()))
    })
    .collect()
}

/// The binding statement's fields.
struct Statement {
    actor: Actor,
    agent: Option<String>,
    session: String,
    presence: Presence,
    custody: Custody,
}

fn statement_error(invalid: &Invalid) -> BundleError {
    let step = match invalid {
        Invalid::Subject(_) => Step::Key,
        _ => Step::Statement,
    };
    BundleError::new(step, invalid.to_string())
}

/// The binding statement in `payload`, which `key` signed: its subject is
/// the key's id, and its predicate's fields are known.
fn binding_statement(payload: &[u8], key: &PublicKey) -> Result<Statement, BundleError> {
    let value = statement_value(payload).map_err(|invalid| statement_error(&invalid))?;
    let Value::Object(root) = &value else {
        return Err(BundleError::new(Step::Statement, "it is not a JSON object"));
    };
    match root.get("_type") {
        Some(Value::String(t)) if t == STATEMENT_TYPE => {}
        _ => {
            return Err(BundleError::new(
                Step::Statement,
                format!("_type is not {STATEMENT_TYPE:?}"),
            ));
        }
    }
    match root.get("predicateType") {
        Some(Value::String(t)) if t == BINDING_PREDICATE_TYPE => {}
        other => {
            return Err(BundleError::new(
                Step::Statement,
                format!(
                    "predicateType is {}, not {BINDING_PREDICATE_TYPE:?}",
                    other.map_or_else(|| "missing".to_owned(), Value::to_string)
                ),
            ));
        }
    }
    let digest =
        subject_digest(root.get("subject")).map_err(|invalid| statement_error(&invalid))?;
    if digest != key.id().as_str() {
        return Err(BundleError::new(
            Step::Key,
            format!(
                "the subject's sha256 {digest} is not the certified key's id {}",
                key.id()
            ),
        ));
    }
    let Some(Value::Object(predicate)) = root.get("predicate") else {
        return Err(BundleError::new(
            Step::Statement,
            "the predicate is missing or not a JSON object",
        ));
    };
    predicate_of(predicate).map_err(|invalid| statement_error(&invalid))
}

fn predicate_of(predicate: &Map<String, Value>) -> Result<Statement, Invalid> {
    let actor = known(
        predicate,
        "actor",
        Actor::parse,
        &names(Actor::ALL, Actor::as_str),
    )?;
    let agent = match predicate.get("agent") {
        None => None,
        Some(Value::String(agent)) => {
            if let Some(problem) = agent_problem(agent) {
                return Err(crate::verify::predicate_error("agent", problem));
            }
            Some(agent.clone())
        }
        Some(_) => return Err(crate::verify::predicate_error("agent", "not a string")),
    };
    let session = string(predicate, "session", "session")?;
    if !is_session_id(session) {
        return Err(crate::verify::predicate_error(
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
    // A keyless key's presence is `none`: nothing verified a person at its
    // use, so a signer cannot claim more about itself (R3.43's
    // `user_verified` needs its own evidence).
    if presence != Presence::None {
        return Err(crate::verify::predicate_error(
            "presence",
            "a keyless key's presence is none",
        ));
    }
    if custody != Custody::Client {
        return Err(crate::verify::predicate_error(
            "custody",
            "a keyless key is held by the client that Fulcio certified it for",
        ));
    }
    Ok(Statement {
        actor,
        agent,
        session: session.to_owned(),
        presence,
        custody,
    })
}

/// The binding statement's payload for a key, as a signer writes it: the
/// statement's JSON, compact, keys in the order of the module's example.
pub fn binding_payload(
    key_id: &KeyId,
    actor: Actor,
    agent: Option<&str>,
    session: &str,
) -> Vec<u8> {
    let mut predicate = Map::new();
    predicate.insert("actor".into(), Value::String(actor.as_str().into()));
    if let Some(agent) = agent {
        predicate.insert("agent".into(), Value::String(agent.into()));
    }
    predicate.insert("session".into(), Value::String(session.into()));
    predicate.insert(
        "presence".into(),
        Value::String(Presence::None.as_str().into()),
    );
    predicate.insert(
        "custody".into(),
        Value::String(Custody::Client.as_str().into()),
    );
    let statement = serde_json::json!({
        "_type": STATEMENT_TYPE,
        "subject": [{"name": crate::session::SUBJECT_NAME, "digest": {"sha256": key_id.as_str()}}],
        "predicateType": BINDING_PREDICATE_TYPE,
        "predicate": predicate,
    });
    serde_json::to_vec(&statement).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reef never relaxes `sigstore-verify`'s policy: no check is skipped.
    #[test]
    fn no_check_is_skipped() {
        let source = include_str!("mod.rs");
        for method in ["skip_sct", "skip_tlog_unsafe", "skip_certificate_chain"] {
            let call = format!(".{method}(");
            assert!(!source.contains(&call), "{method} is called");
        }
    }

    #[test]
    fn hosts() {
        assert_eq!(
            host_of("https://fulcio.sigstore.dev"),
            "fulcio.sigstore.dev"
        );
        assert_eq!(host_of("https://a.example/x?y"), "a.example");
        assert_eq!(host_of("fulcio"), "fulcio");
    }

    #[test]
    fn the_embedded_root_loads() {
        let root = TrustedRoot::embedded().unwrap();
        assert_ne!(root.fulcio_cas(), []);
        assert_eq!(
            root.digest(),
            crate::sha256_hex(EMBEDDED_TRUSTED_ROOT.as_bytes())
        );
        assert!(
            root.fulcio_cas()
                .iter()
                .all(|ca| ca.host == "fulcio.sigstore.dev")
        );
    }

    #[test]
    fn the_binding_payload_parses_back() {
        let key = crate::SigningKey::from_seed(&[9; 32]).unwrap();
        let payload = binding_payload(
            key.public_key().id(),
            Actor::Executor,
            Some("ci-7"),
            "ses_0123456789abcdef",
        );
        let statement = binding_statement(&payload, key.public_key()).unwrap();
        assert_eq!(statement.actor, Actor::Executor);
        assert_eq!(statement.agent.as_deref(), Some("ci-7"));
        let other = crate::SigningKey::from_seed(&[8; 32]).unwrap();
        assert_eq!(
            binding_statement(&payload, other.public_key())
                .err()
                .map(|e| e.step),
            Some(Step::Key)
        );
    }
}
