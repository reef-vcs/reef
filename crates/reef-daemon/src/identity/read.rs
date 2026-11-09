//! Reading certified keys (R3.03): every read checks each certificate
//! again with [`reef_evidence::session::check_certificate`] against the
//! authorities' recorded public keys, and takes every field it reports from
//! the verified certificate, never from the columns, as evidence reads do.
//! A certificate is valid for its row only when it verifies, is the row's
//! bytes in canonical form, and certifies the row's key; a row changed
//! behind the daemon reads `INVALID`, and reports nothing of its columns
//! but the daemon's own records of the row.
//!
//! The revocations that apply are looked up by the certificate's session
//! and identity, so changing a row's `identity_id` changes neither the
//! identity a read reports nor the revocations that apply to it.
//!
//! A keyless key (R3.42) is checked by its Sigstore bundle instead, against
//! the daemon's trusted root, and is valid for its row only when the bundle
//! verifies, certifies the row's key, came from the row's Fulcio CA, and
//! names the issuer and subject of the OIDC match the row records; the
//! identity is that match's, the match's revocation applies beside the
//! others, and the window is Fulcio's. Unlike a daemon certificate, nothing
//! signed names the identity: the match row's `identity_id`, and the row's
//! `oidc_id` among matches of the same issuer and subject, are the daemon's
//! own records, which the same uid can edit as it can revocations (ADR
//! 0027; threat model S-139).

use std::collections::{BTreeSet, HashMap};

use reef_api::v1;
use reef_evidence::session::{
    Actor, Authority, Binding, Certified, Custody, IdentityKind, IdentityRef, IdentityStatus,
    IssuerKind, Presence, Revocation, Revocations, check_certificate, earliest_revocation,
    status_of,
};
use reef_evidence::sigstore::{SigstoreCertified, TrustedRoot, check_sigstore_bundle};
use reef_evidence::{KeyId, PublicKey};

use crate::db::{
    AuthorityRow, Db, DbError, IdentityRow, OidcMatchRow, RevocationRow, SessionKeyRow,
};
use crate::model::Timestamp;
use crate::service::convert::timestamp;

/// A keyless key's verified bundle, as a read sees it (R3.42).
#[derive(Clone, Debug)]
pub struct KeylessRead {
    /// The bundle, verified at this read.
    pub certified: SigstoreCertified,
    /// The OIDC match the key was bound through.
    pub oidc: OidcMatchRow,
}

/// A session key as a read sees it.
#[derive(Clone, Debug)]
pub struct ReadKey {
    /// The row.
    pub row: SessionKeyRow,
    /// Its certificate, when it is a daemon's and valid for the row.
    pub certified: Option<Certified>,
    /// A keyless key's bundle, when it is valid for the row (R3.42).
    pub keyless: Option<KeylessRead>,
    /// What its valid certificate binds, whichever issued it.
    pub binding: Option<Binding>,
    /// Its authority's revocation, when its certificate is valid.
    pub authority_revocation: Option<Revocation>,
    /// The key's, its session's and its identity's revocations.
    pub revocations: Revocations,
    /// The earliest applicable revocation, its authority's included.
    pub revocation: Option<Revocation>,
    /// The identity the certificate names (the column's when it is
    /// invalid), if there is such a record.
    pub identity: Option<IdentityRow>,
}

fn revocation(row: &RevocationRow) -> Revocation {
    Revocation {
        revoked_at_ms: row.revoked_at.0,
        invalid_from_ms: row.invalid_from.0,
        reason: row.reason.clone(),
    }
}

/// `row` as an authority a certificate can be checked against: an ed25519
/// key whose id is the row's id.
fn authority_of(row: &AuthorityRow) -> Option<Authority> {
    if row.algorithm != "ed25519" {
        return None;
    }
    let key = PublicKey::from_spki(&row.public_key).ok()?;
    (key.id().as_str() == row.id).then(|| Authority {
        key,
        kind: IssuerKind::ReefDaemon,
        revocation: row.revocation.as_ref().map(revocation),
    })
}

/// How one row's certificate checked.
enum Checked {
    /// A daemon's certificate, when valid for the row.
    Reef(Option<Certified>),
    /// A keyless key's bundle and match, when valid for the row.
    Keyless(Option<KeylessRead>),
}

impl Checked {
    /// The session and identity ids the certificate names, when valid.
    fn names(&self) -> Option<(String, String)> {
        match self {
            Checked::Reef(Some(certified)) => Some((
                certified.binding.session.clone(),
                certified.binding.identity.id.clone(),
            )),
            Checked::Keyless(Some(keyless)) => Some((
                keyless.certified.session.clone(),
                keyless.oidc.identity_id.clone(),
            )),
            _ => None,
        }
    }
}

/// `row`'s bundle checked: valid only when it verifies against `root`,
/// certifies the row's key, came from the row's authority, and names the
/// issuer and subject of the match the row records.
fn check_keyless(
    row: &SessionKeyRow,
    root: Option<&TrustedRoot>,
    authorities: &HashMap<String, AuthorityRow>,
    matches: &HashMap<String, OidcMatchRow>,
) -> Option<KeylessRead> {
    let root = root?;
    let certified = check_sigstore_bundle(&row.certificate, root).ok()?;
    let oidc = matches.get(row.oidc_id.as_deref()?)?;
    let authority = authorities.get(&row.authority_id)?;
    let valid = certified.public_key.id().as_str() == row.key_id
        && certified.ca.key_id.as_str() == row.authority_id
        && authority.kind == IssuerKind::Sigstore.as_str()
        && oidc.issuer == certified.oidc_issuer
        && oidc.subject == certified.oidc_subject;
    valid.then(|| KeylessRead {
        certified,
        oidc: oidc.clone(),
    })
}

/// `rows` read: each certificate checked against the authorities (a keyless
/// key's bundle against `keyless`, the daemon's trusted root; none reads it
/// invalid), then the sessions and identities their certificates name
/// looked up in one query each. Blocking: the checks are CPU work.
#[expect(
    clippy::too_many_lines,
    reason = "both issuers' checks, then one lookup"
)]
pub fn read_keys(
    db: &Db,
    rows: Vec<SessionKeyRow>,
    keyless: Option<&TrustedRoot>,
) -> Result<Vec<ReadKey>, DbError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let authorities: HashMap<String, AuthorityRow> = db
        .authorities()?
        .into_iter()
        .map(|row| (row.id.clone(), row))
        .collect();
    let oidc_ids: BTreeSet<&str> = rows
        .iter()
        .filter_map(|row| row.oidc_id.as_deref())
        .collect();
    let matches = db.oidc_matches_by_id(&oidc_ids.into_iter().collect::<Vec<_>>())?;
    let lookup = |id: &KeyId| {
        authorities
            .get(id.as_str())
            .filter(|row| row.kind == IssuerKind::ReefDaemon.as_str())
            .and_then(authority_of)
    };
    let checked: Vec<(SessionKeyRow, Checked)> = rows
        .into_iter()
        .map(|row| {
            let checked = if row.issuer_kind == IssuerKind::Sigstore.as_str() {
                Checked::Keyless(check_keyless(&row, keyless, &authorities, &matches))
            } else {
                Checked::Reef(check_certificate(&row.certificate, &lookup).ok().filter(
                    |certified| {
                        certified.canonical == row.certificate
                            && certified.binding.public_key.id().as_str() == row.key_id
                    },
                ))
            };
            if checked.names().is_none() {
                tracing::warn!(
                    key_id = %row.key_id,
                    issuer_kind = %row.issuer_kind,
                    "a stored session key's certificate does not verify; it reads as invalid"
                );
            }
            (row, checked)
        })
        .collect();
    let names = |(row, checked): &(SessionKeyRow, Checked)| {
        checked
            .names()
            .unwrap_or_else(|| (row.session_id.clone(), row.identity_id.clone()))
    };
    let sessions: BTreeSet<String> = checked.iter().map(|entry| names(entry).0).collect();
    let identities: BTreeSet<String> = checked.iter().map(|entry| names(entry).1).collect();
    let records = db.key_records(
        &sessions.iter().map(String::as_str).collect::<Vec<_>>(),
        &identities.iter().map(String::as_str).collect::<Vec<_>>(),
    )?;
    Ok(checked
        .into_iter()
        .map(|entry| {
            let (session_id, identity_id) = names(&entry);
            let session = records.sessions.get(&session_id);
            let identity = records.identities.get(&identity_id).cloned();
            let (row, checked) = entry;
            let (certified, keyless) = match checked {
                Checked::Reef(certified) => (certified, None),
                Checked::Keyless(keyless) => (None, keyless),
            };
            let revocations = Revocations {
                key: row.revocation.as_ref().map(revocation),
                session: session
                    .and_then(|session| session.revocation.as_ref())
                    .map(revocation),
                identity: identity
                    .as_ref()
                    .and_then(|identity| identity.revocation.as_ref())
                    .map(revocation),
                binding: keyless
                    .as_ref()
                    .and_then(|keyless| keyless.oidc.revocation.as_ref())
                    .map(revocation),
            };
            // A keyless key's identity is the match's: it needs the
            // identity's record, of a kind this reader knows, to name it, and
            // a `ci` identity is never bound to a key declared `human`.
            let keyless = keyless.filter(|keyless| {
                identity.as_ref().is_some_and(|identity| {
                    match IdentityKind::parse(&identity.kind) {
                        Some(IdentityKind::Ci) => keyless.certified.actor != Actor::Human,
                        Some(IdentityKind::Person) => true,
                        None => false,
                    }
                })
            });
            let (binding, authority_revocation) = match (&certified, &keyless, &identity) {
                (Some(certified), _, _) => (
                    Some(certified.binding.clone()),
                    certified.authority.revocation.clone(),
                ),
                (None, Some(keyless), Some(identity)) => (
                    Some(keyless.certified.bind(IdentityRef {
                        id: identity.id.clone(),
                        name: identity.name.clone(),
                        kind: IdentityKind::parse(&identity.kind).unwrap_or(IdentityKind::Ci),
                    })),
                    authorities
                        .get(&row.authority_id)
                        .and_then(|row| row.revocation.as_ref())
                        .map(revocation),
                ),
                _ => (None, None),
            };
            // An invalid certificate's columns are not believed: only the
            // key's own revocation, the daemon's record of that row, is.
            let reported = if binding.is_some() {
                earliest_revocation(&revocations, authority_revocation.as_ref()).cloned()
            } else {
                revocations.key.clone()
            };
            ReadKey {
                row,
                certified,
                keyless,
                binding,
                authority_revocation,
                revocations,
                revocation: reported,
                identity,
            }
        })
        .collect())
}

impl ReadKey {
    /// The identity's name a read reports: the certificate's; `None` when
    /// it is invalid, whose columns are not believed.
    pub fn identity_name(&self) -> Option<&str> {
        self.binding
            .as_ref()
            .map(|binding| binding.identity.name.as_str())
    }

    /// The status of evidence this key signed, first witnessed at
    /// `witnessed_at`.
    pub fn status_at(&self, witnessed_at: Timestamp) -> IdentityStatus {
        match &self.binding {
            Some(binding) => status_of(
                binding,
                self.authority_revocation.as_ref(),
                witnessed_at.0,
                &self.revocations,
            ),
            None => IdentityStatus::Invalid,
        }
    }
}

/// `actor` in the API.
pub fn actor_message(actor: Option<Actor>) -> v1::SessionActor {
    match actor {
        Some(Actor::Human) => v1::SessionActor::Human,
        Some(Actor::Agent) => v1::SessionActor::Agent,
        Some(Actor::Executor) => v1::SessionActor::Executor,
        None => v1::SessionActor::Unspecified,
    }
}

/// `kind` in the API.
pub fn kind_message(kind: Option<IdentityKind>) -> v1::IdentityKind {
    match kind {
        Some(IdentityKind::Person) => v1::IdentityKind::Person,
        Some(IdentityKind::Ci) => v1::IdentityKind::Ci,
        None => v1::IdentityKind::Unspecified,
    }
}

fn presence_message(presence: Option<Presence>) -> v1::SessionPresence {
    match presence {
        Some(Presence::None) => v1::SessionPresence::None,
        None => v1::SessionPresence::Unspecified,
    }
}

fn custody_message(custody: Option<Custody>) -> v1::KeyCustody {
    match custody {
        Some(Custody::Client) => v1::KeyCustody::Client,
        Some(Custody::Daemon) => v1::KeyCustody::Daemon,
        None => v1::KeyCustody::Unspecified,
    }
}

fn status_message(status: IdentityStatus) -> v1::IdentityStatus {
    match status {
        IdentityStatus::Unbound => v1::IdentityStatus::Unbound,
        IdentityStatus::Bound => v1::IdentityStatus::Bound,
        IdentityStatus::Expired => v1::IdentityStatus::Expired,
        IdentityStatus::Revoked => v1::IdentityStatus::Revoked,
        IdentityStatus::Invalid => v1::IdentityStatus::Invalid,
    }
}

/// A revocation in the API.
pub fn revocation_message(revocation: &Revocation) -> v1::Revocation {
    v1::Revocation {
        revoked_at: Some(timestamp(Timestamp(revocation.revoked_at_ms))),
        invalid_from: Some(timestamp(Timestamp(revocation.invalid_from_ms))),
        reason: revocation.reason.clone().unwrap_or_default(),
    }
}

/// A recorded revocation in the API.
pub fn revocation_row_message(revocation: &RevocationRow) -> v1::Revocation {
    revocation_message(&self::revocation(revocation))
}

/// `read` as the API's `SessionKey`; with `with_bytes`, its public key and
/// certificate.
pub fn session_key_message(read: &ReadKey, with_bytes: bool) -> v1::SessionKey {
    let row = &read.row;
    let mut message = v1::SessionKey {
        key_id: row.key_id.clone(),
        issued_at: Some(timestamp(row.issued_at)),
        issued_by_agent: row.issued_by_agent.clone().unwrap_or_default(),
        remote_token_id: row.remote_token_id.clone().unwrap_or_default(),
        revocation: read.revocation.as_ref().map(revocation_message),
        certificate_valid: read.binding.is_some(),
        certificate: if with_bytes {
            row.certificate.clone()
        } else {
            Vec::new()
        },
        ..v1::SessionKey::default()
    };
    if let Some(binding) = &read.binding {
        message.identity_id.clone_from(&binding.identity.id);
        message.identity.clone_from(&binding.identity.name);
        message.set_identity_kind(kind_message(Some(binding.identity.kind)));
        message.set_actor(actor_message(Some(binding.actor)));
        message.agent = binding.agent.clone().unwrap_or_default();
        message.session_id.clone_from(&binding.session);
        message.set_presence(presence_message(Some(binding.presence)));
        message.set_custody(custody_message(Some(binding.custody)));
        message.set_issuer_kind(issuer_kind_message(binding.issuer.kind));
        message.authority_key_id = binding.issuer.key_id.to_string();
        message.not_before = Some(timestamp(Timestamp(binding.not_before_ms)));
        message.not_after = Some(timestamp(Timestamp(binding.not_after_ms)));
        if with_bytes {
            message.public_key = binding.public_key.spki().to_vec();
        }
        message.sigstore = read.keyless.as_ref().map(sigstore_message);
    }
    // An invalid certificate: only `key_id`, `certificate_valid: false` and
    // the daemon's own records (set above: `issued_at`, `issued_by_agent`,
    // `remote_token_id` and the key's own revocation) are reported; the
    // identity, kind, actor, presence, session and window stay unset, since
    // nothing vouches for the columns.
    message
}

/// `kind` in the API.
pub fn issuer_kind_message(kind: IssuerKind) -> v1::IssuerKind {
    match kind {
        IssuerKind::ReefDaemon => v1::IssuerKind::ReefDaemon,
        IssuerKind::Sigstore => v1::IssuerKind::Sigstore,
    }
}

/// A keyless key's Sigstore certificate in the API.
fn sigstore_message(keyless: &KeylessRead) -> v1::SigstoreCertificate {
    let certified = &keyless.certified;
    v1::SigstoreCertificate {
        oidc_issuer: certified.oidc_issuer.clone(),
        oidc_subject: certified.oidc_subject.clone(),
        oidc_match_id: keyless.oidc.id.clone(),
        integrated_time: Some(timestamp(Timestamp(certified.integrated_time_ms))),
        log_id: certified.log_id.clone(),
        log_index: certified.log_index,
        ci_claims: certified
            .ci_claims
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        bundle_id: certified.id.clone(),
    }
}

/// What a verifier sees of the key that signed evidence first witnessed at
/// `witnessed_at`: `UNBOUND` when `read` is `None`.
pub fn key_identity(read: Option<&ReadKey>, witnessed_at: Timestamp) -> v1::KeyIdentity {
    let status = read.map_or(IdentityStatus::Unbound, |read| read.status_at(witnessed_at));
    v1::KeyIdentity {
        status: status_message(status).into(),
        key: read.map(|read| session_key_message(read, false)),
        witnessed_at: Some(timestamp(witnessed_at)),
    }
}

/// What a verifier sees of evidence whose signature did not verify at this
/// read: `INVALID`, with no key, since its key id is not believed.
pub fn unverified_identity(witnessed_at: Timestamp) -> v1::KeyIdentity {
    v1::KeyIdentity {
        status: status_message(IdentityStatus::Invalid).into(),
        key: None,
        witnessed_at: Some(timestamp(witnessed_at)),
    }
}
