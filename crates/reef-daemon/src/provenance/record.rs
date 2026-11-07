//! Recording: one signed record per commit the daemon records for a
//! workspace, which never fails the snapshot or the write that recorded
//! the commit.
//!
//! Under the workspace's lock, [`Provenance::record`]: finds the live
//! session (starting one for a workspace that has none); renews the held
//! key when there is none, it is for another session, or it is within
//! [`super::RENEW_BEFORE`] of its end, starting a new session when R3.03
//! revoked this one; builds the record from the session and the commit;
//! signs it; and stores it with its key in one transaction, which moves
//! the session's sequence past it. Any failure is logged at `warn` and
//! counted in the session's `missing_records`.

use std::sync::PoisonError;

use reef_evidence::provenance::{Context, Record, context_digest, sign};
use reef_evidence::session::Actor;

use super::session::{For, IDENTITY_REVOKED, TOKEN_REVOKED};
use super::{FailAt, Held, Provenance, ProvenanceError, RENEW_BEFORE, audit};
use crate::db::{EvidenceKeyRow, ProvenanceRow, ProvenanceSessionRow};
use crate::identity::IdentityError;
use crate::identity::issue::{HeldKey, Refusal};
use crate::model::Timestamp;

/// A commit to record.
#[derive(Clone, Copy, Debug)]
pub struct Commit<'a> {
    /// The workspace it was recorded for.
    pub at: For<'a>,
    /// Its id.
    pub commit_id: &'a str,
    /// Its change.
    pub change_id: &'a str,
    /// Its tree, unless conflicted.
    pub tree_id: Option<&'a str>,
    /// What recorded it: a snapshot kind, or `edit`.
    pub kind: &'a str,
    /// When the operation that wrote it ended. The record says when the
    /// daemon signed it, which is no earlier.
    pub recorded_at: Timestamp,
}

/// Whether a held key can sign at `now` for `session`.
fn usable(held: &Held, session: &str, now: Timestamp) -> bool {
    held.session_id == session
        && held.key.as_ref().is_some_and(|key| {
            key.not_after.is_none_or(|end| {
                now.0 < end.0 - i64::try_from(RENEW_BEFORE.as_millis()).unwrap_or(0)
            })
        })
}

impl Provenance {
    /// Records `commit`: never fails; see the module documentation.
    /// Blocking.
    pub fn record(&self, commit: &Commit<'_>) {
        if !self.enabled() {
            return;
        }
        let held = self.held(commit.at.workspace_id);
        let mut held = held.lock().unwrap_or_else(PoisonError::into_inner);
        let mut session_id = None;
        let recorded = self.record_locked(&mut held, commit, &mut session_id);
        if let Err(ProvenanceError::Ended(_)) = &recorded {
            tracing::debug!(
                target: "reef_daemon::provenance",
                workspace_id = %commit.at.workspace_id,
                commit_id = commit.commit_id,
                "the workspace ended before its commit was recorded; no record"
            );
            return;
        }
        if let Err(error) = recorded {
            tracing::warn!(
                target: "reef_daemon::provenance",
                workspace_id = %commit.at.workspace_id,
                commit_id = commit.commit_id,
                kind = commit.kind,
                error = %reef_lib::error::ErrorChain(&error),
                "could not record provenance for a commit; the commit stands without a record"
            );
            if let Some(session_id) = session_id
                && let Err(error) = self.db.count_missing_record(&session_id)
            {
                tracing::warn!(
                    session_id,
                    error = %reef_lib::error::ErrorChain(&error),
                    "could not count a missing provenance record"
                );
            }
        }
    }

    fn record_locked(
        &self,
        held: &mut Held,
        commit: &Commit<'_>,
        session_id: &mut Option<String>,
    ) -> Result<(), ProvenanceError> {
        let mut session = self.live_or_start(held, commit.at)?;
        *session_id = Some(session.session_id.clone());
        let now = Timestamp::now();
        if !usable(held, &session.session_id, now) {
            if self.fails_at(FailAt::Issue) {
                return Err(ProvenanceError::Task("test hook: issuance fails".into()));
            }
            session = self.renew(held, session, commit)?;
            *session_id = Some(session.session_id.clone());
        }
        let key = held
            .key
            .as_ref()
            .ok_or_else(|| ProvenanceError::Task("no held key after renewal".into()))?;
        // The witness time is the signature's: a key issued for this very
        // record has a window that starts after the operation ended.
        let recorded_at = Timestamp(commit.recorded_at.0.max(Timestamp::now().0));
        let record = Record {
            commit_id: commit.commit_id.to_owned(),
            change_id: commit.change_id.to_owned(),
            tree_id: commit.tree_id.map(str::to_owned),
            workspace: commit.at.workspace_id.as_hex(),
            kind: commit.kind.to_owned(),
            session: session.session_id.clone(),
            sequence: u64::try_from(session.next_sequence).unwrap_or(1),
            agent: session.agent.clone(),
            recorded_on_ms: recorded_at.0,
            context: Context {
                model: session.declared.model.clone(),
                harness: session.declared.harness.clone(),
                task: session.declared.task.clone(),
                instruction_sha256: session.declared.instruction_sha256.clone(),
                client_session: session.declared.client_session.clone(),
                declared_on_ms: session.declared_at.map(|at| at.0),
            },
        };
        if self.fails_at(FailAt::Sign) {
            return Err(ProvenanceError::Task("test hook: signing fails".into()));
        }
        let envelope = sign(&key.key, &record)
            .map_err(|invalid| ProvenanceError::Task(format!("signing refused: {invalid}")))?
            .to_bytes();
        if self.fails_at(FailAt::Insert) {
            return Err(ProvenanceError::Task("test hook: the insert fails".into()));
        }
        let key_row = EvidenceKeyRow {
            id: key.key_id.clone(),
            algorithm: key.key.public_key().algorithm().to_owned(),
            public_key: key.key.public_key().spki().to_vec(),
            first_seen_at: now,
        };
        let row = ProvenanceRow {
            repo_id: commit.at.repo_id,
            id: reef_evidence::sha256_hex(&envelope),
            envelope,
            format: 1,
            commit_id: record.commit_id.clone(),
            change_id: record.change_id.clone(),
            tree_id: record.tree_id.clone(),
            workspace_id: record.workspace.clone(),
            session_id: record.session.clone(),
            sequence: session.next_sequence,
            kind: record.kind.clone(),
            key_id: key.key_id.clone(),
            agent: record.agent.clone(),
            declared: session.declared.clone(),
            context_digest: context_digest(&record.context),
            recorded_at,
        };
        if !self.db.insert_provenance(&key_row, &row)? {
            tracing::info!(
                target: "reef_daemon::provenance",
                commit_id = commit.commit_id,
                "the commit already has a provenance record; kept the first"
            );
        }
        Ok(())
    }

    /// A new held key for `session`, or for a new session when R3.03
    /// revoked it; an uncertified one, with the problem recorded, when no
    /// identity can be bound. Returns the session the key is for.
    fn renew(
        &self,
        held: &mut Held,
        mut session: ProvenanceSessionRow,
        commit: &Commit<'_>,
    ) -> Result<ProvenanceSessionRow, ProvenanceError> {
        held.key = None;
        let actor = Actor::parse(&session.actor).unwrap_or(Actor::Agent);
        let identity = match &session.identity_id {
            Some(id) => self.db.identity(id)?,
            None => None,
        };
        let problem = match (&identity, &session.problem) {
            (None, problem) => Some(
                problem
                    .clone()
                    .unwrap_or_else(|| super::session::NO_DEFAULT_IDENTITY.to_owned()),
            ),
            (Some(identity), _) if identity.revocation.is_some() => {
                Some(IDENTITY_REVOKED.to_owned())
            }
            (Some(_), _)
                if session.channel != "socket" && !self.token_live(&session.channel)? =>
            {
                Some(TOKEN_REVOKED.to_owned())
            }
            (Some(_), _) => None,
        };
        let Some(identity) = identity.filter(|_| problem.is_none()) else {
            let problem = problem.unwrap_or_default();
            if session.problem.as_deref() != Some(problem.as_str()) {
                self.db
                    .set_provenance_problem(&session.session_id, Some(&problem))?;
                session.problem = Some(problem.clone());
                audit::unbound(&session, &problem);
            }
            held.key = Some(HeldKey::generate()?);
            held.session_id.clone_from(&session.session_id);
            return Ok(session);
        };
        let lifetime = self.key_lifetime();
        let issued = self.identities.issue_held(
            &identity,
            actor,
            session.agent.as_deref(),
            &session.session_id,
            &session.channel,
            lifetime,
        );
        let key = match issued {
            Ok(key) => key,
            Err(IdentityError::Refused {
                rule: Refusal::SessionRevoked,
            }) => {
                // R3.03 revoked the session: a new one, same binding.
                let now = Timestamp::now();
                let mut next = session.clone();
                next.session_id = self.unused_session_id()?;
                next.next_sequence = 1;
                next.missing_records = 0;
                next.started_at = now;
                next.ended_at = None;
                self.db
                    .restart_provenance_session(&session.session_id, now, &next)?;
                audit::restarted(&session.session_id, &next, commit.at.workspace_id);
                session = next;
                self.identities.issue_held(
                    &identity,
                    actor,
                    session.agent.as_deref(),
                    &session.session_id,
                    &session.channel,
                    lifetime,
                )?
            }
            Err(error) => return Err(error.into()),
        };
        if session.problem.is_some() {
            self.db.set_provenance_problem(&session.session_id, None)?;
            session.problem = None;
        }
        held.key = Some(key);
        held.session_id.clone_from(&session.session_id);
        Ok(session)
    }

    /// Whether remote token `id` is neither revoked nor expired.
    fn token_live(&self, id: &str) -> Result<bool, ProvenanceError> {
        let now = Timestamp::now();
        Ok(self
            .db
            .remote_tokens()?
            .iter()
            .any(|row| row.id == id && row.is_live(now)))
    }
}
