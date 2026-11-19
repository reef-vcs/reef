//! Approvals (R3.43, ADR 0085): a person's verified approval of one
//! sign-off, the only way a key gets presence `user_verified`.
//!
//! A requester (the CLI on the socket, or a page of the web UI) sends a
//! fresh session key with its proof, the in-toto statement it will sign,
//! and the revision; the caller has parsed the statement and checked its
//! subject is the revision's tree. The approval binds them, and a session
//! chosen now, into a `WebAuthn` challenge
//! ([`reef_evidence::webauthn::approval_challenge`]); a passkey's
//! assertion over it, on the web UI only, approves it, and the key is then
//! certified for 2 minutes with the assertion in its certificate
//! (`Identities::certify_key`). A decline or the expiry ends it.
//!
//! Approvals live in memory, at most [`MAX_PENDING`] pending per identity
//! and [`MAX_APPROVALS`] in all; a restart drops them. An ended one stays
//! readable for [`KEEP_ENDED`] after its expiry. Every state change is
//! sent on the approval's watch channel, which `GetApproval`'s wait reads:
//! no polling. An expiry is noticed when the approval is next read, and
//! logged then.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use reef_api::v1;
use reef_evidence::session::{Actor, PresenceProof};
use reef_evidence::webauthn::{self, ApprovalFor, Approves, PresenceEvidence};
use reef_evidence::{Index, KeyId, PublicKey};
use tokio::sync::watch;

use super::issue::{Channel, Issuance, SessionFor};
use super::passkeys::RP_ID;
use super::{Identities, IdentityError, audit, new_id};
use crate::db::IdentityRow;
use crate::model::Timestamp;

/// How long an approval waits for a passkey, and how long the key it
/// certifies lives.
pub const APPROVAL_LIFETIME: Duration = Duration::from_mins(2);

/// How long an ended approval stays readable after its expiry.
pub const KEEP_ENDED: Duration = Duration::from_mins(2);

/// The most pending approvals of one identity.
pub const MAX_PENDING: usize = 8;

/// The most approvals held, ended ones included.
pub const MAX_APPROVALS: usize = 256;

/// Who asked for an approval.
#[derive(Clone, Debug)]
pub struct Requester {
    /// The socket, or a web UI session.
    pub channel: Channel,
    /// Its `reef-agent`.
    pub agent: Option<String>,
    /// The workspace it named.
    pub workspace: Option<String>,
}

/// What the requester's revision resolved to.
#[derive(Clone, Debug)]
pub struct Resolved {
    /// The repository's id.
    pub repo_id: String,
    /// The commit.
    pub commit_id: String,
    /// Its change, 32 lowercase hex digits.
    pub change_id: String,
}

/// Where an approval stands.
#[derive(Clone, Debug)]
pub enum State {
    /// Waiting for a passkey; `resolving` while an approve is being
    /// checked, which a second one waits out as refused.
    Pending {
        /// An approve is in flight.
        resolving: bool,
    },
    /// Approved: the key is certified.
    Approved {
        /// When.
        at: Timestamp,
        /// The key, as issued, with its certificate.
        key: Box<v1::SessionKey>,
        /// The web UI session that approved it.
        web_session: String,
    },
    /// Declined.
    Declined {
        /// When.
        at: Timestamp,
        /// Why, as the decliner said.
        reason: String,
        /// The web UI session that declined it; `None` on the socket.
        web_session: Option<String>,
    },
    /// Not approved by its expiry.
    Expired,
}

impl State {
    /// Declines a pending approval. Refused while an approve is in flight:
    /// its outcome would replace the decline, approving what was declined
    /// or making it pending again, so the decliner tries again after it.
    fn decline(
        &mut self,
        at: Timestamp,
        reason: &str,
        web_session: Option<&str>,
    ) -> Result<(), ApprovalEnd> {
        match self {
            State::Pending { resolving: false } => {
                *self = State::Declined {
                    at,
                    reason: reason.trim().to_owned(),
                    web_session: web_session.map(str::to_owned),
                };
                Ok(())
            }
            State::Pending { resolving: true } | State::Approved { .. } => {
                Err(ApprovalEnd::Approved)
            }
            State::Declined { .. } => Err(ApprovalEnd::Declined),
            State::Expired => Err(ApprovalEnd::Expired),
        }
    }

    /// An approve in flight was refused: pending again.
    fn approve_failed(&mut self) {
        if matches!(self, State::Pending { resolving: true }) {
            *self = State::Pending { resolving: false };
        }
    }
}

/// A request for a person's verified approval of one sign-off.
#[derive(Clone, Debug)]
pub struct Approval {
    /// `apr_` and 16 hex digits.
    pub id: String,
    /// The person identity it signs as.
    pub identity: IdentityRow,
    /// The authority key id current at the request, which the challenge
    /// names.
    pub authority: String,
    /// The session key.
    pub public_key: PublicKey,
    /// The session it will be certified in.
    pub session_id: String,
    /// What the touch approves.
    pub approves: Approves,
    /// The statement's bytes.
    pub statement: Vec<u8>,
    /// The statement's index: kind, result, target, tree.
    pub index: Index,
    /// The revision it is for.
    pub resolved: Resolved,
    /// Who asked.
    pub requester: Requester,
    /// When.
    pub created_at: Timestamp,
    /// When it ends unless approved.
    pub expires_at: Timestamp,
    /// Where it stands.
    pub state: State,
}

impl Approval {
    /// The `WebAuthn` challenge a passkey signs to approve it.
    pub fn challenge(&self) -> [u8; 32] {
        let authority = KeyId::parse(&self.authority).unwrap_or_else(|| KeyId::of(&[]));
        webauthn::approval_challenge(&ApprovalFor {
            authority: &authority,
            key: self.public_key.id(),
            identity: &self.identity.id,
            session: &self.session_id,
            approves: &self.approves,
        })
    }

    fn is_pending(&self) -> bool {
        matches!(self.state, State::Pending { .. })
    }

    /// Marks it expired when pending past its expiry; whether it did. An
    /// approve in flight began before the expiry (it checks that first), so
    /// it is left to finish: approved, or pending and then expired.
    fn expire(&mut self, now: Timestamp) -> bool {
        if matches!(self.state, State::Pending { resolving: false }) && now >= self.expires_at {
            self.state = State::Expired;
            return true;
        }
        false
    }

    /// The refusal of a resolution of it, unless it is pending.
    fn not_pending(&self) -> Option<IdentityError> {
        let id = self.id.clone();
        match self.state {
            State::Pending { .. } => None,
            State::Expired => Some(IdentityError::ApprovalEnded {
                id,
                end: ApprovalEnd::Expired,
            }),
            State::Declined { .. } => Some(IdentityError::ApprovalEnded {
                id,
                end: ApprovalEnd::Declined,
            }),
            State::Approved { .. } => Some(IdentityError::ApprovalEnded {
                id,
                end: ApprovalEnd::Approved,
            }),
        }
    }
}

/// How an approval ended, for the refusal of a resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalEnd {
    /// `APPROVAL_EXPIRED`.
    Expired,
    /// `APPROVAL_DECLINED`.
    Declined,
    /// `APPROVAL_RESOLVED`.
    Approved,
}

/// What a request asks to approve, its statement and revision checked.
#[derive(Debug)]
pub struct ApprovalRequest {
    /// The identity named; empty for the default.
    pub identity: String,
    /// The statement's bytes.
    pub statement: Vec<u8>,
    /// Its index.
    pub index: Index,
    /// The revision it is for.
    pub resolved: Resolved,
    /// The session key's DER.
    pub public_key: Vec<u8>,
    /// Its proof of possession.
    pub proof: Vec<u8>,
}

/// One held approval and its watch channel.
#[derive(Debug)]
struct Held {
    approval: Approval,
    changed: watch::Sender<()>,
}

/// An approve in flight on approval `id`. Expiry leaves it alone, so if
/// its future is dropped before the outcome is recorded (the client went
/// away mid-call), dropping this makes the approval pending again, and so
/// able to expire.
struct Resolving<'a> {
    approvals: &'a Approvals,
    id: &'a str,
    ended: bool,
}

impl Drop for Resolving<'_> {
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        if let Some(entry) = self.approvals.lock().get_mut(self.id) {
            entry.approval.state.approve_failed();
            entry.changed.send_replace(());
        }
    }
}

/// The approvals `Identities` holds.
#[derive(Debug)]
pub(crate) struct Approvals {
    held: Mutex<HashMap<String, Held>>,
    /// How long a new approval waits, in milliseconds:
    /// [`APPROVAL_LIFETIME`], shortened by tests of the expiry.
    waits_ms: AtomicU64,
}

impl Default for Approvals {
    fn default() -> Approvals {
        Approvals {
            held: Mutex::new(HashMap::new()),
            waits_ms: AtomicU64::new(
                u64::try_from(APPROVAL_LIFETIME.as_millis()).unwrap_or(u64::MAX),
            ),
        }
    }
}

impl Approvals {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Held>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// How a resolution resolves.
#[derive(Debug)]
pub enum Resolution {
    /// With this assertion.
    Approve(v1::PasskeyAssertion),
    /// With this reason.
    Decline(String),
}

impl Identities {
    /// Shortens how long new approvals wait, for the tests of the expiry;
    /// the keys they certify still live [`APPROVAL_LIFETIME`].
    #[doc(hidden)]
    pub fn set_approval_wait(&self, wait: Duration) {
        self.approvals.waits_ms.store(
            u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// `RequestApproval` through `channel`.
    pub async fn request_approval(
        &self,
        request: ApprovalRequest,
        requester: Requester,
    ) -> Result<Approval, IdentityError> {
        if matches!(requester.channel, Channel::Remote { .. }) {
            return Err(IdentityError::PasskeyOverRemote {
                method: "RequestApproval",
            });
        }
        let identity = self.person(&request.identity)?;
        let (public_key, authority) = self
            .proven_key(
                request.public_key,
                request.proof,
                request.identity,
                Actor::Human,
                String::new(),
            )
            .await?;
        self.check_fresh(public_key.id().as_str())?;
        let now = Timestamp::now();
        let approval = Approval {
            id: new_id("apr_"),
            identity,
            authority,
            public_key,
            session_id: self.unused_session_id()?,
            approves: Approves {
                statement: reef_evidence::sha256_hex(&request.statement),
                change: request.resolved.change_id.clone(),
            },
            statement: request.statement,
            index: request.index,
            resolved: request.resolved,
            requester,
            created_at: now,
            expires_at: Timestamp(now.0.saturating_add(
                i64::try_from(self.approvals.waits_ms.load(Ordering::Relaxed)).unwrap_or(i64::MAX),
            )),
            state: State::Pending { resolving: false },
        };
        let mut held = self.approvals.lock();
        let keep = crate::model::duration_millis(KEEP_ENDED);
        held.retain(|_, entry| {
            if entry.approval.expire(now) {
                audit::approval("expired", &entry.approval, None);
            }
            entry.approval.is_pending() || now.0 < entry.approval.expires_at.0.saturating_add(keep)
        });
        let pending = held
            .values()
            .filter(|entry| {
                entry.approval.is_pending() && entry.approval.identity.id == approval.identity.id
            })
            .count();
        if pending >= MAX_PENDING || held.len() >= MAX_APPROVALS {
            return Err(IdentityError::TooManyApprovals {
                limit: if pending >= MAX_PENDING {
                    MAX_PENDING
                } else {
                    MAX_APPROVALS
                },
            });
        }
        audit::approval(
            "requested",
            &approval,
            approval.requester.channel.web_session(),
        );
        let (changed, _) = watch::channel(());
        held.insert(
            approval.id.clone(),
            Held {
                approval: approval.clone(),
                changed,
            },
        );
        Ok(approval)
    }

    /// The approval `id` now, marked expired if it is; and a receiver of
    /// its changes.
    fn approval_now(&self, id: &str) -> Result<(Approval, watch::Receiver<()>), IdentityError> {
        let mut held = self.approvals.lock();
        let entry = held
            .get_mut(id.trim())
            .ok_or_else(|| IdentityError::ApprovalNotFound { id: id.to_owned() })?;
        if entry.approval.expire(Timestamp::now()) {
            audit::approval("expired", &entry.approval, None);
            entry.changed.send_replace(());
        }
        Ok((entry.approval.clone(), entry.changed.subscribe()))
    }

    /// `GetApproval`: approval `id`, once it is not pending or `wait` (at
    /// most until its expiry) has passed.
    pub async fn get_approval(&self, id: &str, wait: Duration) -> Result<Approval, IdentityError> {
        let (approval, mut changed) = self.approval_now(id)?;
        if !approval.is_pending() || wait.is_zero() {
            return Ok(approval);
        }
        let until_expiry = u64::try_from(approval.expires_at.0 - Timestamp::now().0)
            .map_or(Duration::ZERO, Duration::from_millis);
        // An approve in flight may finish past the expiry, so its outcome,
        // not the expiry, ends the wait; a client would otherwise poll
        // again at once until it does.
        let resolving = matches!(approval.state, State::Pending { resolving: true });
        let deadline = tokio::time::Instant::now()
            + if resolving {
                wait
            } else {
                wait.min(until_expiry)
            };
        loop {
            match tokio::time::timeout_at(deadline, changed.changed()).await {
                Ok(Ok(())) => {
                    let (approval, _) = self.approval_now(id)?;
                    if !approval.is_pending() {
                        return Ok(approval);
                    }
                }
                Ok(Err(_)) | Err(_) => return Ok(self.approval_now(id)?.0),
            }
        }
    }

    /// `ResolveApproval` through `channel`, whose page is at `origin` when
    /// it is the web UI.
    pub async fn resolve_approval(
        &self,
        channel: &Channel,
        origin: Option<&str>,
        id: &str,
        resolution: Resolution,
    ) -> Result<Approval, IdentityError> {
        let id = id.trim();
        match resolution {
            Resolution::Decline(reason) => self.decline(channel, id, &reason),
            Resolution::Approve(assertion) => {
                let web_session = match channel {
                    Channel::Web { session } => session.clone(),
                    Channel::Socket => {
                        return Err(IdentityError::ApprovalNeedsWeb { id: id.to_owned() });
                    }
                    Channel::Remote { .. } => {
                        return Err(IdentityError::PasskeyOverRemote {
                            method: "ResolveApproval",
                        });
                    }
                };
                let approval = self.begin_approve(id)?;
                let mut resolving = Resolving {
                    approvals: &self.approvals,
                    id,
                    ended: false,
                };
                let approved = self
                    .approve(&approval, &assertion, origin.unwrap_or_default(), channel)
                    .await;
                resolving.ended = true;
                self.end_approve(&approval, approved, &web_session)
            }
        }
    }

    fn decline(
        &self,
        channel: &Channel,
        id: &str,
        reason: &str,
    ) -> Result<Approval, IdentityError> {
        if matches!(channel, Channel::Remote { .. }) {
            return Err(IdentityError::PasskeyOverRemote {
                method: "ResolveApproval",
            });
        }
        let mut held = self.approvals.lock();
        let entry = held
            .get_mut(id)
            .ok_or_else(|| IdentityError::ApprovalNotFound { id: id.to_owned() })?;
        let now = Timestamp::now();
        if entry.approval.expire(now) {
            audit::approval("expired", &entry.approval, None);
            entry.changed.send_replace(());
        }
        if let Some(refusal) = entry.approval.not_pending() {
            return Err(refusal);
        }
        entry
            .approval
            .state
            .decline(now, reason, channel.web_session())
            .map_err(|end| IdentityError::ApprovalEnded {
                id: id.to_owned(),
                end,
            })?;
        audit::approval("declined", &entry.approval, channel.web_session());
        entry.changed.send_replace(());
        Ok(entry.approval.clone())
    }

    /// Marks pending approval `id` as being approved, so a second approve
    /// is refused while this one runs; the approval.
    fn begin_approve(&self, id: &str) -> Result<Approval, IdentityError> {
        let mut held = self.approvals.lock();
        let entry = held
            .get_mut(id)
            .ok_or_else(|| IdentityError::ApprovalNotFound { id: id.to_owned() })?;
        if entry.approval.expire(Timestamp::now()) {
            audit::approval("expired", &entry.approval, None);
            entry.changed.send_replace(());
        }
        if let Some(refusal) = entry.approval.not_pending() {
            return Err(refusal);
        }
        if matches!(entry.approval.state, State::Pending { resolving: true }) {
            return Err(IdentityError::ApprovalEnded {
                id: id.to_owned(),
                end: ApprovalEnd::Approved,
            });
        }
        entry.approval.state = State::Pending { resolving: true };
        Ok(entry.approval.clone())
    }

    /// Records the outcome of an approve begun by [`Self::begin_approve`]:
    /// approved, or pending again after a refusal.
    fn end_approve(
        &self,
        approval: &Approval,
        approved: Result<v1::SessionKey, IdentityError>,
        web_session: &str,
    ) -> Result<Approval, IdentityError> {
        let mut held = self.approvals.lock();
        let Some(entry) = held.get_mut(&approval.id) else {
            return approved.map(|_| approval.clone());
        };
        match approved {
            Err(error) => {
                entry.approval.state.approve_failed();
                entry.changed.send_replace(());
                Err(error)
            }
            Ok(key) => {
                entry.approval.state = State::Approved {
                    at: Timestamp::now(),
                    key: Box::new(key),
                    web_session: web_session.to_owned(),
                };
                audit::approval("approved", &entry.approval, Some(web_session));
                entry.changed.send_replace(());
                Ok(entry.approval.clone())
            }
        }
    }

    /// The checks and the issuance of an approve: the identity still live,
    /// the authority not rotated since the request, the assertion by a
    /// live passkey of the identity over the approval's challenge from the
    /// web UI at `origin`, then the key certified for
    /// [`APPROVAL_LIFETIME`] with the assertion as its presence evidence.
    async fn approve(
        &self,
        approval: &Approval,
        assertion: &v1::PasskeyAssertion,
        origin: &str,
        channel: &Channel,
    ) -> Result<v1::SessionKey, IdentityError> {
        if Timestamp::now() >= approval.expires_at {
            return Err(IdentityError::ApprovalEnded {
                id: approval.id.clone(),
                end: ApprovalEnd::Expired,
            });
        }
        let identity = self.person(&approval.identity.id)?;
        if self.current_authority().id != approval.authority {
            return Err(IdentityError::PasskeyInvalid {
                check: "challenge",
                message: "the authority rotated since the approval was requested; request it again"
                    .to_owned(),
            });
        }
        let (passkey, key, bytes) =
            self.verify_assertion(&identity.id, assertion, &approval.challenge(), origin)?;
        let evidence = PresenceEvidence {
            passkey: key,
            credential_id: passkey.credential_id.clone(),
            rp_id: RP_ID.to_owned(),
            approves: approval.approves.clone(),
            assertion: bytes,
        };
        let read = self
            .certify_key(Issuance {
                public_key: approval.public_key.clone(),
                identity,
                actor: Actor::Human,
                agent: approval.requester.agent.clone(),
                session: SessionFor::New(approval.session_id.clone()),
                presence: PresenceProof::UserVerified(Box::new(evidence)),
                lifetime: APPROVAL_LIFETIME,
                channel: channel.clone(),
                audience: approval.authority.clone(),
                passkey_id: Some(passkey.id.clone()),
                approval_id: Some(approval.id.clone()),
            })
            .await?;
        Ok(super::read::session_key_message(&read, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decline_waits_out_an_approve_in_flight() {
        let at = Timestamp(0);
        let mut state = State::Pending { resolving: true };
        assert!(matches!(
            state.decline(at, "no", None),
            Err(ApprovalEnd::Approved)
        ));
        state.approve_failed();
        assert!(matches!(state, State::Pending { resolving: false }));
        state.decline(at, " not mine ", Some("web_a")).unwrap();
        assert!(matches!(
            &state,
            State::Declined { reason, web_session: Some(session), .. }
                if reason == "not mine" && session == "web_a"
        ));
        assert!(matches!(
            state.decline(at, "", None),
            Err(ApprovalEnd::Declined)
        ));
    }

    #[test]
    fn a_failed_approve_leaves_an_expiry() {
        let mut state = State::Expired;
        state.approve_failed();
        assert!(matches!(state, State::Expired));
    }
}
