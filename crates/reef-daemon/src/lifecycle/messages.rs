//! Lifecycle rows as v1 messages, and the join that fills
//! `Change.lifecycle` on every change the daemon returns (R3.09).

use std::collections::HashMap;
use std::time::Duration;

use reef_api::v1;
use reef_lib::jj::lifecycle::{LifecycleState, TransitionCause};
use reef_lib::jj::oplog::OpEntry;

use super::Actor;
use crate::daemon::AppState;
use crate::db::{DbError, LifecycleRow, TransitionRow};
use crate::model::{RepoId, Timestamp};
use crate::service::convert::timestamp;

/// How long a read waits for a pending transition to commit (#293). A
/// transition's operation is usually one jj write; a batch landing or an
/// observed landing that moves many workspaces can take longer, and a read
/// past this answers with the committed rows, as before.
const PENDING_WAIT: Duration = Duration::from_secs(5);

/// How recent a pending row must be for a read to wait for it: an older
/// one is a write that failed and left its row for the next start.
const PENDING_RECENT: Duration = Duration::from_secs(30);

/// Waits, for at most five seconds, until no transition of `changes`
/// (every change of `repo` when `None`) is being written, so a read never
/// shows a transition's operation without its row (#293). A writer holds
/// `repo`'s lifecycle lock from the pending row to its commit, so the wait
/// is for that lock; it is taken alone and dropped at once. The caller
/// holds no other lock: a reader never stalls a writer for longer than one
/// acquire and release. Most reads find no pending row and do not wait.
pub async fn await_pending(state: &AppState, repo: RepoId, changes: Option<&[String]>) {
    let since = Timestamp(
        Timestamp::now()
            .0
            .saturating_sub(i64::try_from(PENDING_RECENT.as_millis()).unwrap_or(i64::MAX)),
    );
    let pending = match state.db.pending_changes(repo, since) {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(error = %reef_lib::error::ErrorChain(&error), "cannot read pending transitions; answering without waiting for them");
            return;
        }
    };
    let wait = match changes {
        None => !pending.is_empty(),
        Some(changes) => changes.iter().any(|change| pending.contains(change)),
    };
    if !wait {
        return;
    }
    let lock = state.lifecycle.lock(repo);
    if tokio::time::timeout(PENDING_WAIT, lock.lock())
        .await
        .is_err()
    {
        tracing::warn!(repo = %repo, "a pending transition outlasted a read's wait; answering with the committed rows");
    }
}

/// A state as the v1 enum.
pub fn state_message(state: LifecycleState) -> v1::LifecycleState {
    match state {
        LifecycleState::Draft => v1::LifecycleState::Draft,
        LifecycleState::Proposed => v1::LifecycleState::Proposed,
        LifecycleState::Queued => v1::LifecycleState::Queued,
        LifecycleState::Landed => v1::LifecycleState::Landed,
        LifecycleState::Declined => v1::LifecycleState::Declined,
        LifecycleState::Withdrawn => v1::LifecycleState::Withdrawn,
        LifecycleState::Superseded => v1::LifecycleState::Superseded,
    }
}

/// The state a v1 value names; `None` for UNSPECIFIED or an unknown value.
pub fn state_of(value: i32) -> Option<LifecycleState> {
    Some(match v1::LifecycleState::try_from(value).ok()? {
        v1::LifecycleState::Unspecified => return None,
        v1::LifecycleState::Draft => LifecycleState::Draft,
        v1::LifecycleState::Proposed => LifecycleState::Proposed,
        v1::LifecycleState::Queued => LifecycleState::Queued,
        v1::LifecycleState::Landed => LifecycleState::Landed,
        v1::LifecycleState::Declined => LifecycleState::Declined,
        v1::LifecycleState::Withdrawn => LifecycleState::Withdrawn,
        v1::LifecycleState::Superseded => LifecycleState::Superseded,
    })
}

/// A column's state text as the v1 enum.
fn column_state(text: &str) -> v1::LifecycleState {
    text.parse()
        .map_or(v1::LifecycleState::Unspecified, state_message)
}

/// A cause as the v1 enum.
pub fn cause_message(cause: TransitionCause) -> v1::TransitionCause {
    match cause {
        TransitionCause::Request => v1::TransitionCause::Request,
        TransitionCause::Undo => v1::TransitionCause::Undo,
        TransitionCause::Observed => v1::TransitionCause::Observed,
        TransitionCause::Landing => v1::TransitionCause::Landing,
    }
}

/// The cause a v1 value names; `None` for UNSPECIFIED or an unknown value.
pub fn cause_of(value: i32) -> Option<TransitionCause> {
    match v1::TransitionCause::try_from(value).ok()? {
        v1::TransitionCause::Unspecified => None,
        v1::TransitionCause::Request => Some(TransitionCause::Request),
        v1::TransitionCause::Undo => Some(TransitionCause::Undo),
        v1::TransitionCause::Observed => Some(TransitionCause::Observed),
        v1::TransitionCause::Landing => Some(TransitionCause::Landing),
    }
}

/// The actor a v1 value names; `None` for UNSPECIFIED, DAEMON (which a
/// client cannot declare) or an unknown value.
pub fn actor_of(value: i32) -> Option<Actor> {
    match v1::TransitionActor::try_from(value).ok()? {
        v1::TransitionActor::Human => Some(Actor::Human),
        v1::TransitionActor::Agent => Some(Actor::Agent),
        v1::TransitionActor::Daemon | v1::TransitionActor::Unspecified => None,
    }
}

pub(crate) fn actor_message(text: &str) -> v1::TransitionActor {
    match Actor::parse(text) {
        Actor::Human => v1::TransitionActor::Human,
        Actor::Agent => v1::TransitionActor::Agent,
        Actor::Daemon => v1::TransitionActor::Daemon,
    }
}

/// A committed transition as a message.
pub fn transition_message(row: &TransitionRow) -> v1::ChangeTransition {
    v1::ChangeTransition {
        id: row.id.clone(),
        change_id: row.change_id.clone(),
        commit_id: row.commit_id.clone(),
        from: column_state(&row.from_state).into(),
        to: column_state(&row.to_state).into(),
        cause: row
            .cause
            .parse()
            .map_or(v1::TransitionCause::Unspecified, cause_message)
            .into(),
        round: row.round,
        reason: row.reason.clone().unwrap_or_default(),
        superseded_by: row.superseded_by.clone().unwrap_or_default(),
        destination: row.destination.clone().unwrap_or_default(),
        undoes_operation_id: row.undoes_operation_id.clone().unwrap_or_default(),
        operation_id: row.operation_id.clone().unwrap_or_default(),
        operation_collected: row.operation_collected,
        recorded_at: row.recorded_at.map(timestamp),
        agent: row.agent.clone().unwrap_or_default(),
        actor: actor_message(&row.actor).into(),
        identity: row.identity_name.clone().unwrap_or_default(),
        identity_id: row.identity_id.clone().unwrap_or_default(),
        session_id: row.session_id.clone().unwrap_or_default(),
        remote: row.remote_token.as_ref().map(|token| v1::RemoteCaller {
            token_id: token.clone(),
            token_name: row.remote_token_name.clone().unwrap_or_default(),
        }),
    }
}

/// The transitions an operation's `reef.lifecycle` attribute records, as
/// messages: what the attribute holds, without who performed them.
pub fn operation_transitions(entry: &OpEntry) -> Vec<v1::ChangeTransition> {
    reef_lib::jj::lifecycle::transitions_of(&entry.attributes)
        .into_iter()
        .map(|record| v1::ChangeTransition {
            id: record.id,
            change_id: record.change,
            commit_id: record.commit,
            from: state_message(record.from).into(),
            to: state_message(record.to).into(),
            cause: cause_message(record.cause).into(),
            round: record.round,
            reason: record.reason,
            superseded_by: record.superseded_by,
            destination: record.destination,
            undoes_operation_id: record.undoes,
            operation_id: entry.id.to_string(),
            recorded_at: Some(timestamp(Timestamp::from(entry.end))),
            ..v1::ChangeTransition::default()
        })
        .collect()
}

/// A recorded lifecycle as a message, for a change whose visible commit is
/// `commit` (`None` when it has none).
pub fn lifecycle_message(
    row: &LifecycleRow,
    last: Option<&TransitionRow>,
    commit: Option<&str>,
) -> v1::ChangeLifecycle {
    let state: LifecycleState = row.state.parse().unwrap_or(LifecycleState::Draft);
    let in_review = matches!(state, LifecycleState::Proposed | LifecycleState::Queued);
    v1::ChangeLifecycle {
        state: state_message(state).into(),
        recorded: true,
        round: row.round,
        commit_id: row.commit_id.clone(),
        amended: in_review && commit.is_some_and(|commit| commit != row.commit_id),
        destination: row.destination.clone().unwrap_or_default(),
        superseded_by: row.superseded_by.clone().unwrap_or_default(),
        last: last.map(transition_message),
        visible: commit.is_some(),
    }
}

/// The derived lifecycle of a change with no record: LANDED when immutable,
/// else DRAFT.
pub fn derived(change: &v1::Change) -> v1::ChangeLifecycle {
    v1::ChangeLifecycle {
        state: if change.immutable {
            v1::LifecycleState::Landed
        } else {
            v1::LifecycleState::Draft
        }
        .into(),
        visible: !change.hidden,
        ..v1::ChangeLifecycle::default()
    }
}

/// A transition row as of a past operation, as a lifecycle row: its state
/// then.
fn as_lifecycle(row: &TransitionRow) -> LifecycleRow {
    LifecycleRow {
        repo_id: row.repo_id,
        change_id: row.change_id.clone(),
        state: row.to_state.clone(),
        round: row.round,
        commit_id: row.commit_id.clone(),
        destination: row.destination.clone(),
        superseded_by: row.superseded_by.clone(),
        transition_id: row.id.clone(),
        proposed_by_agent: None,
        proposed_by_token: None,
        updated_at: row.recorded_at.unwrap_or(row.created_at),
    }
}

/// Fills `lifecycle` on each of `changes` of `repo`: at head when `at` is
/// `None`, else as of the operation `at` names, which ended at its time (a
/// read at an operation: the latest transition it or an earlier operation
/// recorded). A database failure leaves them unset and is logged: a read
/// never fails for it.
///
/// A change with a pending transition is being written: its operation may
/// already be visible (an observed landing's workspaces are on their new
/// changes) while its row is not yet committed. The read first waits for
/// it ([`await_pending`]); at an operation too, since the row, once
/// committed, is found by that operation's id.
pub async fn fill<'a>(
    state: &AppState,
    repo: RepoId,
    changes: impl IntoIterator<Item = &'a mut v1::Change>,
    at: Option<(Timestamp, String)>,
) {
    let mut changes: Vec<&mut v1::Change> = changes.into_iter().collect();
    if changes.is_empty() {
        return;
    }
    let ids: Vec<String> = changes
        .iter()
        .map(|change| change.change_id.clone())
        .collect();
    await_pending(state, repo, Some(&ids)).await;
    match lookup(state, repo, &ids, at) {
        Ok(found) => {
            for change in &mut changes {
                let commit = (!change.hidden).then_some(change.commit_id.as_str());
                change.lifecycle = Some(match found.get(&change.change_id) {
                    Some((row, last)) if row.state != LifecycleState::Draft.as_str() => {
                        let mut message = lifecycle_message(row, last.as_ref(), commit);
                        // A change named by a hidden commit may still be
                        // visible through another; only the latest is known
                        // here.
                        message.visible = !change.hidden;
                        message
                    }
                    _ => derived(change),
                });
            }
        }
        Err(error) => {
            tracing::warn!(error = %reef_lib::error::ErrorChain(&error), "cannot read changes' lifecycles; answering without them");
        }
    }
}

/// The lifecycle rows of `ids` with their latest transitions, in two
/// queries.
fn lookup(
    state: &AppState,
    repo: RepoId,
    ids: &[String],
    at: Option<(Timestamp, String)>,
) -> Result<HashMap<String, (LifecycleRow, Option<TransitionRow>)>, DbError> {
    let mut found = HashMap::new();
    match at {
        None => {
            let rows = state.db.lifecycle_rows(repo, ids)?;
            let wanted: Vec<String> = rows.values().map(|row| row.transition_id.clone()).collect();
            let mut lasts = state.db.transitions_by_id(repo, &wanted)?;
            for (change, row) in rows {
                let last = lasts.remove(&row.transition_id);
                found.insert(change, (row, last));
            }
        }
        Some((at, op)) => {
            for (change, row) in state.db.transitions_as_of(repo, ids, at, &op)? {
                found.insert(change, (as_lifecycle(&row), Some(row)));
            }
        }
    }
    Ok(found)
}
