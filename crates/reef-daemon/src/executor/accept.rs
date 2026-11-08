//! `UpdateJob` and `CompleteJob`: a claim's heartbeat, progress, evidence
//! and outcome (R3.05).
//!
//! The service verifies a call's envelopes first, on the blocking pool,
//! with R3.01's [`reef_evidence::Verified::check`], and [`bind`] checks
//! each against the job's tree and kinds. Only then does [`super::Executor::accept`]
//! take one transaction: it reads the job again, applies any lapse that
//! came due, checks the claim, checks completeness for `DONE`, and stores
//! every object through the insert `AddEvidence` uses
//! ([`crate::db::add_evidence_in`]) with one `job_evidence` row each, and
//! the job's update. One refused object stores nothing.

use std::collections::BTreeSet;

use reef_evidence::Verified;

use super::{
    ExecutorError, FRESH_SLACK, MISSING_SHOWN, apply_due, check_call, check_owner, digest_matches,
    end_attempt, end_attempt_time, millis,
};
use crate::db::{
    DbError, JobEvidenceRow, JobRow, JobState, add_evidence_in, insert_job_evidence_in,
    job_evidence_in, job_in, next_job_evidence_seq_in, update_job_in,
};
use crate::model::Timestamp;

/// What an `UpdateJob` changes besides its evidence.
#[derive(Clone, Debug, Default)]
pub struct Update {
    /// The attempt's progress, as JSON; `None` keeps the last.
    pub progress: Option<String>,
    /// Give the claim back after storing the evidence.
    pub release: bool,
}

/// What a `CompleteJob` records.
#[derive(Clone, Debug)]
pub struct Completion {
    /// `done`, `budget_exhausted` or `failed`.
    pub outcome: &'static str,
    /// Reported wall time.
    pub usage_wall_ms: Option<i64>,
    /// Reported CPU time.
    pub usage_cpu_ms: Option<i64>,
    /// The executor's message.
    pub message: Option<String>,
}

/// The call: an update or a completion.
#[derive(Clone, Debug)]
pub enum Call {
    /// `UpdateJob`.
    Update(Update),
    /// `CompleteJob`.
    Complete(Completion),
}

/// What a call stored.
#[derive(Clone, Debug)]
pub struct Accepted {
    /// The job after the call.
    pub job: JobRow,
    /// The rows of the evidence this call accepted, in the call's order.
    pub evidence: Vec<JobEvidenceRow>,
}

/// One verified object of a call, with who sent it.
#[derive(Clone, Debug)]
pub struct Incoming {
    /// The object.
    pub verified: Verified,
    /// The request's `reef-agent`.
    pub agent: Option<String>,
    /// The remote token it came through.
    pub remote_token_id: Option<String>,
}

/// Checks that each of `verified` is about `job`'s tree and of one of its
/// kinds: `JOB_EVIDENCE_MISMATCH` with the first that is not.
pub fn bind(job: &JobRow, verified: &[Verified]) -> Result<(), ExecutorError> {
    for (index, object) in verified.iter().enumerate() {
        if object.index.tree_id != job.tree_id {
            return Err(ExecutorError::Mismatch {
                index,
                field: "tree",
                evidence_id: object.id.clone(),
                actual: object.index.tree_id.clone(),
                expected: job.tree_id.clone(),
            });
        }
        if !job.kinds.contains(&object.index.kind) {
            return Err(ExecutorError::Mismatch {
                index,
                field: "kind",
                evidence_id: object.id.clone(),
                actual: object.index.kind.clone(),
                expected: job.kinds.join(", "),
            });
        }
    }
    Ok(())
}

/// The requested pairs no object of `have` covers, in the spec's order:
/// kinds, then targets, `kind:target`, or `kind:` for the whole check.
pub fn missing_pairs(job: &JobRow, have: &BTreeSet<(String, Option<String>)>) -> Vec<String> {
    let targets: Vec<Option<String>> = if job.targets.is_empty() {
        vec![None]
    } else {
        job.targets.iter().cloned().map(Some).collect()
    };
    let mut missing = Vec::new();
    for kind in &job.kinds {
        for target in &targets {
            if !have.contains(&(kind.clone(), target.clone())) {
                missing.push(format!("{kind}:{}", target.as_deref().unwrap_or("")));
            }
        }
    }
    missing
}

/// A completion repeated after it succeeded: the same token, and nothing
/// the job has not accepted already. Returns the rows of the evidence the
/// repeat names.
fn repeated_completion(
    job: &JobRow,
    token: &str,
    incoming: &[Incoming],
    existing: &[JobEvidenceRow],
    call: &Call,
) -> Option<Vec<JobEvidenceRow>> {
    let repeat = matches!(call, Call::Complete(_))
        && job.state == JobState::Completed
        && digest_matches(job, token);
    if !repeat {
        return None;
    }
    incoming
        .iter()
        .map(|object| {
            existing
                .iter()
                .find(|row| row.evidence_id == object.verified.id)
                .cloned()
        })
        .collect()
}

/// For a `DONE` completion, `JOB_INCOMPLETE` unless `existing` and
/// `incoming` cover every requested pair.
fn check_complete(
    job: &JobRow,
    existing: &[JobEvidenceRow],
    incoming: &[Incoming],
    call: &Call,
) -> Result<(), ExecutorError> {
    let Call::Complete(completion) = call else {
        return Ok(());
    };
    if completion.outcome != "done" {
        return Ok(());
    }
    let mut have: BTreeSet<(String, Option<String>)> = existing
        .iter()
        .map(|row| (row.kind.clone(), row.target.clone()))
        .collect();
    have.extend(incoming.iter().map(|object| {
        (
            object.verified.index.kind.clone(),
            object.verified.index.target.clone(),
        )
    }));
    let missing = missing_pairs(job, &have);
    if missing.is_empty() {
        return Ok(());
    }
    Err(ExecutorError::Incomplete {
        missing_count: missing.len(),
        missing: missing.into_iter().take(MISSING_SHOWN).collect(),
    })
}

/// Stores `incoming` for `job` through `AddEvidence`'s insert, with a
/// `job_evidence` row each; returns those rows.
fn store(
    tx: &rusqlite::Transaction<'_>,
    job: &JobRow,
    incoming: &[Incoming],
    now: Timestamp,
) -> Result<Vec<JobEvidenceRow>, DbError> {
    let claimed = job.claimed_at.unwrap_or(now);
    let fresh_from = claimed.0.saturating_sub(millis(FRESH_SLACK));
    // Evidence rows take the wall clock, as `AddEvidence`'s do; the job's
    // rows the queue's.
    let added = Timestamp::now();
    let mut seq = next_job_evidence_seq_in(tx, job.repo_id, &job.id)?;
    let mut evidence = Vec::with_capacity(incoming.len());
    for object in incoming {
        let (key, row, digests) = crate::service::evidence::evidence_rows(
            &object.verified,
            job.repo_id,
            object.agent.clone(),
            object.remote_token_id.clone(),
            added,
        );
        let inserted = add_evidence_in(tx, &key, &row, None, &digests)?;
        let mut accepted = JobEvidenceRow {
            repo_id: job.repo_id,
            job_id: job.id.clone(),
            evidence_id: object.verified.id.clone(),
            seq,
            attempt: job.attempt,
            kind: object.verified.index.kind.clone(),
            target: object.verified.index.target.clone(),
            created: inserted.created,
            fresh: inserted.created && object.verified.index.started_at_ms >= fresh_from,
            accepted_at: now,
        };
        if insert_job_evidence_in(tx, &accepted)? {
            seq += 1;
        } else {
            // Re-sent to this job: its first row stands, and this call
            // reports it as neither created nor fresh.
            accepted.created = false;
            accepted.fresh = false;
        }
        evidence.push(accepted);
    }
    Ok(evidence)
}

/// Applies `call` to `job` at `now`, after its evidence is stored.
fn apply(job: &mut JobRow, call: &Call, now: Timestamp) {
    match call {
        Call::Update(update) => {
            if update.progress.is_some() {
                job.progress.clone_from(&update.progress);
            }
            if update.release {
                end_attempt(job, now);
            } else {
                let lease = job.lease_ms.unwrap_or(millis(super::DEFAULT_LEASE));
                job.lease_expires_at = Some(Timestamp(now.0.saturating_add(lease)));
            }
        }
        Call::Complete(completion) => {
            end_attempt_time(job, now);
            job.state = JobState::Completed;
            job.outcome = Some(completion.outcome.to_owned());
            job.message.clone_from(&completion.message);
            job.usage_wall_ms = completion.usage_wall_ms;
            job.usage_cpu_ms = completion.usage_cpu_ms;
            job.finished_at = Some(now);
            job.lease_expires_at = None;
            job.deadline_at = None;
        }
    }
}

/// What the transaction decided.
enum Decided {
    /// The call was accepted; the job moved from `before`.
    Done(JobState, Accepted),
    /// A completion repeated after it succeeded: nothing changed.
    Repeat(Accepted),
    /// The claim had lapsed: the lapse is committed, the call refused.
    Lapsed(JobState, JobRow, &'static str),
}

impl super::Executor {
    /// Stores a call's evidence and applies it, for the claim `token` of
    /// job `id`. `incoming` is verified and bound already. Through the
    /// remote listener, `caller` is the token, which must hold the claim.
    pub fn accept(
        &self,
        id: &str,
        token: &str,
        caller: Option<&str>,
        incoming: &[Incoming],
        call: &Call,
    ) -> Result<Accepted, ExecutorError> {
        let now = self.now();
        let decided = self.db.transact_then(
            |tx| {
                let mut job = job_in(tx, id)?.ok_or_else(|| ExecutorError::NotFound {
                    job_id: id.to_owned(),
                })?;
                let before = job.state;
                // Only a completion reads what the job accepted before.
                let existing = match call {
                    Call::Complete(_) => job_evidence_in(tx, job.repo_id, &job.id)?,
                    Call::Update(_) => Vec::new(),
                };
                if let Some(evidence) = repeated_completion(&job, token, incoming, &existing, call)
                {
                    check_owner(&job, caller)?;
                    return Ok(Decided::Repeat(Accepted { job, evidence }));
                }
                // What came due before this call: a lapse refuses it.
                let lapsed = apply_due(&mut job, now);
                match check_call(&job, token, caller) {
                    Ok(()) => {}
                    Err(ExecutorError::NotClaimed { state, .. }) if lapsed => {
                        // Committed with the refusal: the claim is over either way.
                        update_job_in(tx, &job)?;
                        return Ok(Decided::Lapsed(before, job, state));
                    }
                    Err(error) => return Err(error),
                }
                check_complete(&job, &existing, incoming, call)?;
                let evidence = store(tx, &job, incoming, now)?;
                apply(&mut job, call, now);
                update_job_in(tx, &job)?;
                Ok(Decided::Done(before, Accepted { job, evidence }))
            },
            |decided| match decided {
                Decided::Done(before, accepted) => self.after(*before, &accepted.job),
                Decided::Lapsed(before, job, _) => self.after(*before, job),
                Decided::Repeat(_) => {}
            },
        )?;
        match decided {
            Decided::Lapsed(_, job, state) => Err(ExecutorError::NotClaimed {
                job_id: job.id,
                state,
            }),
            Decided::Repeat(accepted) => Ok(accepted),
            Decided::Done(_, accepted) => {
                for row in &accepted.evidence {
                    tracing::info!(
                        job_id = %accepted.job.id,
                        evidence_id = %row.evidence_id,
                        created = row.created,
                        fresh = row.fresh,
                        "job evidence accepted"
                    );
                }
                Ok(accepted)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_are_kinds_times_targets_or_the_whole_check() {
        let mut job = super::super::placeholder();
        job.kinds = vec!["lint".into(), "test".into()];
        let have = BTreeSet::from([("lint".to_owned(), None)]);
        assert_eq!(missing_pairs(&job, &have), ["test:"]);
        job.targets = vec!["a".into(), "b".into()];
        let have = BTreeSet::from([
            ("lint".to_owned(), Some("a".to_owned())),
            ("test".to_owned(), Some("b".to_owned())),
            ("test".to_owned(), Some("b/finer".to_owned())),
        ]);
        assert_eq!(missing_pairs(&job, &have), ["lint:b", "test:a"]);
    }
}
