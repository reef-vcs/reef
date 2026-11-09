//! Sigstore keyless session keys (R3.42, ADR 0066): registering a key that
//! Fulcio certified against an OIDC identity, and the OIDC matches that
//! decide which operator identity it is bound to.
//!
//! `RegisterSigstoreKey` checks, in order: that keyless keys are on and the
//! trusted root loaded; the bundle (`reef_evidence::sigstore::
//! check_sigstore_bundle`, offline, on the blocking pool); the live OIDC
//! match whose issuer and subject equal the certificate's; the identity
//! (live, and `human` never for `ci`); the session the statement names
//! (a keyless session with the same identity and actor, unrevoked); that
//! the key is fresh (the same bundle again returns the record); and the
//! identity's limit of unexpired keys. The channel decides nothing: the
//! proof is Fulcio's and Rekor's, and every bound field is signed by the
//! key, so registering someone else's public bundle gains nothing.
//!
//! The daemon never fetches anything: the trusted root is the public-good
//! root Reef ships, or `[sigstore] trusted_root`, read at start.

use std::sync::Arc;

use reef_evidence::session::{Actor, IdentityKind, IssuerKind, Presence};
use reef_evidence::sigstore::{SigstoreCertified, TrustedRoot, check_sigstore_bundle};

use super::issue::{Channel, Refusal, invalid_from};
use super::read::ReadKey;
use super::{Identities, IdentityError, audit, new_id};
use crate::config::SigstoreSection;
use crate::db::{
    AuthorityRow, DbError, KeyInserted, OidcMatchRow, RevocationRow, SessionKeyRow, SessionRow,
};
use crate::model::Timestamp;

/// The channel a keyless session records: its keys come through any.
pub const CHANNEL: &str = "sigstore";

/// The longest OIDC issuer, in bytes.
pub const MAX_ISSUER_BYTES: usize = 256;

/// The longest OIDC subject, in bytes.
pub const MAX_SUBJECT_BYTES: usize = 1024;

/// The most keys a revoked match's response lists.
pub const MAX_LISTED_KEYS: usize = 1000;

/// `[sigstore]` as loaded at start: the trusted root, or why there is none.
#[derive(Debug)]
pub struct Keyless {
    root: Option<Arc<TrustedRoot>>,
    problem: &'static str,
    detail: String,
}

impl Keyless {
    /// Loads `[sigstore]`'s trusted root. An unusable one is logged and
    /// leaves keyless keys unavailable; the daemon still starts.
    pub fn load(section: &SigstoreSection) -> Keyless {
        if !section.enabled {
            return Keyless {
                root: None,
                problem: "disabled",
                detail: String::new(),
            };
        }
        let loaded = if section.trusted_root.is_empty() {
            TrustedRoot::embedded().map_err(|error| error.to_string())
        } else {
            std::fs::read(&section.trusted_root)
                .map_err(|error| format!("{}: {error}", section.trusted_root))
                .and_then(|json| {
                    TrustedRoot::from_json(&json, &section.trusted_root)
                        .map_err(|error| error.to_string())
                })
        };
        match loaded {
            Ok(root) => {
                tracing::info!(
                    source = root.source(),
                    digest = root.digest(),
                    "loaded the Sigstore trusted root for keyless keys"
                );
                Keyless {
                    root: Some(Arc::new(root)),
                    problem: "",
                    detail: String::new(),
                }
            }
            Err(detail) => {
                tracing::warn!(
                    problem = %detail,
                    "the Sigstore trusted root is not usable; keyless keys are refused and read as invalid until it is fixed"
                );
                Keyless {
                    root: None,
                    problem: "trusted_root",
                    detail,
                }
            }
        }
    }

    /// The trusted root, or why there is none.
    fn root(&self) -> Result<Arc<TrustedRoot>, IdentityError> {
        self.root
            .clone()
            .ok_or_else(|| IdentityError::SigstoreUnavailable {
                problem: self.problem,
                detail: self.detail.clone(),
            })
    }
}

/// Why `issuer` and `subject` are not a match's, if they are not.
pub fn match_problem(issuer: &str, subject: &str) -> Option<IdentityError> {
    let issuer_ok = issuer.len() <= MAX_ISSUER_BYTES
        && issuer
            .strip_prefix("https://")
            .is_some_and(|rest| !rest.is_empty())
        && issuer.bytes().all(|b| b.is_ascii_graphic());
    if !issuer_ok {
        return Some(IdentityError::InvalidMatch {
            field: "issuer",
            message: format!(
                "{issuer:?} is not an https:// URL of at most {MAX_ISSUER_BYTES} bytes of visible ASCII"
            ),
        });
    }
    let subject_ok = !subject.is_empty()
        && subject.len() <= MAX_SUBJECT_BYTES
        && subject.bytes().all(|b| b.is_ascii_graphic());
    if !subject_ok {
        return Some(IdentityError::InvalidMatch {
            field: "subject",
            message: format!("{subject:?} is not 1 to {MAX_SUBJECT_BYTES} bytes of visible ASCII"),
        });
    }
    None
}

impl Identities {
    /// The keyless trusted root, when keyless keys are on and it loaded.
    pub fn keyless_root(&self) -> Option<Arc<TrustedRoot>> {
        self.sigstore.root.clone()
    }

    /// Adds a match to `identity`: it accepts keyless certificates for
    /// `issuer` and `subject` from now on.
    pub fn add_match(
        &self,
        identity: &str,
        issuer: &str,
        subject: &str,
    ) -> Result<OidcMatchRow, IdentityError> {
        if identity.trim().is_empty() {
            return Err(IdentityError::NotFound {
                name: String::new(),
            });
        }
        let identity = self.find(identity)?;
        if let Some(revocation) = &identity.revocation {
            return Err(IdentityError::Revoked {
                name: identity.name.clone(),
                revoked_at: revocation.revoked_at,
            });
        }
        if let Some(problem) = match_problem(issuer, subject) {
            return Err(problem);
        }
        let row = OidcMatchRow {
            id: self.unused_oidc_id()?,
            identity_id: identity.id.clone(),
            issuer: issuer.to_owned(),
            subject: subject.to_owned(),
            created_at: Timestamp::now(),
            revocation: None,
        };
        if let Some(existing) = self.db.insert_oidc_match(&row)? {
            let holder = self
                .db
                .identity(&existing.identity_id)?
                .map_or(existing.identity_id, |row| row.name);
            return Err(IdentityError::MatchExists { identity: holder });
        }
        audit::oidc_changed("add", &row, &identity.name);
        Ok(row)
    }

    /// Revokes match `id`; the match after, and the keys bound through it.
    pub fn revoke_match(
        &self,
        id: &str,
        given: Option<&prost_types::Timestamp>,
        since_created: bool,
        reason: &str,
    ) -> Result<(OidcMatchRow, Vec<String>), IdentityError> {
        let id = id.trim();
        let row = self
            .db
            .oidc_match(id)?
            .ok_or_else(|| IdentityError::MatchNotFound { id: id.to_owned() })?;
        let now = Timestamp::now();
        let from = invalid_from(given, since_created, row.created_at, now)?;
        let revocation = RevocationRow {
            revoked_at: now,
            invalid_from: from,
            reason: Some(reason.to_owned()).filter(|reason| !reason.is_empty()),
        };
        let revoked = self
            .db
            .revoke_oidc_match(&row.id, &revocation)?
            .ok_or_else(|| IdentityError::MatchNotFound { id: id.to_owned() })?;
        let keys = self.db.keys_of_oidc_match(&row.id, MAX_LISTED_KEYS)?;
        let applied = revoked
            .revocation
            .as_ref()
            .map_or(from, |revocation| revocation.invalid_from);
        audit::revoked(
            "oidc_match",
            &revoked.id,
            applied,
            revocation.reason.as_deref(),
            keys.len(),
        );
        Ok((revoked, keys))
    }

    fn unused_oidc_id(&self) -> Result<String, DbError> {
        loop {
            let id = new_id("oidc_");
            if !self.db.oidc_id_taken(&id)? {
                return Ok(id);
            }
        }
    }

    /// Registers a keyless key from its bundle; the record, and whether this
    /// call stored it.
    #[expect(
        clippy::too_many_lines,
        reason = "the registration steps, in their order"
    )]
    pub async fn register(
        &self,
        bundle: Vec<u8>,
        channel: Channel,
        agent: Option<String>,
    ) -> Result<(ReadKey, bool), IdentityError> {
        // 1. The trusted root, then the bundle, on the blocking pool.
        let root = self.sigstore.root()?;
        let checked_bundle = bundle.clone();
        let certified: SigstoreCertified = tokio::task::spawn_blocking(move || {
            check_sigstore_bundle(&checked_bundle, &root).map_err(IdentityError::InvalidBundle)
        })
        .await
        .map_err(|error| IdentityError::Task(error.to_string()))??;
        // 2. The match.
        let matched = self
            .db
            .live_oidc_match(&certified.oidc_issuer, &certified.oidc_subject)?
            .ok_or_else(|| IdentityError::Unmatched {
                issuer: certified.oidc_issuer.clone(),
                subject: certified.oidc_subject.clone(),
            })?;
        // 3. The identity.
        let identity =
            self.db
                .identity(&matched.identity_id)?
                .ok_or_else(|| IdentityError::NotFound {
                    name: matched.identity_id.clone(),
                })?;
        if let Some(revocation) = &identity.revocation {
            return Err(IdentityError::Revoked {
                name: identity.name.clone(),
                revoked_at: revocation.revoked_at,
            });
        }
        let ci = identity.kind == IdentityKind::Ci.as_str();
        if ci && certified.actor == Actor::Human {
            return Err(IdentityError::Refused {
                rule: Refusal::HumanForCi,
            });
        }
        // 4. The key: fresh, or this very bundle again.
        let key_id = certified.public_key.id().to_string();
        match self.db.key_state(&key_id)? {
            Some(true) => return self.existing(&key_id, &bundle).await,
            Some(false) => {
                return Err(IdentityError::KeyExists {
                    key_id,
                    state: "used",
                });
            }
            None => {}
        }
        // 5. The session the statement names.
        let now = Timestamp::now();
        let new_session = match self.db.session(&certified.session)? {
            Some(session) => {
                if session.channel != CHANNEL {
                    return Err(IdentityError::Refused {
                        rule: Refusal::SessionChannelMismatch,
                    });
                }
                if session.identity_id != identity.id || session.actor != certified.actor.as_str() {
                    return Err(IdentityError::Refused {
                        rule: Refusal::SessionMismatch,
                    });
                }
                if session.revocation.is_some() {
                    return Err(IdentityError::Refused {
                        rule: Refusal::SessionRevoked,
                    });
                }
                None
            }
            None => Some(SessionRow {
                id: certified.session.clone(),
                identity_id: identity.id.clone(),
                actor: certified.actor.as_str().to_owned(),
                agent: certified.agent.clone(),
                created_at: now,
                channel: CHANNEL.to_owned(),
                revocation: None,
            }),
        };
        // 6. The Fulcio CA as an authority, then the key and its session.
        let ca = &certified.ca;
        let authority = self.db.record_sigstore_authority(&AuthorityRow {
            id: ca.key_id.to_string(),
            algorithm: ca.algorithm.clone(),
            public_key: ca.public_key.clone(),
            host: ca.host.clone(),
            created_at: now,
            retired_at: Some(now),
            revocation: None,
            kind: IssuerKind::Sigstore.as_str().to_owned(),
        })?;
        let (token_id, token_name) = match &channel {
            Channel::Remote {
                token_id,
                token_name,
                ..
            } => (Some(token_id.clone()), Some(token_name.clone())),
            Channel::Socket => (None, None),
        };
        let row = SessionKeyRow {
            key_id: key_id.clone(),
            session_id: certified.session.clone(),
            identity_id: identity.id.clone(),
            authority_id: authority.id,
            actor: certified.actor.as_str().to_owned(),
            presence: Presence::None.as_str().to_owned(),
            custody: certified.custody.as_str().to_owned(),
            certificate: bundle.clone(),
            not_before: Timestamp(certified.not_before_ms),
            not_after: Timestamp(certified.not_after_ms),
            issued_at: now,
            issued_by_agent: agent,
            remote_token_id: token_id,
            revocation: None,
            issuer_kind: IssuerKind::Sigstore.as_str().to_owned(),
            oidc_id: Some(matched.id.clone()),
        };
        let limit = self.limit();
        let mut inserted = self
            .db
            .insert_session_key(new_session.as_ref(), &row, limit);
        // Two registrations naming one new session: the loser finds it
        // made, and joins it if it is the same identity and actor.
        if inserted.is_err()
            && new_session.is_some()
            && let Some(session) = self.db.session(&certified.session)?
        {
            if session.channel != CHANNEL
                || session.identity_id != identity.id
                || session.actor != certified.actor.as_str()
            {
                return Err(IdentityError::Refused {
                    rule: Refusal::SessionMismatch,
                });
            }
            inserted = self.db.insert_session_key(None, &row, limit);
        }
        match inserted {
            Ok(KeyInserted::Stored) => {}
            Ok(KeyInserted::Full) => {
                return Err(IdentityError::TooMany {
                    identity: identity.name.clone(),
                    limit,
                });
            }
            // Raced with another registration of the key.
            Err(DbError::Conflict { .. }) => return self.existing(&key_id, &bundle).await,
            Err(error) => return Err(error.into()),
        }
        audit::registered(&row, &identity.name, &certified, token_name.as_deref());
        let read = self
            .read(vec![row])
            .await?
            .pop()
            .ok_or_else(|| IdentityError::KeyNotFound {
                field: "key_id",
                id: key_id,
            })?;
        Ok((read, true))
    }

    /// A key registered already: the record when it is `bundle`'s, else
    /// `SESSION_KEY_EXISTS`.
    async fn existing(
        &self,
        key_id: &str,
        bundle: &[u8],
    ) -> Result<(ReadKey, bool), IdentityError> {
        let rows = self.db.session_keys_by_id(&[key_id])?;
        if rows.first().is_some_and(|row| {
            row.issuer_kind == IssuerKind::Sigstore.as_str() && row.certificate == bundle
        }) && let Some(read) = self.read(rows).await?.pop()
        {
            return Ok((read, false));
        }
        Err(IdentityError::KeyExists {
            key_id: key_id.to_owned(),
            state: "certified",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_problems() {
        assert!(
            match_problem(
                "https://token.actions.githubusercontent.com",
                "https://github.com/o/r/.github/workflows/w.yml@refs/heads/main"
            )
            .is_none()
        );
        assert!(match_problem("https://accounts.google.com", "alice@example.com").is_none());
        for (issuer, subject, field) in [
            ("http://insecure.example", "a", "issuer"),
            ("https://", "a", "issuer"),
            ("https://a b", "a", "issuer"),
            ("https://ok.example", "", "subject"),
            ("https://ok.example", "has space", "subject"),
        ] {
            match match_problem(issuer, subject) {
                Some(IdentityError::InvalidMatch { field: got, .. }) => assert_eq!(got, field),
                other => panic!("{issuer} {subject}: {other:?}"),
            }
        }
        assert!(match_problem(&format!("https://{}", "a".repeat(260)), "x").is_some());
        assert!(match_problem("https://a", &"x".repeat(1025)).is_some());
    }
}
