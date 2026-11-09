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
use reef_lib::jj::lifecycle::{LifecycleState, TransitionRecord};
use reef_lib::jj::oplog::{self, OpMeta};

use super::{
    Judged, LandingError, MAX_ATTEMPTS, Outcome, Reason, Requester, Verdict, change_hex,
    change_row, commit_hex, judge, pins,
};
use crate::daemon::AppState;
use crate::db::{LandingChangeRow, StoredLanding};
use crate::lifecycle::Actor;
use crate::model::{RepoId, Timestamp};
use crate::workspace::{ManagerError, Write, WriteRequest};

/// Resumes every repository's queue at start: gives a queued change with
/// no open landing one, and wakes each repository's worker.
pub fn start(state: &Arc<AppState>) {
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
    loop {
        let Some(state) = weak.upgrade() else {
            return;
        };
        if state.shutdown.is_cancelled() {
            return;
        }
        let open = match state.db.open_landings(repo) {
            Ok(open) => open,
            Err(error) => {
                tracing::warn!(repo = %repo, error = %ErrorChain(&error), "cannot read the landing queue");
                Vec::new()
            }
        };
        for landing in open {
            if state.shutdown.is_cancelled() {
                return;
            }
            let id = landing.row.id.clone();
            if let Err(error) = land_batch(&state, repo, landing).await {
                tracing::warn!(repo = %repo, landing_id = %id, error = %ErrorChain(&error), "a landing failed");
                fail(&state, repo, &id, &error);
            }
            state.landings.notify_finished();
        }
        if state.landings.release(repo) {
            return;
        }
    }
}

/// Notes on landing `id` an error the worker could not turn into a
/// dequeue. The change stays queued, and the next wake-up (a request, or
/// the next start) tries again.
fn fail(state: &AppState, repo: RepoId, id: &str, error: &LandingError) {
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

/// Lands a queued landing's changes in order, a prefix of them when one
/// fails: one change until stacks (R3.10) and batches (R3.14).
pub(crate) async fn land_batch(
    state: &Arc<AppState>,
    repo: RepoId,
    landing: StoredLanding,
) -> Result<(), LandingError> {
    land_one(state, repo, landing).await
}

/// What `who` the landing's row records.
fn who_of(landing: &StoredLanding) -> Requester {
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

#[expect(clippy::too_many_lines, reason = "a landing's steps in order")]
async fn land_one(
    state: &Arc<AppState>,
    repo: RepoId,
    mut landing: StoredLanding,
) -> Result<(), LandingError> {
    let Some(first) = landing.changes.first().cloned() else {
        return finish(
            state,
            repo,
            &mut landing,
            Outcome::Cancelled,
            None,
            "no change",
        );
    };
    let (Some(change), Some(commit)) = (
        change_hex(&first.change_id),
        commit_hex(&first.queued_commit),
    ) else {
        return finish(
            state,
            repo,
            &mut landing,
            Outcome::Cancelled,
            None,
            "an unreadable change",
        );
    };
    let target = LandTarget {
        change,
        commit,
        bookmark: landing.row.destination.clone(),
    };
    let who = who_of(&landing);
    // Held to the end: GC can neither delete the evidence judged nor
    // rewrite the operation before its row is committed.
    let shared = Arc::new(state.manager.repo_lock(repo).read_owned().await);
    // Read again under the lock: GC may have mapped its operation ids while
    // this waited (review finding 4).
    match state.db.landing(repo, &landing.row.id)? {
        Some(fresh) if fresh.row.outcome == Outcome::Queued.as_str() => landing = fresh,
        _ => return Ok(()),
    }
    for _attempt in 0..MAX_ATTEMPTS {
        // Still queued at the commit it was queued at?
        if let Some(done) = settled(state, repo, &mut landing, &target)? {
            return done;
        }
        let verdict = judge(state, repo, &target).await?;
        let judged = match verdict {
            Verdict::Allow(judged) => judged,
            Verdict::Block {
                reason: Reason::Busy,
                ..
            } => continue,
            Verdict::Block {
                reason,
                message,
                preview,
                judged,
                destination,
                ..
            } => {
                landing.row.destination_before = destination;
                landing.changes = vec![change_row(
                    &target,
                    first.queued_tree.clone(),
                    preview.as_ref(),
                    judged.as_deref(),
                )];
                if let Some(judged) = &judged {
                    record_policy(&mut landing, judged);
                }
                return dequeue(state, repo, &mut landing, &target, &who, reason, &message).await;
            }
        };
        landing.row.destination_before = Some(judged.preview.destination.as_str().to_owned());
        record_policy(&mut landing, &judged);
        landing.changes = vec![change_row(
            &target,
            first.queued_tree.clone(),
            None,
            Some(&judged),
        )];
        state.db.write_landing(&landing.row, &landing.changes)?;

        // Land, holding the lifecycle lock from the check to the record.
        let lifecycle_lock = state.lifecycle.lock(repo);
        let held = lifecycle_lock.lock().await;
        if let Some(done) = settled(state, repo, &mut landing, &target)? {
            return done;
        }
        let row = state
            .db
            .lifecycle_row(repo, target.change.as_str())?
            .ok_or_else(|| LandingError::Input("the queued change has no row".to_owned()))?;
        let mut transition = super::transition_row(
            repo,
            target.change.as_str(),
            target.commit.as_str(),
            LifecycleState::Queued,
            LifecycleState::Landed,
            row.round,
            &target.bookmark,
            None,
            &who,
            Actor::Daemon,
        );
        state.db.insert_pending_transition(&transition)?;
        let record = TransitionRecord {
            id: transition.id.clone(),
            change: transition.change_id.clone(),
            commit: String::new(),
            from: LifecycleState::Queued,
            to: LifecycleState::Landed,
            cause: reef_lib::jj::lifecycle::TransitionCause::Landing,
            round: row.round,
            reason: String::new(),
            superseded_by: String::new(),
            destination: target.bookmark.clone(),
            undoes: String::new(),
        };
        let written = state
            .manager
            .rewrite_held(
                WriteRequest {
                    repo,
                    workspace: None,
                    agent: who.marked.clone(),
                    write: Write::Land {
                        target: target.clone(),
                        destination: judged.preview.destination.clone(),
                        landing: landing.row.id.clone(),
                        transitions: vec![record],
                    },
                },
                Arc::clone(&shared),
            )
            .await;
        let written = match written {
            Ok(written) => written,
            Err(error) if !refused_before_writing(&error) => {
                // The operation may have been written before the failure:
                // look for it, as start-up does (review finding 3).
                if let Some((op_id, commit)) = find_written(state, repo, &transition).await {
                    let info = (commit, None);
                    return record_landed(
                        state,
                        repo,
                        &mut landing,
                        &target,
                        (&mut transition, &op_id, info),
                        &judged,
                    )
                    .await;
                }
                let _ = state.db.delete_pending_transition(repo, &transition.id);
                let message = ErrorChain(&error).to_string();
                return dequeue_locked(
                    state,
                    repo,
                    &mut landing,
                    &target,
                    &who,
                    Reason::Failed,
                    &message,
                )
                .await;
            }
            Err(error) => {
                let _ = state.db.delete_pending_transition(repo, &transition.id);
                let reason = match &error {
                    ManagerError::Land(LandError::DestinationMoved { .. }) => {
                        drop(held);
                        continue;
                    }
                    ManagerError::Land(LandError::Amended { .. })
                    | ManagerError::Rewrite(reef_lib::jj::rewrite::RewriteError::Hidden(_)) => {
                        Reason::Amended
                    }
                    ManagerError::Land(LandError::RebaseConflicts | LandError::Conflicted(_)) => {
                        Reason::Conflicted
                    }
                    ManagerError::Land(LandError::UnlandedParents { .. }) => {
                        Reason::UnlandedParents
                    }
                    ManagerError::Land(
                        LandError::DestinationNotFound(_)
                        | LandError::DestinationConflicted(_)
                        | LandError::AlreadyOnDestination(_),
                    ) => Reason::Destination,
                    _ => Reason::Failed,
                };
                let message = ErrorChain(&error).to_string();
                return dequeue_locked(state, repo, &mut landing, &target, &who, reason, &message)
                    .await;
            }
        };
        let (Some(op_id), Some(info)) = (written.op_id.clone(), written.change.clone()) else {
            let _ = state.db.delete_pending_transition(repo, &transition.id);
            return Err(LandingError::Input("the landing wrote nothing".to_owned()));
        };
        let landed = (info.commit_id.as_str().to_owned(), info.tree_id.clone());
        let done = record_landed(
            state,
            repo,
            &mut landing,
            &target,
            (&mut transition, &op_id, landed),
            &judged,
        )
        .await;
        drop(shared);
        return done;
    }
    dequeue(
        state,
        repo,
        &mut landing,
        &target,
        &who,
        Reason::Busy,
        &format!(
            "{} moved under {MAX_ATTEMPTS} attempts in a row",
            target.bookmark
        ),
    )
    .await
}

/// Whether `error` came before the landing's operation could be written:
/// a refusal of the plan, the snapshots or the checks (R3.13).
fn refused_before_writing(error: &ManagerError) -> bool {
    match error {
        ManagerError::Land(error) => !matches!(error, LandError::Commit(_)),
        ManagerError::Rewrite(_) | ManagerError::RewriteSnapshot { .. } => true,
        _ => false,
    }
}

/// The land operation that recorded `transition`, if one was written, and
/// the commit its attribute names.
async fn find_written(
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

/// Commits the land transition `transition` with `op_id` and finishes the
/// landing landed at `(commit, tree)`, pinning what `judged` used.
async fn record_landed(
    state: &AppState,
    repo: RepoId,
    landing: &mut StoredLanding,
    target: &LandTarget,
    (transition, op_id, (landed_commit, landed_tree)): (
        &mut crate::db::TransitionRow,
        &str,
        (String, Option<String>),
    ),
    judged: &Judged,
) -> Result<(), LandingError> {
    state
        .db
        .set_pending_transition_commit(repo, &transition.id, &landed_commit)?;
    transition.commit_id.clone_from(&landed_commit);
    let recorded_at = operation_end(state, repo, op_id).await;
    crate::lifecycle::commit_written_transition(state, repo, transition, op_id, recorded_at)?;
    if let Some(first) = landing.changes.first_mut() {
        first.landed_commit = Some(landed_commit.clone());
        first.landed_tree = landed_tree;
    }
    landing.row.destination_after = Some(landed_commit);
    landing.row.operation_id = Some(op_id.to_owned());
    let pinned: Vec<(String, String)> = pins(&judged.evaluation)
        .into_iter()
        .map(|id| (target.change.as_str().to_owned(), id))
        .collect();
    Outcome::Landed
        .as_str()
        .clone_into(&mut landing.row.outcome);
    landing.row.finished_at = Some(Timestamp::now());
    let policy = judged
        .policy
        .sha256
        .as_deref()
        .zip(judged.policy.bytes.as_deref());
    state
        .db
        .finish_landing(&landing.row, &landing.changes, &pinned, policy)?;
    tracing::info!(change_id = %target.change, landing_id = %landing.row.id, op_id = %op_id, "landed");
    Ok(())
}

/// The landing's policy columns from `judged`.
fn record_policy(landing: &mut StoredLanding, judged: &Judged) {
    landing.row.policy_commit = Some(judged.policy.commit.clone());
    landing.row.policy_blob.clone_from(&judged.policy.blob);
    landing.row.policy_sha256.clone_from(&judged.policy.sha256);
}

/// When the landing has nothing left to do (its change is no longer queued
/// at the queued commit), finishes it and returns how; `None` to go on.
fn settled(
    state: &AppState,
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
        Outcome::Landed
            .as_str()
            .clone_into(&mut landing.row.outcome);
        landing.row.finished_at = Some(Timestamp::now());
        state
            .db
            .finish_landing(&landing.row, &landing.changes, &pinned, None)?;
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
fn finish(
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
    tracing::info!(landing_id = %landing.row.id, outcome = outcome.as_str(), "landing finished");
    Ok(())
}

/// Returns the change to proposed with `reason`, under the lifecycle lock.
async fn dequeue(
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
async fn dequeue_locked(
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
async fn operation_end(state: &AppState, repo: RepoId, op_id: &str) -> Timestamp {
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
