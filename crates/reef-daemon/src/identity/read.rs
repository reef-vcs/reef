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

use std::collections::{BTreeSet, HashMap};

use reef_api::v1;
use reef_evidence::session::{
    Actor, Authority, Certified, Custody, IdentityKind, IdentityStatus, IssuerKind, Presence,
    Revocation, Revocations, check_certificate, status_at,
};
use reef_evidence::{KeyId, PublicKey};

use crate::db::{AuthorityRow, Db, DbError, IdentityRow, RevocationRow, SessionKeyRow};
use crate::model::Timestamp;
use crate::service::convert::timestamp;

/// A session key as a read sees it.
#[derive(Clone, Debug)]
pub struct ReadKey {
    /// The row.
    pub row: SessionKeyRow,
    /// Its certificate, when it is valid for the row.
    pub certified: Option<Certified>,
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

/// `rows` read: each certificate checked against the authorities, then the
/// sessions and identities their certificates name looked up in one query
/// each. Blocking: the checks are CPU work.
pub fn read_keys(db: &Db, rows: Vec<SessionKeyRow>) -> Result<Vec<ReadKey>, DbError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let authorities: HashMap<String, AuthorityRow> = db
        .authorities()?
        .into_iter()
        .map(|row| (row.id.clone(), row))
        .collect();
    let lookup = |id: &KeyId| authorities.get(id.as_str()).and_then(authority_of);
    let checked: Vec<(SessionKeyRow, Option<Certified>)> = rows
        .into_iter()
        .map(|row| {
            let certified = check_certificate(&row.certificate, &lookup)
                .ok()
                .filter(|certified| {
                    certified.canonical == row.certificate
                        && certified.binding.public_key.id().as_str() == row.key_id
                });
            if certified.is_none() {
                tracing::warn!(
                    key_id = %row.key_id,
                    "a stored session key's certificate does not verify; it reads as invalid"
                );
            }
            (row, certified)
        })
        .collect();
    let session_of = |(row, certified): &(SessionKeyRow, Option<Certified>)| {
        certified
            .as_ref()
            .map_or_else(|| row.session_id.clone(), |c| c.binding.session.clone())
    };
    let identity_of = |(row, certified): &(SessionKeyRow, Option<Certified>)| {
        certified.as_ref().map_or_else(
            || row.identity_id.clone(),
            |c| c.binding.identity.id.clone(),
        )
    };
    let sessions: BTreeSet<String> = checked.iter().map(session_of).collect();
    let identities: BTreeSet<String> = checked.iter().map(identity_of).collect();
    let records = db.key_records(
        &sessions.iter().map(String::as_str).collect::<Vec<_>>(),
        &identities.iter().map(String::as_str).collect::<Vec<_>>(),
    )?;
    Ok(checked
        .into_iter()
        .map(|entry| {
            let session = records.sessions.get(&session_of(&entry));
            let identity = records.identities.get(&identity_of(&entry)).cloned();
            let (row, certified) = entry;
            let revocations = Revocations {
                key: row.revocation.as_ref().map(revocation),
                session: session
                    .and_then(|session| session.revocation.as_ref())
                    .map(revocation),
                identity: identity
                    .as_ref()
                    .and_then(|identity| identity.revocation.as_ref())
                    .map(revocation),
            };
            // An invalid certificate's columns are not believed: only the
            // key's own revocation, the daemon's record of that row, is.
            let reported = certified.as_ref().map_or_else(
                || revocations.key.clone(),
                |certified| certified.revocation(&revocations).cloned(),
            );
            ReadKey {
                row,
                certified,
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
        self.certified
            .as_ref()
            .map(|certified| certified.binding.identity.name.as_str())
    }

    /// The status of evidence this key signed, first witnessed at
    /// `witnessed_at`.
    pub fn status_at(&self, witnessed_at: Timestamp) -> IdentityStatus {
        match &self.certified {
            Some(certified) => status_at(certified, witnessed_at.0, &self.revocations),
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
        certificate_valid: read.certified.is_some(),
        certificate: if with_bytes {
            row.certificate.clone()
        } else {
            Vec::new()
        },
        ..v1::SessionKey::default()
    };
    if let Some(certified) = &read.certified {
        let binding = &certified.binding;
        message.identity_id.clone_from(&binding.identity.id);
        message.identity.clone_from(&binding.identity.name);
        message.set_identity_kind(kind_message(Some(binding.identity.kind)));
        message.set_actor(actor_message(Some(binding.actor)));
        message.agent = binding.agent.clone().unwrap_or_default();
        message.session_id.clone_from(&binding.session);
        message.set_presence(presence_message(Some(binding.presence)));
        message.set_custody(custody_message(Some(binding.custody)));
        message.set_issuer_kind(v1::IssuerKind::ReefDaemon);
        message.authority_key_id = binding.issuer.key_id.to_string();
        message.not_before = Some(timestamp(Timestamp(binding.not_before_ms)));
        message.not_after = Some(timestamp(Timestamp(binding.not_after_ms)));
        if with_bytes {
            message.public_key = binding.public_key.spki().to_vec();
        }
    }
    // An invalid certificate: only `key_id`, `certificate_valid: false` and
    // the daemon's own records (set above: `issued_at`, `issued_by_agent`,
    // `remote_token_id` and the key's own revocation) are reported; the
    // identity, kind, actor, presence, session and window stay unset, since
    // nothing vouches for the columns.
    message
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
