//! Passkeys (R3.43, ADR 0085): the `WebAuthn` credentials that verify a
//! person, registered to a person identity from the web UI.
//!
//! An identity's first passkey is added from any web UI session; every
//! later add, and every removal, needs a user-verified assertion by one of
//! its live passkeys over a challenge derived from the change
//! ([`reef_evidence::webauthn::passkey_change_challenge`]). Registration
//! challenges are in memory: 32 random bytes, single use, 5 minutes, and
//! only for the web UI session that started them. A reset (the socket's
//! `ResetPasskeys`) removes them all and revokes the unexpired keys they
//! certified. One lock serializes every change to the passkeys, so the
//! check that an identity has none and the add that follows it are one
//! step.
//!
//! `Identities::verify_assertion` is the check every assertion goes
//! through, an approval's too: a live passkey of the identity, the pure
//! checks of `reef_evidence::webauthn`, the exact origin of the web UI's
//! listener (the RP ID `localhost` is every local web server's), and a sign
//! count that went up.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rand::RngExt as _;
use reef_api::v1;
use reef_evidence::session::IdentityKind;
use reef_evidence::webauthn::{
    self, Algorithm, Assertion, AssertionError, PasskeyChange, PasskeyKey,
};

use super::issue::Channel;
use super::{Identities, IdentityError, Refusal, audit, new_id};
use crate::db::{IdentityRow, PasskeyInserted, PasskeyRow};
use crate::model::Timestamp;

/// The RP ID of the web UI's listener, which serves `http://localhost`.
pub const RP_ID: &str = "localhost";

/// The most live passkeys one identity may hold.
pub const MAX_PASSKEYS: usize = 32;

/// How long a registration challenge is accepted.
pub const CHALLENGE_LIFETIME: Duration = Duration::from_mins(5);

/// The most registration challenges held at once; one more drops the
/// oldest.
const MAX_CHALLENGES: usize = 64;

/// The longest label, in bytes.
const MAX_LABEL_BYTES: usize = 64;

/// The algorithms a registration offers, in order of preference.
pub const ALGORITHMS: [Algorithm; 3] = [Algorithm::EdDsa, Algorithm::Es256, Algorithm::Rs256];

/// A registration challenge in flight.
#[derive(Debug)]
struct Started {
    identity_id: String,
    web_session: String,
    expires: Instant,
}

/// The passkey state `Identities` holds: registration challenges, and the
/// lock every change to the passkeys takes.
#[derive(Debug, Default)]
pub(crate) struct Passkeys {
    challenges: Mutex<HashMap<[u8; 32], Started>>,
    changes: Mutex<()>,
}

impl Passkeys {
    fn challenges(&self) -> MutexGuard<'_, HashMap<[u8; 32], Started>> {
        self.challenges
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn change(&self) -> MutexGuard<'_, ()> {
        self.changes.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A started registration, as `StartPasskeyRegistration` answers it.
#[derive(Debug)]
pub struct Registration {
    /// The challenge.
    pub challenge: [u8; 32],
    /// When it stops being accepted.
    pub expires_at: Timestamp,
    /// The identity.
    pub identity: IdentityRow,
    /// Its live passkeys.
    pub live: Vec<PasskeyRow>,
}

/// The web UI session of `channel`, for a write only the web UI makes:
/// `PASSKEY_NEEDS_WEB` on the socket, `PASSKEY_OVER_REMOTE` remotely.
pub(crate) fn web_only<'a>(
    channel: &'a Channel,
    method: &'static str,
) -> Result<&'a str, IdentityError> {
    match channel {
        Channel::Web { session } => Ok(session),
        Channel::Socket => Err(IdentityError::PasskeyNeedsWeb { method }),
        Channel::Remote { .. } => Err(IdentityError::PasskeyOverRemote { method }),
    }
}

/// `PASSKEY_ASSERTION_INVALID` for a failed pure check.
pub(crate) fn invalid(error: AssertionError) -> IdentityError {
    IdentityError::PasskeyInvalid {
        check: error.check.as_str(),
        message: error.message,
    }
}

/// `assertion` as `reef_evidence`'s.
pub(crate) fn assertion_of(assertion: &v1::PasskeyAssertion) -> Assertion {
    Assertion {
        authenticator_data: assertion.authenticator_data.clone(),
        client_data_json: assertion.client_data_json.clone(),
        signature: assertion.signature.clone(),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn label_problem(label: &str) -> Option<String> {
    if label.trim().is_empty() || label.len() > MAX_LABEL_BYTES {
        return Some(format!(
            "{} bytes; a passkey's label is 1 to {MAX_LABEL_BYTES}",
            label.len()
        ));
    }
    label
        .chars()
        .any(char::is_control)
        .then(|| "a passkey's label has no control characters".to_owned())
}

impl Identities {
    /// The person identity `named` (empty: the default), live: refused for
    /// a revoked one, and with `human_for_ci` for a `ci` one, whose keys
    /// are never a person's.
    pub(crate) fn person(&self, named: &str) -> Result<IdentityRow, IdentityError> {
        let identity = self.find(named)?;
        if let Some(revocation) = &identity.revocation {
            return Err(IdentityError::Revoked {
                name: identity.name.clone(),
                revoked_at: revocation.revoked_at,
            });
        }
        if identity.kind != IdentityKind::Person.as_str() {
            return Err(IdentityError::Refused {
                rule: Refusal::HumanForCi,
            });
        }
        Ok(identity)
    }

    /// `ListPasskeys`: `named`'s passkeys (empty: every identity's), with
    /// the identities they belong to.
    pub fn list_passkeys(
        &self,
        named: &str,
        include_removed: bool,
    ) -> Result<(Vec<PasskeyRow>, HashMap<String, IdentityRow>), IdentityError> {
        let only = match named.trim() {
            "" => None,
            named => Some(self.find(named)?),
        };
        let rows = self
            .db
            .passkeys(only.as_ref().map(|row| row.id.as_str()), include_removed)?;
        let identities = self
            .db
            .identities()?
            .into_iter()
            .map(|row| (row.id.clone(), row))
            .collect();
        Ok((rows, identities))
    }

    /// `StartPasskeyRegistration` through `channel`.
    pub fn start_registration(
        &self,
        channel: &Channel,
        named: &str,
    ) -> Result<Registration, IdentityError> {
        let web_session = web_only(channel, "StartPasskeyRegistration")?;
        let identity = self.person(named)?;
        let live = self.db.passkeys(Some(&identity.id), false)?;
        if live.len() >= MAX_PASSKEYS {
            return Err(IdentityError::TooManyPasskeys {
                identity: identity.name,
                limit: MAX_PASSKEYS,
            });
        }
        let mut challenge = [0_u8; 32];
        rand::rng().fill(&mut challenge);
        let now = Instant::now();
        let mut challenges = self.passkeys.challenges();
        challenges.retain(|_, started| started.expires > now);
        while challenges.len() >= MAX_CHALLENGES {
            let Some(oldest) = challenges
                .iter()
                .min_by_key(|(_, started)| started.expires)
                .map(|(challenge, _)| *challenge)
            else {
                break;
            };
            challenges.remove(&oldest);
        }
        challenges.insert(
            challenge,
            Started {
                identity_id: identity.id.clone(),
                web_session: web_session.to_owned(),
                expires: now + CHALLENGE_LIFETIME,
            },
        );
        Ok(Registration {
            challenge,
            expires_at: Timestamp(
                Timestamp::now()
                    .0
                    .saturating_add(crate::model::duration_millis(CHALLENGE_LIFETIME)),
            ),
            identity,
            live,
        })
    }

    /// The registration challenge `challenge`, taken: it must be this web
    /// UI session's and unexpired. Another session's is left in place.
    fn take_challenge(
        &self,
        challenge: &[u8],
        web_session: &str,
    ) -> Result<([u8; 32], String), IdentityError> {
        let stale = |message: &str| IdentityError::PasskeyInvalid {
            check: "challenge",
            message: message.to_owned(),
        };
        let challenge: [u8; 32] = challenge
            .try_into()
            .map_err(|_| stale("a registration challenge is 32 bytes"))?;
        let mut challenges = self.passkeys.challenges();
        let Entry::Occupied(entry) = challenges.entry(challenge) else {
            return Err(stale(
                "no registration was started with this challenge, or it was used already",
            ));
        };
        if entry.get().web_session != web_session {
            return Err(stale(
                "this registration was started by another web UI session",
            ));
        }
        let started = entry.remove();
        if started.expires <= Instant::now() {
            return Err(stale("the registration challenge expired"));
        }
        Ok((challenge, started.identity_id))
    }

    /// `RegisterPasskey` through `channel`, whose page is at `origin`.
    pub fn register_passkey(
        &self,
        channel: &Channel,
        origin: &str,
        request: &v1::RegisterPasskeyRequest,
    ) -> Result<PasskeyRow, IdentityError> {
        let web_session = web_only(channel, "RegisterPasskey")?;
        if let Some(message) = label_problem(&request.label) {
            return Err(IdentityError::InvalidPasskey {
                field: "label",
                message,
            });
        }
        let (challenge, identity_id) = self.take_challenge(&request.challenge, web_session)?;
        let registered = webauthn::check_registration(
            RP_ID,
            &challenge,
            &request.client_data_json,
            &request.authenticator_data,
        )
        .map_err(invalid)?;
        if registered.origin != origin {
            return Err(IdentityError::PasskeyInvalid {
                check: "origin",
                message: format!(
                    "the registration ran at {:?}, not the web UI's {origin:?}",
                    registered.origin
                ),
            });
        }
        let key = PasskeyKey::from_spki(&request.public_key, i64::from(request.algorithm))
            .map_err(|error| IdentityError::PasskeyAlgorithm {
                algorithm: i64::from(request.algorithm),
                message: error.message,
            })?;
        let identity = self.person(&identity_id)?;
        let _change = self.passkeys.change();
        let live = self.db.passkeys(Some(&identity.id), false)?;
        if !live.is_empty() {
            let Some(approval) = &request.approval else {
                return Err(IdentityError::PasskeyRequired {
                    identity: identity.name,
                    what: "adding another passkey",
                });
            };
            let change = webauthn::passkey_change_challenge(PasskeyChange::Add {
                registration_challenge: &challenge,
            });
            self.verify_assertion(&identity.id, approval, &change, origin)?;
        }
        let now = Timestamp::now();
        let row = PasskeyRow {
            id: self.unused_passkey_id()?,
            identity_id: identity.id.clone(),
            credential_id: registered.credential_id,
            public_key: key.spki().to_vec(),
            algorithm: key.algorithm().cose(),
            fingerprint: key.fingerprint().to_string(),
            rp_id: RP_ID.to_owned(),
            label: request.label.trim().to_owned(),
            aaguid: hex(&registered.aaguid),
            sign_count: registered.sign_count,
            created_at: now,
            created_web_session: Some(web_session.to_owned()),
            last_used_at: None,
            removed_at: None,
            removed_reason: None,
            removed_web_session: None,
        };
        match self.db.insert_passkey(&row, MAX_PASSKEYS)? {
            PasskeyInserted::Stored => {}
            PasskeyInserted::Full => {
                return Err(IdentityError::TooManyPasskeys {
                    identity: identity.name,
                    limit: MAX_PASSKEYS,
                });
            }
            PasskeyInserted::Exists(id) => return Err(IdentityError::PasskeyExists { id }),
        }
        audit::passkey_changed("add", &row, Some(web_session));
        Ok(row)
    }

    /// `RemovePasskey` through `channel`, whose page is at `origin`.
    pub fn remove_passkey(
        &self,
        channel: &Channel,
        origin: &str,
        request: &v1::RemovePasskeyRequest,
    ) -> Result<PasskeyRow, IdentityError> {
        let web_session = web_only(channel, "RemovePasskey")?;
        let id = request.id.trim();
        let not_found = || IdentityError::PasskeyNotFound { id: id.to_owned() };
        let _change = self.passkeys.change();
        let row = self
            .db
            .passkey(id)?
            .filter(|row| row.removed_at.is_none())
            .ok_or_else(not_found)?;
        let identity = self.person(&row.identity_id)?;
        let Some(assertion) = &request.assertion else {
            return Err(IdentityError::PasskeyRequired {
                identity: identity.name,
                what: "removing a passkey",
            });
        };
        let change = webauthn::passkey_change_challenge(PasskeyChange::Remove { passkey: &row.id });
        self.verify_assertion(&identity.id, assertion, &change, origin)?;
        let reason = match request.reason.trim() {
            "" => "removed".to_owned(),
            reason => format!("removed: {reason}"),
        };
        let removed = self
            .db
            .remove_passkey(&row.id, Timestamp::now(), &reason, Some(web_session))?
            .ok_or_else(not_found)?;
        audit::passkey_changed("remove", &removed, Some(web_session));
        Ok(removed)
    }

    /// `ResetPasskeys`: every live passkey of `named` removed, and every
    /// unexpired key they certified revoked from now. The removed passkeys,
    /// and the keys revoked.
    pub async fn reset_passkeys(
        &self,
        named: &str,
        reason: &str,
    ) -> Result<(Vec<PasskeyRow>, Vec<String>), IdentityError> {
        if named.trim().is_empty() {
            return Err(IdentityError::InvalidPasskey {
                field: "identity",
                message: "name the identity whose passkeys to reset".to_owned(),
            });
        }
        let identity = self.find(named)?;
        let reason = match reason.trim() {
            "" => "reset".to_owned(),
            reason => format!("reset: {reason}"),
        };
        let (removed, keys) = {
            let _change = self.passkeys.change();
            self.db
                .reset_passkeys(&identity.id, Timestamp::now(), &reason)?
        };
        for row in &removed {
            audit::passkey_changed("reset", row, None);
        }
        for key_id in &keys {
            self.revoke_key(key_id, None, false, &reason).await?;
        }
        tracing::warn!(
            target: "reef_daemon::identity::audit",
            identity_id = %identity.id,
            identity = %identity.name,
            removed_count = removed.len(),
            revoked_key_count = keys.len(),
            "passkeys reset"
        );
        Ok((removed, keys))
    }

    /// Checks `assertion` over `challenge` as a ceremony of the web UI at
    /// `origin`: by a live passkey of `identity_id` (`passkey`), passing
    /// `reef_evidence`'s checks (`type`, `challenge`, `origin`, `rp`,
    /// `flags`, `signature`), from exactly `origin`, with a sign count above
    /// the passkey's last unless both are 0 (`counter`), which it records
    /// with the time of use. The passkey as it is after, its key, and the
    /// assertion.
    pub(crate) fn verify_assertion(
        &self,
        identity_id: &str,
        assertion: &v1::PasskeyAssertion,
        challenge: &[u8; 32],
        origin: &str,
    ) -> Result<(PasskeyRow, PasskeyKey, Assertion), IdentityError> {
        let unknown = || IdentityError::PasskeyInvalid {
            check: "passkey",
            message: "the assertion's credential is no live passkey of the identity".to_owned(),
        };
        let row = self
            .db
            .passkeys(Some(identity_id), false)?
            .into_iter()
            .find(|row| row.credential_id == assertion.credential_id)
            .ok_or_else(unknown)?;
        let key = PasskeyKey::from_spki(&row.public_key, row.algorithm).map_err(invalid)?;
        let bytes = assertion_of(assertion);
        let asserted =
            webauthn::check_assertion(&key, &row.rp_id, challenge, &bytes).map_err(invalid)?;
        if asserted.origin != origin {
            return Err(IdentityError::PasskeyInvalid {
                check: "origin",
                message: format!(
                    "the assertion ran at {:?}, not the web UI's {origin:?}",
                    asserted.origin
                ),
            });
        }
        let used = self
            .db
            .use_passkey(&row.id, asserted.sign_count, Timestamp::now())?
            .ok_or_else(|| IdentityError::PasskeyInvalid {
                check: "counter",
                message: format!(
                    "the sign count {} is not above the passkey's last, {}: a cloned authenticator?",
                    asserted.sign_count, row.sign_count
                ),
            })?;
        Ok((used, key, bytes))
    }

    fn unused_passkey_id(&self) -> Result<String, IdentityError> {
        loop {
            let id = new_id("pk_");
            if !self.db.passkey_id_taken(&id)? {
                return Ok(id);
            }
        }
    }
}
