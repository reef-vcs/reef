//! The landing worker (R3.13): one per repository with queued changes,
//! landing them one at a time, oldest first, then stopping.
//!
//! The queue is the repository's `queued` landings. At start, a queued
//! change with no open landing (a crash between its enqueue and its row)
//! gets one, and every repository with a queued landing gets a worker.

use std::sync::{Arc, Weak};

use futures::FutureExt as _;
use reef_lib::error::ErrorChain;
use reef_lib::jj::OpKind;
use reef_lib::jj::land::{LandError, LandTarget};
use reef_lib::jj::lifecycle::LifecycleState;
use reef_lib::jj::oplog::{self, OpMeta};

use super::{Judged, LandingError, Outcome, Reason, Requester, pins};
use crate::daemon::AppState;
use crate::db::{LandingChangeRow, StoredLanding};
use crate::lifecycle::Actor;
use crate::model::{RepoId, Timestamp};
use crate::workspace::ManagerError;

/// Resumes every repository's queue at start: gives a queued change with
/// no open landing one, and wakes each repository's worker.
pub fn start(state: &Arc<AppState>) {
    super::batch::interrupt_open(state);
    super::push::sweep(state);
    let repos = match state.db.repos_with_queued() {
        Ok(repos) => repos,
        Err(error) => {
            tracing::warn!(error = %ErrorChain(&error), "cannot read the landing queues; they resume at the next request");
            return;
        }
    };
    for repo in repos {
        if let Err(error) = adopt_orphans(state, repo) {
            tracing::warn!(repo = %repo, error = %ErrorChain(&error), "cannot resume a landing queue");
        }
        wake(state, repo);
    }
}

/// Gives each queued change of `repo` with no open landing one.
fn adopt_orphans(state: &AppState, repo: RepoId) -> Result<(), LandingError> {
    for row in state.db.queued_without_landing(repo)? {
        let mut landing = super::new_row(
            repo,
            row.destination.as_deref().unwrap_or_default(),
            &Requester::default(),
        );
        landing.message = Some("resumed at start".to_owned());
        let change = LandingChangeRow {
            change_id: row.change_id.clone(),
            queued_commit: row.commit_id.clone(),
            ..LandingChangeRow::default()
        };
        state.db.write_landing(&landing, &[change])?;
    }
    Ok(())
}

/// Starts `repo`'s worker, or tells the running one to look again.
pub(crate) fn wake(state: &Arc<AppState>, repo: RepoId) {
    if !state.landings.claim(repo) {
        return;
    }
    let weak = Arc::downgrade(state);
    tokio::spawn(run(weak, repo));
}

async fn run(weak: Weak<AppState>, repo: RepoId) {
    // Batches re-formed in a row because the destination moved (R3.14).
    let mut pace = super::batch::Pace::default();
    loop {
        let Some(state) = weak.upgrade() else {
            return;
        };
        if state.shutdown.is_cancelled() {
            return;
        }
        let next = match super::batch::drive(&state, repo, &mut pace).await {
            Ok(next) => next,
            Err(error) => {
                tracing::warn!(repo = %repo, error = %ErrorChain(&error), "a batch failed");
                if let Ok(open) = state.db.open_landings(repo)
                    && let Some(first) = open.first()
                {
                    fail(&state, repo, &first.row.id, &error);
                }
                super::batch::Next::Idle
            }
        };
        state.landings.notify_finished();
        if next == super::batch::Next::Again {
            continue;
        }
        if state.landings.release(repo) {
            return;
        }
    }
}

/// Notes on landing `id` an error the worker could not turn into a
/// dequeue. The change stays queued, and the next wake-up (a request, or
/// the next start) tries again.
pub(super) fn fail(state: &AppState, repo: RepoId, id: &str, error: &LandingError) {
    let Ok(Some(mut landing)) = state.db.landing(repo, id) else {
        return;
    };
    if landing.row.outcome != Outcome::Queued.as_str() {
        return;
    }
    landing.row.message = Some(ErrorChain(error).to_string());
    // The change stays queued: the next wake-up tries again.
    let _ = state.db.write_landing(&landing.row, &landing.changes);
}

/// What `who` the landing's row records.
pub(super) fn who_of(landing: &StoredLanding) -> Requester {
    Requester {
        agent: landing.row.agent.clone(),
        actor: landing.row.actor.clone(),
        identity_id: None,
        identity_name: landing.row.identity_name.clone(),
        session_id: None,
        remote: landing.row.remote_token.clone().map(|id| {
            (
                id,
                landing.row.remote_token_name.clone().unwrap_or_default(),
            )
        }),
        marked: landing.row.agent.clone(),
    }
}

/// Whether `error` came before the landing's operation could be written:
/// a refusal of the plan, the snapshots or the checks (R3.13).
pub(super) fn refused_before_writing(error: &ManagerError) -> bool {
    match error {
        ManagerError::Land(error) => !matches!(error, LandError::Commit(_)),
        ManagerError::Rewrite(_) | ManagerError::RewriteSnapshot { .. } => true,
        _ => false,
    }
}

/// The land operation that recorded `transition`, if one was written, and
/// the commit its attribute names.
pub(super) async fn find_written(
    state: &AppState,
    repo: RepoId,
    transition: &crate::db::TransitionRow,
) -> Option<(String, String)> {
    let handle = state.manager.handle(repo).ok()?;
    let id = transition.id.clone();
    let created = std::time::UNIX_EPOCH
        + std::time::Duration::from_millis(u64::try_from(transition.created_at.0).unwrap_or(0));
    let not_before = created
        .checked_sub(std::time::Duration::from_mins(5))
        .unwrap_or(std::time::UNIX_EPOCH);
    handle
        .call_async(move |head| {
            async move {
                let repo = head.refresh().await.ok()?;
                let ids = std::collections::BTreeSet::from([id.clone()]);
                let found = reef_lib::jj::lifecycle::find_transitions(&repo, &ids, not_before)
                    .await
                    .ok()?;
                let op_id = found.get(&id)?.op_id.to_string();
                let op = oplog::resolve_operation(&repo, &op_id).await.ok()?;
                let commit = reef_lib::jj::lifecycle::transitions_of(&oplog::entry(&op).attributes)
                    .into_iter()
                    .find(|record| record.id == id)?
                    .commit;
                Some((op_id, commit))
            }
            .boxed_local()
        })
        .await
        .ok()
        .flatten()
}

/// The landing's policy columns from `judged`.
pub(super) fn record_policy(landing: &mut StoredLanding, judged: &Judged) {
    landing.row.policy_commit = Some(judged.policy.commit.clone());
    landing.row.policy_blob.clone_from(&judged.policy.blob);
    landing.row.policy_sha256.clone_from(&judged.policy.sha256);
}

/// When the landing has nothing left to do (its change is no longer queued
/// at the queued commit), finishes it and returns how; `None` to go on.
pub(super) fn settled(
    state: &Arc<AppState>,
    repo: RepoId,
    landing: &mut StoredLanding,
    target: &LandTarget,
) -> Result<Option<Result<(), LandingError>>, LandingError> {
    let row = state.db.lifecycle_row(repo, target.change.as_str())?;
    let Some(row) = row else {
        return Ok(Some(finish(
            state,
            repo,
            landing,
            Outcome::Cancelled,
            None,
            "the change has no proposal",
        )));
    };
    if row.state == LifecycleState::Queued.as_str() && row.commit_id == target.commit.as_str() {
        return Ok(None);
    }
    // Landed by this landing before a crash cut its record: finish it so.
    if row.state == LifecycleState::Landed.as_str()
        && let Some(last) = state.db.transition(repo, &row.transition_id)?
        && last.cause == reef_lib::jj::lifecycle::TransitionCause::Landing.as_str()
        && landing.row.operation_id.is_none()
        && state
            .db
            .open_landing_of(repo, target.change.as_str())?
            .is_some_and(|latest| latest.row.id == landing.row.id)
    {
        if let Some(first) = landing.changes.first_mut() {
            first.landed_commit = Some(last.commit_id.clone());
        }
        landing.row.destination_after = Some(last.commit_id.clone());
        landing.row.operation_id.clone_from(&last.operation_id);
        let pinned: Vec<(String, String)> = landing
            .changes
            .first()
            .and_then(|first| first.evaluation.as_deref())
            .and_then(|json| serde_json::from_str::<reef_policy::Evaluation>(json).ok())
            .map(|evaluation| {
                pins(&evaluation)
                    .into_iter()
                    .map(|id| (target.change.as_str().to_owned(), id))
                    .collect()
            })
            .unwrap_or_default();
        // The `[history]` its batch formed under (#297): what a landing
        // finished in its batch records.
        let history = landing
            .row
            .batch_id
            .as_deref()
            .and_then(|id| state.db.batch(repo, id).ok().flatten())
            .and_then(|batch| batch.row.history);
        if let Some(recorded) = history.as_deref().and_then(super::history::Recorded::parse) {
            let summarized = recorded.message == reef_policy::Message::Summary.as_str();
            if let Some(first) = landing.changes.first_mut() {
                first.summarized = summarized;
            }
            landing.row.history = history;
        }
        Outcome::Landed
            .as_str()
            .clone_into(&mut landing.row.outcome);
        landing.row.finished_at = Some(Timestamp::now());
        state
            .db
            .finish_landing(&landing.row, &landing.changes, &pinned, None)?;
        if state
            .db
            .destination_push(repo, &landing.row.destination)
            .is_ok_and(|row| row.is_some())
        {
            super::push::wake(
                state,
                repo,
                &landing.row.destination,
                super::push::Trigger::Landing,
            );
        }
        return Ok(Some(Ok(())));
    }
    let message = format!("the change is now {}", row.state);
    Ok(Some(finish(
        state,
        repo,
        landing,
        Outcome::Cancelled,
        None,
        &message,
    )))
}

/// Finishes the landing with `outcome`.
pub(super) fn finish(
    state: &AppState,
    _repo: RepoId,
    landing: &mut StoredLanding,
    outcome: Outcome,
    reason: Option<Reason>,
    message: &str,
) -> Result<(), LandingError> {
    outcome.as_str().clone_into(&mut landing.row.outcome);
    landing.row.reason = reason.map(|reason| reason.as_str().to_owned());
    landing.row.message = Some(message.to_owned()).filter(|message| !message.is_empty());
    landing.row.finished_at = Some(Timestamp::now());
    state.db.write_landing(&landing.row, &landing.changes)?;
    state.landings.notify_finished();
    tracing::info!(landing_id = %landing.row.id, outcome = outcome.as_str(), "landing finished");
    Ok(())
}

/// Returns the change to proposed with `reason`, under the lifecycle lock.
pub(super) async fn dequeue(
    state: &Arc<AppState>,
    repo: RepoId,
    landing: &mut StoredLanding,
    target: &LandTarget,
    who: &Requester,
    reason: Reason,
    message: &str,
) -> Result<(), LandingError> {
    let lifecycle_lock = state.lifecycle.lock(repo);
    let _held = lifecycle_lock.lock().await;
    dequeue_locked(state, repo, landing, target, who, reason, message).await
}

/// [`dequeue`], the lifecycle lock held.
pub(super) async fn dequeue_locked(
    state: &Arc<AppState>,
    repo: RepoId,
    landing: &mut StoredLanding,
    target: &LandTarget,
    who: &Requester,
    reason: Reason,
    message: &str,
) -> Result<(), LandingError> {
    let row = state.db.lifecycle_row(repo, target.change.as_str())?;
    let Some(row) = row.filter(|row| {
        row.state == LifecycleState::Queued.as_str() && row.commit_id == target.commit.as_str()
    }) else {
        return finish(
            state,
            repo,
            landing,
            Outcome::Cancelled,
            None,
            "the change left the queue",
        );
    };
    let transition = super::transition_row(
        repo,
        target.change.as_str(),
        target.commit.as_str(),
        LifecycleState::Queued,
        LifecycleState::Proposed,
        row.round,
        &target.bookmark,
        Some(format!("{}: {message}", reason.as_str())),
        who,
        Actor::Daemon,
    );
    let mut meta = OpMeta::new(
        OpKind::Transition,
        oplog::transition_description("dequeue", target.change.as_str()),
    );
    meta.agent.clone_from(&who.marked);
    let recorded =
        crate::lifecycle::record_daemon_transition(state, repo, transition, meta).await?;
    landing.row.operation_id = Some(recorded.op_id.to_string());
    finish(
        state,
        repo,
        landing,
        Outcome::Dequeued,
        Some(reason),
        message,
    )?;
    tracing::info!(change_id = %target.change, reason = reason.as_str(), "returned to proposed");
    Ok(())
}

/// When operation `op_id` ended; now when it cannot be read.
pub(crate) async fn operation_end(state: &AppState, repo: RepoId, op_id: &str) -> Timestamp {
    let Ok(handle) = state.manager.handle(repo) else {
        return Timestamp::now();
    };
    let wanted = op_id.to_owned();
    let found = handle
        .call_async(move |head| {
            async move {
                let repo = head.refresh().await.ok()?;
                let op = oplog::resolve_operation(&repo, &wanted).await.ok()?;
                Some(oplog::entry(&op).end)
            }
            .boxed_local()
        })
        .await;
    match found {
        Ok(Some(end)) => Timestamp::from(end),
        _ => Timestamp::now(),
    }
}
