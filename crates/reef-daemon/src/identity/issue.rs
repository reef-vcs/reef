//! Issuing session keys (R3.03, ADR 0057), and revoking them, their
//! sessions, identities and authorities.
//!
//! `IssueSessionKey` checks, in order: the key and the proof of possession
//! (on the blocking pool); the identity, by the channel's rules; the
//! session, when one is continued; that the key is fresh; the identity's
//! limit of unexpired keys. Then the authority signs the certificate, with
//! `notBefore` now, holding the authority's lock only for the signature,
//! and one transaction stores the session (if new) and the key.
//!
//! Who can get a key for what:
//!
//! | Caller | `person` | `ci` | `human` |
//! | --- | --- | --- | --- |
//! | The socket | any live one; empty is the default | `ci_over_socket` | allowed, `presence: none` |
//! | A remote token linked to X | X only (`token_identity_mismatch`) | X only | `human_over_remote` |
//! | A remote token with no link | `token_not_linked` | `token_not_linked` | refused |
//!
//! and `human` for a `ci` identity is `human_for_ci`.

use std::sync::{Arc, PoisonError};
use std::time::Duration;

use reef_api::v1::{self, IssueSessionKeyRequest};
use reef_evidence::session::{
    Actor, Binding, Custody, IdentityKind, IdentityRef, Issuer, IssuerKind, Presence, ProofFor,
    certify, verify_proof,
};
use reef_evidence::{KeyId, PublicKey};

use super::read::{ReadKey, read_keys};
use super::{Identities, IdentityError, MIN_LIFETIME, audit, new_id};
use crate::db::{
    AuthorityRow, DbError, IdentityRow, KeyInserted, RevocationRow, SessionKeyRow, SessionRow,
};
use crate::model::Timestamp;

/// An issuance rule's refusal: `SESSION_KEY_REFUSED`'s `rule`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A `ci` identity over the socket.
    CiOverSocket,
    /// `human` through the remote listener.
    HumanOverRemote,
    /// `human` for a `ci` identity.
    HumanForCi,
    /// A remote token linked to no identity.
    TokenNotLinked,
    /// A remote token naming another identity than its own.
    TokenIdentityMismatch,
    /// A renewal with another identity or actor than the session's.
    SessionMismatch,
    /// A renewal of a revoked session.
    SessionRevoked,
    /// A renewal through another channel than the session's first key's.
    SessionChannelMismatch,
}

impl Refusal {
    /// The `rule` metadata value.
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::CiOverSocket => "ci_over_socket",
            Refusal::HumanOverRemote => "human_over_remote",
            Refusal::HumanForCi => "human_for_ci",
            Refusal::TokenNotLinked => "token_not_linked",
            Refusal::TokenIdentityMismatch => "token_identity_mismatch",
            Refusal::SessionMismatch => "session_mismatch",
            Refusal::SessionRevoked => "session_revoked",
            Refusal::SessionChannelMismatch => "session_channel_mismatch",
        }
    }

    /// What was refused.
    pub fn message(self) -> &'static str {
        match self {
            Refusal::CiOverSocket => {
                "a ci identity's session keys are never issued over the socket"
            }
            Refusal::HumanOverRemote => {
                "a key declared human is never issued through the remote listener"
            }
            Refusal::HumanForCi => "a ci identity is a pipeline; its keys are never human",
            Refusal::TokenNotLinked => {
                "this remote token is linked to no identity, so it gets no session keys"
            }
            Refusal::TokenIdentityMismatch => {
                "a remote token gets session keys only for the identity it is linked to"
            }
            Refusal::SessionMismatch => {
                "a session is renewed only with the identity and actor of its first key"
            }
            Refusal::SessionRevoked => "the session is revoked",
            Refusal::SessionChannelMismatch => {
                "a session is renewed only through the channel its first key was issued through"
            }
        }
    }

    /// Which rule refused, and what would work.
    pub fn hint(self) -> &'static str {
        match self {
            Refusal::CiOverSocket => {
                "a ci identity's keys are issued only to a remote token linked to it: `reef remote token create <name> --scope write --identity <ci>`"
            }
            Refusal::HumanOverRemote => {
                "Keys declared human are issued only on the daemon's socket; a remote caller declares agent or executor."
            }
            Refusal::HumanForCi => "Declare agent or executor for a ci identity's keys.",
            Refusal::TokenNotLinked => {
                "On the daemon's machine, create a token linked to an identity: `reef remote token create <name> --scope write --identity <identity>`."
            }
            Refusal::TokenIdentityMismatch => {
                "Leave the identity empty to get a key for the token's own identity, or use a token linked to the one you want."
            }
            Refusal::SessionMismatch => {
                "Renew a session with its first key's identity and actor, or leave the session empty for a new one."
            }
            Refusal::SessionRevoked => "Leave the session empty to start a new one.",
            Refusal::SessionChannelMismatch => {
                "Renew the session through the socket or remote token that started it, or leave the session empty for a new one."
            }
        }
    }
}

/// Where an issuance request came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Channel {
    /// The socket: every client is the daemon's user.
    Socket,
    /// The remote listener, with a token.
    Remote {
        /// The token's id.
        token_id: String,
        /// Its name.
        token_name: String,
        /// The identity it is linked to, by id.
        linked: Option<String>,
    },
}

impl Channel {
    /// What a session records of it: `socket`, or the remote token's id.
    pub fn name(&self) -> &str {
        match self {
            Channel::Socket => "socket",
            Channel::Remote { token_id, .. } => token_id,
        }
    }
}

/// The issuance rules for `identity` and `actor` through `channel`.
pub fn check_rules(channel: &Channel, identity: &IdentityRow, actor: Actor) -> Result<(), Refusal> {
    let ci = identity.kind == IdentityKind::Ci.as_str();
    let remote = matches!(channel, Channel::Remote { .. });
    if remote && actor == Actor::Human {
        return Err(Refusal::HumanOverRemote);
    }
    if ci && actor == Actor::Human {
        return Err(Refusal::HumanForCi);
    }
    if ci && !remote {
        return Err(Refusal::CiOverSocket);
    }
    Ok(())
}

/// `actor` from the API; `INVALID_SESSION_KEY_REQUEST` (`actor`) when unset.
pub fn actor_of(actor: i32) -> Result<Actor, IdentityError> {
    match v1::SessionActor::try_from(actor) {
        Ok(v1::SessionActor::Human) => Ok(Actor::Human),
        Ok(v1::SessionActor::Agent) => Ok(Actor::Agent),
        Ok(v1::SessionActor::Executor) => Ok(Actor::Executor),
        _ => Err(IdentityError::InvalidRequest {
            field: "actor",
            message: "declare the key's actor: human, agent or executor".into(),
        }),
    }
}

/// When a revocation applies from: `invalid_from`, or `since` (the
/// record's start) with `since_flag`, or now. Refused when both are given,
/// or for a time after now.
pub fn invalid_from(
    given: Option<&prost_types::Timestamp>,
    since_flag: bool,
    since: Timestamp,
    now: Timestamp,
) -> Result<Timestamp, IdentityError> {
    match (given, since_flag) {
        (Some(_), true) => Err(IdentityError::InvalidRevocation {
            field: "invalid_from",
            message: "give invalid_from or the since flag, not both".into(),
        }),
        (None, true) => Ok(since.min(now)),
        (None, false) => Ok(now),
        (Some(at), false) => {
            let millis = i128::from(at.seconds) * 1000 + i128::from(at.nanos) / 1_000_000;
            let at = Timestamp(i64::try_from(millis).unwrap_or(i64::MAX));
            if at > now {
                return Err(IdentityError::InvalidRevocation {
                    field: "invalid_from",
                    message: format!("invalid_from {} is in the future", at.to_rfc3339()),
                });
            }
            Ok(at)
        }
    }
}

fn task(error: &tokio::task::JoinError) -> IdentityError {
    IdentityError::Task(error.to_string())
}

impl Identities {
    /// A request's lifetime: `[session_keys] lifetime` when unset, else
    /// from 5 minutes to `[session_keys] max_lifetime`.
    fn lifetime_of(
        &self,
        lifetime: Option<&prost_types::Duration>,
    ) -> Result<Duration, IdentityError> {
        let Some(lifetime) = lifetime else {
            return Ok(self.settings.lifetime);
        };
        let max = self.settings.max_lifetime;
        let millis = i128::from(lifetime.seconds) * 1000 + i128::from(lifetime.nanos) / 1_000_000;
        u64::try_from(millis)
            .map(Duration::from_millis)
            .ok()
            .filter(|life| (MIN_LIFETIME..=max).contains(life))
            .ok_or_else(|| IdentityError::InvalidRequest {
                field: "lifetime",
                message: format!(
                    "a session key lives from 5m to [session_keys] max_lifetime ({}s)",
                    max.as_secs()
                ),
            })
    }

    /// The identity a request through `channel` gets a key for.
    fn identity_for(&self, channel: &Channel, named: &str) -> Result<IdentityRow, IdentityError> {
        match channel {
            Channel::Socket => self.find(named),
            Channel::Remote { linked: None, .. } => Err(IdentityError::Refused {
                rule: Refusal::TokenNotLinked,
            }),
            Channel::Remote {
                linked: Some(id), ..
            } => {
                let linked = self
                    .db
                    .identity(id)?
                    .ok_or_else(|| IdentityError::NotFound { name: id.clone() })?;
                if !named.is_empty() && named != linked.name && named != linked.id {
                    return Err(IdentityError::Refused {
                        rule: Refusal::TokenIdentityMismatch,
                    });
                }
                Ok(linked)
            }
        }
    }

    /// `IssueSessionKey`: certifies the request's key for the identity its
    /// channel allows; see the module documentation.
    #[expect(clippy::too_many_lines, reason = "the issuance steps, in their order")]
    pub async fn issue(
        &self,
        request: IssueSessionKeyRequest,
        channel: Channel,
        agent: Option<String>,
    ) -> Result<ReadKey, IdentityError> {
        let actor = actor_of(request.actor)?;
        let session_sent = request.session_id.clone();
        if !session_sent.is_empty() && !reef_evidence::session::is_session_id(&session_sent) {
            return Err(IdentityError::InvalidRequest {
                field: "session_id",
                message: format!("{session_sent:?} is not ses_ and 16 lowercase hex digits"),
            });
        }
        // 1. The key and the proof, on the blocking pool.
        let (der, proof, named) = (
            request.public_key.clone(),
            request.proof.clone(),
            request.identity.clone(),
        );
        let session_for_proof = session_sent.clone();
        // The proof is addressed to the current authority; the signature
        // below checks that it still is.
        let audience = self.current_authority().id;
        let audience_for_proof = audience.clone();
        let public_key = tokio::task::spawn_blocking(move || {
            let key =
                PublicKey::from_spki(&der).map_err(|invalid| IdentityError::InvalidRequest {
                    field: "public_key",
                    message: invalid.to_string(),
                })?;
            let authority = KeyId::parse(&audience_for_proof).unwrap_or_else(|| KeyId::of(&[]));
            let proof_for = ProofFor {
                authority: &authority,
                identity: &named,
                actor,
                session: &session_for_proof,
            };
            if !verify_proof(&key, &proof_for, &proof) {
                return Err(IdentityError::InvalidProof {
                    key_id: key.id().to_string(),
                    authority_key_id: audience_for_proof,
                });
            }
            Ok(key)
        })
        .await
        .map_err(|error| task(&error))??;
        let lifetime = self.lifetime_of(request.lifetime.as_ref())?;
        // 2. The rules.
        let identity = self.identity_for(&channel, &request.identity)?;
        if let Some(revocation) = &identity.revocation {
            return Err(IdentityError::Revoked {
                name: identity.name.clone(),
                revoked_at: revocation.revoked_at,
            });
        }
        check_rules(&channel, &identity, actor).map_err(|rule| IdentityError::Refused { rule })?;
        let session = if session_sent.is_empty() {
            None
        } else {
            let session =
                self.db
                    .session(&session_sent)?
                    .ok_or_else(|| IdentityError::InvalidRequest {
                        field: "session_id",
                        message: format!("no session is {session_sent}"),
                    })?;
            if session.identity_id != identity.id || session.actor != actor.as_str() {
                return Err(IdentityError::Refused {
                    rule: Refusal::SessionMismatch,
                });
            }
            if session.revocation.is_some() {
                return Err(IdentityError::Refused {
                    rule: Refusal::SessionRevoked,
                });
            }
            if session.channel != channel.name() {
                return Err(IdentityError::Refused {
                    rule: Refusal::SessionChannelMismatch,
                });
            }
            Some(session)
        };
        // 3. A fresh key.
        let key_id = public_key.id().to_string();
        match self.db.key_state(&key_id)? {
            Some(true) => {
                return Err(IdentityError::KeyExists {
                    key_id,
                    state: "certified",
                });
            }
            Some(false) => {
                return Err(IdentityError::KeyExists {
                    key_id,
                    state: "used",
                });
            }
            None => {}
        }
        // 4. The limit is counted in the insert's transaction (step 6).
        let now = Timestamp::now();
        let limit = self.limit();
        // 5. The certificate.
        let new_session = match &session {
            Some(_) => None,
            None => Some(SessionRow {
                id: self.unused_session_id()?,
                identity_id: identity.id.clone(),
                actor: actor.as_str().to_owned(),
                agent: agent.clone(),
                created_at: now,
                channel: channel.name().to_owned(),
                revocation: None,
            }),
        };
        let session_id = session
            .as_ref()
            .or(new_session.as_ref())
            .map(|session| session.id.clone())
            .unwrap_or_default();
        let kind = IdentityKind::parse(&identity.kind).unwrap_or(IdentityKind::Person);
        let not_after = Timestamp(
            now.0
                .saturating_add(crate::model::duration_millis(lifetime)),
        );
        let authority = Arc::clone(&self.authority);
        let host = self.host.clone();
        let binding = Binding {
            public_key,
            identity: IdentityRef {
                id: identity.id.clone(),
                name: identity.name.clone(),
                kind,
            },
            actor,
            agent: agent.clone(),
            session: session_id.clone(),
            presence: Presence::None,
            custody: Custody::Client,
            not_before_ms: now.0,
            not_after_ms: not_after.0,
            issuer: Issuer {
                kind: IssuerKind::ReefDaemon,
                key_id: KeyId::of(&[]),
                host,
            },
        };
        let key_id_for_proof = key_id.clone();
        let (certificate, authority_id) = tokio::task::spawn_blocking(move || {
            let mut binding = binding;
            let current = authority.read().unwrap_or_else(PoisonError::into_inner);
            if current.row.id != audience {
                // Rotated since the proof was checked: it names the old one.
                return Err(IdentityError::InvalidProof {
                    key_id: key_id_for_proof,
                    authority_key_id: current.row.id.clone(),
                });
            }
            binding.issuer.key_id = current.key.public_key().id().clone();
            certify(&current.key, &binding)
                .map(|envelope| (envelope.to_bytes(), current.row.id.clone()))
                .map_err(|invalid| IdentityError::InvalidRequest {
                    field: "public_key",
                    message: invalid.to_string(),
                })
        })
        .await
        .map_err(|error| task(&error))??;
        // 6. The session and the key, in one transaction.
        let (token_id, token_name) = match &channel {
            Channel::Remote {
                token_id,
                token_name,
                ..
            } => (Some(token_id.clone()), Some(token_name.as_str())),
            Channel::Socket => (None, None),
        };
        let row = SessionKeyRow {
            key_id: key_id.clone(),
            session_id,
            identity_id: identity.id.clone(),
            authority_id,
            actor: actor.as_str().to_owned(),
            presence: Presence::None.as_str().to_owned(),
            custody: Custody::Client.as_str().to_owned(),
            certificate,
            not_before: now,
            not_after,
            issued_at: now,
            issued_by_agent: agent,
            remote_token_id: token_id,
            revocation: None,
        };
        match self
            .db
            .insert_session_key(new_session.as_ref(), &row, limit)
        {
            Ok(KeyInserted::Stored) => {}
            Ok(KeyInserted::Full) => {
                return Err(IdentityError::TooMany {
                    identity: identity.name.clone(),
                    limit,
                });
            }
            Err(DbError::Conflict { .. }) => {
                return Err(IdentityError::KeyExists {
                    key_id,
                    state: "certified",
                });
            }
            Err(error) => return Err(error.into()),
        }
        audit::issued(&row, &identity.name, token_name);
        // 7. The record and the certificate, read back as any read does.
        self.read(vec![row])
            .await?
            .pop()
            .ok_or_else(|| IdentityError::KeyNotFound {
                field: "key_id",
                id: key_id,
            })
    }

    /// `rows` read on the blocking pool ([`read_keys`]).
    pub async fn read(&self, rows: Vec<SessionKeyRow>) -> Result<Vec<ReadKey>, IdentityError> {
        let db = Arc::clone(&self.db);
        Ok(tokio::task::spawn_blocking(move || read_keys(&db, rows))
            .await
            .map_err(|error| task(&error))??)
    }

    fn unused_session_id(&self) -> Result<String, DbError> {
        loop {
            let id = new_id("ses_");
            if !self.db.session_id_taken(&id)? {
                return Ok(id);
            }
        }
    }

    /// `RevokeIdentity`: the identity, revoked from `invalid_from`.
    pub fn revoke_identity(
        &self,
        name: &str,
        given: Option<&prost_types::Timestamp>,
        since_created: bool,
        reason: &str,
    ) -> Result<IdentityRow, IdentityError> {
        if name.trim().is_empty() {
            return Err(IdentityError::NotFound {
                name: String::new(),
            });
        }
        let identity = self.find(name)?;
        if self.default_id.as_deref() == Some(identity.id.as_str()) {
            return Err(IdentityError::IsDefault {
                name: identity.name,
            });
        }
        let now = Timestamp::now();
        let from = invalid_from(given, since_created, identity.created_at, now)?;
        let revocation = RevocationRow {
            revoked_at: now,
            invalid_from: from,
            reason: Some(reason.to_owned()).filter(|reason| !reason.is_empty()),
        };
        let revoked = self
            .db
            .revoke_identity(&identity.id, &revocation)?
            .ok_or_else(|| IdentityError::NotFound {
                name: name.to_owned(),
            })?;
        let applied = revoked
            .revocation
            .as_ref()
            .map_or(from, |revocation| revocation.invalid_from);
        audit::revoked(
            "identity",
            &revoked.id,
            applied,
            revocation.reason.as_deref(),
            0,
        );
        Ok(revoked)
    }

    /// `RevokeSessionKeys` of one key, found by id or prefix.
    pub async fn revoke_key(
        &self,
        key_id: &str,
        given: Option<&prost_types::Timestamp>,
        since_issued: bool,
        reason: &str,
    ) -> Result<Vec<ReadKey>, IdentityError> {
        let row = self.key_by_prefix(key_id)?;
        let now = Timestamp::now();
        let from = invalid_from(given, since_issued, row.issued_at, now)?;
        let revocation = RevocationRow {
            revoked_at: now,
            invalid_from: from,
            reason: Some(reason.to_owned()).filter(|reason| !reason.is_empty()),
        };
        self.db.revoke_session_key(&row.key_id, &revocation)?;
        audit::revoked(
            "session_key",
            &row.key_id,
            from,
            revocation.reason.as_deref(),
            1,
        );
        let rows = self.db.session_keys_by_id(&[row.key_id.as_str()])?;
        self.read(rows).await
    }

    /// `RevokeSessionKeys` of a session: every key it has.
    pub async fn revoke_session(
        &self,
        session_id: &str,
        given: Option<&prost_types::Timestamp>,
        since_issued: bool,
        reason: &str,
    ) -> Result<Vec<ReadKey>, IdentityError> {
        let session = self
            .db
            .session(session_id)?
            .ok_or_else(|| IdentityError::KeyNotFound {
                field: "session_id",
                id: session_id.to_owned(),
            })?;
        let now = Timestamp::now();
        let from = invalid_from(given, since_issued, session.created_at, now)?;
        let revocation = RevocationRow {
            revoked_at: now,
            invalid_from: from,
            reason: Some(reason.to_owned()).filter(|reason| !reason.is_empty()),
        };
        self.db.revoke_session(&session.id, &revocation)?;
        let rows = self.db.keys_of_session(&session.id)?;
        audit::revoked(
            "session",
            &session.id,
            from,
            revocation.reason.as_deref(),
            rows.len(),
        );
        self.read(rows).await
    }

    /// `RevokeAuthority`: a retired authority, found by key id or a unique
    /// prefix, revoked from `invalid_from`. The current one is refused: it
    /// would go on issuing; `RotateAuthority` with `revoke` retires it.
    pub fn revoke_authority(
        &self,
        key_id: &str,
        given: Option<&prost_types::Timestamp>,
        since_created: bool,
        reason: &str,
    ) -> Result<AuthorityRow, IdentityError> {
        let text = key_id.trim().to_ascii_lowercase();
        let valid = (crate::db::MIN_PREFIX_LEN..=reef_evidence::keys::KEY_ID_LEN)
            .contains(&text.len())
            && text.bytes().all(|b| b.is_ascii_hexdigit());
        if !valid {
            return Err(IdentityError::AuthorityNotFound { key_id: text });
        }
        let matches: Vec<AuthorityRow> = self
            .db
            .authorities()?
            .into_iter()
            .filter(|row| row.id.starts_with(&text))
            .collect();
        let row = match <[AuthorityRow; 1]>::try_from(matches) {
            Ok([row]) => row,
            Err(matches) if matches.is_empty() => {
                return Err(IdentityError::AuthorityNotFound { key_id: text });
            }
            Err(matches) => {
                return Err(IdentityError::AmbiguousAuthority {
                    key_id: text,
                    candidates: matches.into_iter().take(10).map(|row| row.id).collect(),
                });
            }
        };
        if row.retired_at.is_none() {
            return Err(IdentityError::AuthorityIsCurrent { key_id: row.id });
        }
        let now = Timestamp::now();
        let from = invalid_from(given, since_created, row.created_at, now)?;
        let revocation = RevocationRow {
            revoked_at: now,
            invalid_from: from,
            reason: Some(reason.to_owned()).filter(|reason| !reason.is_empty()),
        };
        let revoked = self
            .db
            .revoke_authority(&row.id, &revocation)?
            .ok_or_else(|| IdentityError::AuthorityNotFound {
                key_id: row.id.clone(),
            })?;
        let applied = revoked
            .revocation
            .as_ref()
            .map_or(from, |revocation| revocation.invalid_from);
        audit::revoked(
            "authority",
            &revoked.id,
            applied,
            revocation.reason.as_deref(),
            0,
        );
        Ok(revoked)
    }

    /// The session key whose id is, or starts with, `text`.
    pub fn key_by_prefix(&self, text: &str) -> Result<SessionKeyRow, IdentityError> {
        let text = text.trim();
        let valid = (crate::db::MIN_PREFIX_LEN..=reef_evidence::keys::KEY_ID_LEN)
            .contains(&text.len())
            && text.bytes().all(|b| b.is_ascii_hexdigit());
        if !valid {
            return Err(IdentityError::InvalidKeyId {
                key_id: text.to_owned(),
            });
        }
        match self.db.session_key_by_prefix(text) {
            Ok(row) => Ok(row),
            Err(DbError::NotFound { .. }) => Err(IdentityError::KeyNotFound {
                field: "key_id",
                id: text.to_owned(),
            }),
            Err(DbError::Ambiguous { candidates, .. }) => Err(IdentityError::AmbiguousKey {
                key_id: text.to_owned(),
                candidates,
            }),
            Err(error) => Err(error.into()),
        }
    }

    /// `RotateAuthority`: a new current key; the old one retired, and
    /// revoked when `revoke`. Returns the old row as it is after, and the
    /// new one.
    pub async fn rotate(
        &self,
        revoke: bool,
        given: Option<&prost_types::Timestamp>,
        reason: &str,
    ) -> Result<(AuthorityRow, AuthorityRow), IdentityError> {
        if given.is_some() && !revoke {
            return Err(IdentityError::InvalidRevocation {
                field: "invalid_from",
                message: "invalid_from applies only with revoke".into(),
            });
        }
        let now = Timestamp::now();
        let given = match given {
            Some(_) => Some(invalid_from(given, false, now, now)?),
            None => None,
        };
        let reason = Some(reason.to_owned()).filter(|reason| !reason.is_empty());
        let (db, dir, host, authority) = (
            Arc::clone(&self.db),
            self.dir.clone(),
            self.host.clone(),
            Arc::clone(&self.authority),
        );
        let (old, current) = tokio::task::spawn_blocking(move || {
            // Issuance waits on the lock only while the new key is made.
            let mut slot = authority.write().unwrap_or_else(PoisonError::into_inner);
            let old = slot.row.clone();
            let revocation = revoke.then(|| RevocationRow {
                revoked_at: now,
                invalid_from: given.unwrap_or(old.created_at).min(now),
                reason,
            });
            let made = super::authority::rotate(&dir, &db, &host, &old, revocation.as_ref())?;
            let row = made.row.clone();
            *slot = made;
            if let Some(revocation) = &revocation {
                audit::revoked(
                    "authority",
                    &old.id,
                    revocation.invalid_from,
                    revocation.reason.as_deref(),
                    0,
                );
            }
            Ok::<_, IdentityError>((old, row))
        })
        .await
        .map_err(|error| task(&error))??;
        let retired = self
            .db
            .authorities()?
            .into_iter()
            .find(|row| row.id == old.id)
            .unwrap_or(old);
        Ok((retired, current))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(kind: IdentityKind) -> IdentityRow {
        IdentityRow {
            id: "idn_0000000000000001".into(),
            name: "x".into(),
            kind: kind.as_str().into(),
            display_name: None,
            email: None,
            created_at: Timestamp(1),
            revocation: None,
        }
    }

    // The rules table of the module documentation.
    #[test]
    fn rules() {
        let remote = Channel::Remote {
            token_id: "t".into(),
            token_name: "n".into(),
            linked: Some("idn_0000000000000001".into()),
        };
        let person = identity(IdentityKind::Person);
        let ci = identity(IdentityKind::Ci);
        let socket = Channel::Socket;
        assert_eq!(check_rules(&socket, &person, Actor::Human), Ok(()));
        assert_eq!(check_rules(&socket, &person, Actor::Agent), Ok(()));
        assert_eq!(
            check_rules(&socket, &ci, Actor::Agent),
            Err(Refusal::CiOverSocket)
        );
        assert_eq!(
            check_rules(&socket, &ci, Actor::Human),
            Err(Refusal::HumanForCi)
        );
        assert_eq!(check_rules(&remote, &ci, Actor::Executor), Ok(()));
        assert_eq!(check_rules(&remote, &person, Actor::Agent), Ok(()));
        assert_eq!(
            check_rules(&remote, &ci, Actor::Human),
            Err(Refusal::HumanOverRemote)
        );
        assert_eq!(
            check_rules(&remote, &person, Actor::Human),
            Err(Refusal::HumanOverRemote)
        );
    }

    #[test]
    fn revocation_times() {
        let now = Timestamp(1_000);
        let at = |seconds| prost_types::Timestamp { seconds, nanos: 0 };
        assert_eq!(invalid_from(None, false, Timestamp(5), now).unwrap(), now);
        assert_eq!(
            invalid_from(None, true, Timestamp(5), now).unwrap(),
            Timestamp(5)
        );
        assert_eq!(
            invalid_from(Some(&at(0)), false, Timestamp(5), now).unwrap(),
            Timestamp(0)
        );
        assert!(invalid_from(Some(&at(2)), false, Timestamp(5), now).is_err());
        assert!(invalid_from(Some(&at(0)), true, Timestamp(5), now).is_err());
    }
}
