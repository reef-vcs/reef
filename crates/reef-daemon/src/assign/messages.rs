//! Assignments as API messages (R3.27).

use std::collections::HashMap;

use reef_api::v1;
use reef_lib::jj::assignment::AssignmentNotice;
use reef_lib::jj::oplog::OpEntry;

use super::{KIND_TREE, Reason, State};
use crate::daemon::AppState;
use crate::db::{AssignmentEventRow, AssignmentRow, OwnerColumns, TierColumns};
use crate::model::Timestamp;

fn time(at: Timestamp) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: at.0.div_euclid(1000),
        nanos: i32::try_from(at.0.rem_euclid(1000) * 1_000_000).unwrap_or(0),
    }
}

/// A state's message value.
pub fn state_message(text: &str) -> v1::ConflictAssignmentState {
    match State::parse(text) {
        Some(State::Assigned) => v1::ConflictAssignmentState::Assigned,
        Some(State::Queued) => v1::ConflictAssignmentState::Queued,
        Some(State::Taken) => v1::ConflictAssignmentState::Taken,
        Some(State::Unowned) => v1::ConflictAssignmentState::Unowned,
        Some(State::Closed) => v1::ConflictAssignmentState::Closed,
        Some(State::Blocked) => v1::ConflictAssignmentState::Blocked,
        None => v1::ConflictAssignmentState::Unspecified,
    }
}

/// The state a message value names.
pub fn state_of(value: i32) -> Option<State> {
    match v1::ConflictAssignmentState::try_from(value).ok()? {
        v1::ConflictAssignmentState::Assigned => Some(State::Assigned),
        v1::ConflictAssignmentState::Queued => Some(State::Queued),
        v1::ConflictAssignmentState::Taken => Some(State::Taken),
        v1::ConflictAssignmentState::Unowned => Some(State::Unowned),
        v1::ConflictAssignmentState::Closed => Some(State::Closed),
        v1::ConflictAssignmentState::Blocked => Some(State::Blocked),
        v1::ConflictAssignmentState::Unspecified => None,
    }
}

/// A reason's message value.
pub fn reason_message(text: &str) -> v1::ConflictAssignmentReason {
    use v1::ConflictAssignmentReason as R;
    match Reason::parse(text) {
        Some(Reason::Default) => R::Default,
        Some(Reason::Followed) => R::Followed,
        Some(Reason::Returned) => R::Returned,
        Some(Reason::OwnerEnded) => R::OwnerEnded,
        Some(Reason::NoOwner) => R::NoOwner,
        Some(Reason::Claimed) => R::Claimed,
        Some(Reason::Released) => R::Released,
        Some(Reason::Assigned) => R::Assigned,
        Some(Reason::Taken) => R::Taken,
        Some(Reason::Changed) => R::Changed,
        Some(Reason::Resolved) => R::Resolved,
        Some(Reason::Inherited) => R::Inherited,
        Some(Reason::Abandoned) => R::Abandoned,
        Some(Reason::Immutable) => R::Immutable,
        Some(Reason::Reopened) => R::Reopened,
        Some(Reason::Ask) => R::Ask,
        Some(Reason::Attempts) => R::Attempts,
        Some(Reason::Allowed) => R::Allowed,
        None => R::Unspecified,
    }
}

/// A tier's message value.
pub fn tier_value(text: Option<&str>) -> v1::ConflictTier {
    match text.and_then(reef_policy::Tier::parse) {
        Some(reef_policy::Tier::Land) => v1::ConflictTier::Land,
        Some(reef_policy::Tier::Approve) => v1::ConflictTier::Approve,
        Some(reef_policy::Tier::Ask) => v1::ConflictTier::Ask,
        None => v1::ConflictTier::Unspecified,
    }
}

/// `columns` as a message; `None` until looked up.
fn tier_message(columns: &TierColumns) -> Option<v1::ConflictTierDecision> {
    columns.tier.as_ref()?;
    let mut message = v1::ConflictTierDecision {
        attempts: columns.cap.unwrap_or_default(),
        rule: columns.rule,
        policy_commit: columns.policy_commit.clone().unwrap_or_default(),
        policy_sha256: columns
            .policy_sha256
            .clone()
            .filter(|key| !key.starts_with("blob:"))
            .unwrap_or_default(),
        error: columns.error.clone().unwrap_or_default(),
        conflict_id: columns.conflict_id.clone().unwrap_or_default(),
        looked_up_at: columns.at.map(time),
        ..v1::ConflictTierDecision::default()
    };
    message.set_tier(tier_value(columns.tier.as_deref()));
    Some(message)
}

/// `row`, closed resolved, as the resolution a proposal lists (R3.28).
pub fn resolution_message(state: &AppState, row: &AssignmentRow) -> v1::ConflictResolution {
    let session = row
        .resolved_session_id
        .as_deref()
        .and_then(|id| state.db.provenance_session(id).ok().flatten());
    let mut message = v1::ConflictResolution {
        assignment_id: row.id.clone(),
        conflicted_commit_id: row.commit_id.clone(),
        resolved_commit_id: row.closed_commit_id.clone().unwrap_or_default(),
        attempts: row.attempts,
        attempt_limit: super::tier::limit(row),
        approval: row.approval,
        resolved_session_id: row.resolved_session_id.clone().unwrap_or_default(),
        resolved_agent: session
            .as_ref()
            .and_then(|s| s.agent.clone())
            .unwrap_or_default(),
        resolved_actor: session.map(|s| s.actor).unwrap_or_default(),
        resolved_at: row.closed_at.map(time),
        ..v1::ConflictResolution::default()
    };
    message.set_tier(tier_value(row.tier.tier.as_deref()));
    message
}

/// Identity names by id, read once per response.
#[derive(Debug, Default)]
pub struct Names(HashMap<String, Option<String>>);

impl Names {
    fn name(&mut self, state: &AppState, id: Option<&str>) -> String {
        let Some(id) = id else {
            return String::new();
        };
        self.0
            .entry(id.to_owned())
            .or_insert_with(|| state.db.identity(id).ok().flatten().map(|row| row.name))
            .clone()
            .unwrap_or_default()
    }
}

/// `owner` as a message; `None` when it names nobody. `live` is whether
/// the assignment treats it as live now.
fn owner_message(
    state: &AppState,
    names: &mut Names,
    owner: &OwnerColumns,
    live: bool,
) -> Option<v1::ConflictOwner> {
    if owner.is_empty() {
        return None;
    }
    Some(v1::ConflictOwner {
        session_id: owner.session_id.clone().unwrap_or_default(),
        workspace_id: owner.workspace_id.clone().unwrap_or_default(),
        workspace_owner: owner.workspace_owner.clone().unwrap_or_default(),
        identity_id: owner.identity_id.clone().unwrap_or_default(),
        identity_name: names.name(state, owner.identity_id.as_deref()),
        actor: owner.actor.clone().unwrap_or_default(),
        agent: owner.agent.clone().unwrap_or_default(),
        client_session: owner.client_session.clone().unwrap_or_default(),
        live,
        ended_at: owner.ended_at.map(time),
    })
}

/// `row` as a message, with `queue_position` (0 when not given).
pub fn assignment_message(
    state: &AppState,
    names: &mut Names,
    row: &AssignmentRow,
    queue_position: u32,
) -> v1::ConflictAssignment {
    let assigned = row.state == State::Assigned.as_str();
    let live = assigned && row.owner.session_id.is_some() && row.owner.ended_at.is_none();
    let grace = super::grace_millis(state.assignments.grace());
    let queue_after = row
        .owner
        .ended_at
        .filter(|_| assigned)
        .map(|ended| time(Timestamp(ended.0.saturating_add(grace))));
    let mut message = v1::ConflictAssignment {
        id: row.id.clone(),
        change_id: row.change_id.clone(),
        commit_id: row.commit_id.clone(),
        conflict_id: row.conflict_id.clone(),
        first_conflict_id: row.first_conflict_id.clone(),
        introduced: row.introduced,
        files_total: row.files_total,
        owner: owner_message(state, names, &row.owner, live),
        priority: row.priority,
        queued_at: row.queued_at.map(time),
        queue_position,
        queue_after,
        claims: row.claims,
        events: row.events,
        opened_at: Some(time(row.opened_at)),
        updated_at: Some(time(row.updated_at)),
        conflict_changed_at: Some(time(row.conflict_changed_at)),
        closed_at: row.closed_at.map(time),
        closed_commit_id: row.closed_commit_id.clone().unwrap_or_default(),
        resolved_session_id: row.resolved_session_id.clone().unwrap_or_default(),
        tier: tier_message(&row.tier),
        attempts: row.attempts,
        attempt_limit: super::tier::limit(row),
        go_ahead_identity_id: row.go_ahead_identity_id.clone().unwrap_or_default(),
        go_ahead_at: row.go_ahead_at.map(time),
        approval: row.approval,
        ..v1::ConflictAssignment::default()
    };
    message.set_kind(if row.kind == KIND_TREE {
        v1::ConflictKind::Tree
    } else {
        v1::ConflictKind::Unspecified
    });
    message.set_state(state_message(&row.state));
    message.set_reason(reason_message(&row.reason));
    message
}

/// `event` as a message.
pub fn event_message(
    state: &AppState,
    names: &mut Names,
    event: &AssignmentEventRow,
) -> v1::ConflictAssignmentEvent {
    let live = event.to_state == State::Assigned.as_str() && event.owner.ended_at.is_none();
    let mut message = v1::ConflictAssignmentEvent {
        id: event.id.clone(),
        seq: u64::try_from(event.seq).unwrap_or(0),
        assignment_id: event.assignment_id.clone(),
        change_id: event.change_id.clone(),
        conflict_id: event.conflict_id.clone(),
        owner: owner_message(state, names, &event.owner, live),
        priority: event.priority,
        note: event.note.clone().unwrap_or_default(),
        by_agent: event.by_agent.clone().unwrap_or_default(),
        by_identity_id: event.by_identity_id.clone().unwrap_or_default(),
        by_session_id: event.by_session_id.clone().unwrap_or_default(),
        by_remote_token: event.by_remote_token.clone().unwrap_or_default(),
        at: Some(time(event.at)),
        from_workspace_id: event.from_workspace_id.clone().unwrap_or_default(),
        ..v1::ConflictAssignmentEvent::default()
    };
    if let Some(from) = &event.from_state {
        message.set_from_state(state_message(from));
    }
    message.set_to_state(state_message(&event.to_state));
    message.set_reason(reason_message(&event.reason));
    message.set_by_actor(crate::lifecycle::messages::actor_message(&event.by_actor));
    message
}

/// The events an operation announced, as messages: what its attribute
/// carries.
pub fn operation_events(entry: &OpEntry) -> Vec<v1::ConflictAssignmentEvent> {
    reef_lib::jj::assignment::notices_of(&entry.attributes)
        .iter()
        .map(notice_message)
        .collect()
}

fn notice_message(notice: &AssignmentNotice) -> v1::ConflictAssignmentEvent {
    let owner =
        (!notice.workspace.is_empty() || !notice.agent.is_empty()).then(|| v1::ConflictOwner {
            workspace_id: notice.workspace.clone(),
            agent: notice.agent.clone(),
            ..v1::ConflictOwner::default()
        });
    let mut message = v1::ConflictAssignmentEvent {
        id: notice.id.clone(),
        seq: notice.seq,
        assignment_id: notice.assignment.clone(),
        change_id: notice.change.clone(),
        conflict_id: notice.conflict.clone(),
        owner,
        priority: notice.priority,
        from_workspace_id: notice.from_workspace.clone(),
        ..v1::ConflictAssignmentEvent::default()
    };
    if !notice.from.is_empty() {
        message.set_from_state(state_message(&notice.from));
    }
    message.set_to_state(state_message(&notice.to));
    message.set_reason(reason_message(&notice.reason));
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_and_reason_has_a_message() {
        for text in crate::db::OPEN_ASSIGNMENT_STATES.iter().chain(&["closed"]) {
            assert_ne!(
                state_message(text),
                v1::ConflictAssignmentState::Unspecified
            );
            assert_eq!(
                state_of(state_message(text).into()).map(State::as_str),
                Some(*text)
            );
        }
        for reason in Reason::ALL {
            assert_ne!(
                reason_message(reason.as_str()),
                v1::ConflictAssignmentReason::Unspecified
            );
        }
    }
}
