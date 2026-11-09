//! Landings as `reef.api.v1` messages (R3.13).

use reef_api::v1;

use super::{Outcome, Reason};
use crate::db::{LandingChangeRow, StoredLanding};
use crate::service::convert::timestamp;

/// An outcome as the v1 enum.
pub fn outcome_message(text: &str) -> v1::LandingOutcome {
    match Outcome::parse(text) {
        Some(Outcome::Blocked) => v1::LandingOutcome::Blocked,
        Some(Outcome::Queued) => v1::LandingOutcome::Queued,
        Some(Outcome::Landed) => v1::LandingOutcome::Landed,
        Some(Outcome::Dequeued) => v1::LandingOutcome::Dequeued,
        Some(Outcome::Cancelled) => v1::LandingOutcome::Cancelled,
        None => v1::LandingOutcome::Unspecified,
    }
}

/// The outcome's text a v1 value names; `None` for UNSPECIFIED or an
/// unknown value.
pub fn outcome_of(value: i32) -> Option<&'static str> {
    Some(match v1::LandingOutcome::try_from(value).ok()? {
        v1::LandingOutcome::Unspecified => return None,
        v1::LandingOutcome::Blocked => Outcome::Blocked.as_str(),
        v1::LandingOutcome::Queued => Outcome::Queued.as_str(),
        v1::LandingOutcome::Landed => Outcome::Landed.as_str(),
        v1::LandingOutcome::Dequeued => Outcome::Dequeued.as_str(),
        v1::LandingOutcome::Cancelled => Outcome::Cancelled.as_str(),
    })
}

/// A reason as the v1 enum.
pub fn reason_message(text: Option<&str>) -> v1::LandingReason {
    match text.and_then(Reason::parse) {
        Some(Reason::Policy) => v1::LandingReason::Policy,
        Some(Reason::Conflicted) => v1::LandingReason::Conflicted,
        Some(Reason::UnlandedParents) => v1::LandingReason::UnlandedParents,
        Some(Reason::Amended) => v1::LandingReason::Amended,
        Some(Reason::Destination) => v1::LandingReason::Destination,
        Some(Reason::Busy) => v1::LandingReason::Busy,
        Some(Reason::Failed) => v1::LandingReason::Failed,
        None => v1::LandingReason::Unspecified,
    }
}

/// `{"paths": [...], "total": n}` read back.
fn paths(json: Option<&str>) -> (Vec<String>, u32) {
    let Some(value) = json.and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
    else {
        return (Vec::new(), 0);
    };
    let list = value["paths"]
        .as_array()
        .map(|paths| {
            paths
                .iter()
                .filter_map(|path| path.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let total = value["total"]
        .as_u64()
        .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX));
    (list, total)
}

/// One change of a landing; `whole` keeps every rule of its evaluation.
fn change_message(
    landing: &StoredLanding,
    change: &LandingChangeRow,
    whole: bool,
) -> v1::LandedChange {
    let (overlap, overlap_total) = paths(change.overlap.as_deref());
    let (conflicts, conflicts_total) = paths(change.conflicts.as_deref());
    let evaluation = change
        .evaluation
        .as_deref()
        .and_then(|json| serde_json::from_str::<reef_policy::Evaluation>(json).ok())
        .map(|evaluation| {
            let mut message = crate::service::policy::evaluation_message(&evaluation);
            if !whole {
                message.rules.clear();
                message.diagnostics.clear();
            }
            let row = &landing.row;
            message.source = Some(v1::PolicySource {
                revision: row.destination.clone(),
                commit_id: row.policy_commit.clone().unwrap_or_default(),
                path: reef_policy::POLICY_PATH.to_owned(),
                blob_id: row.policy_blob.clone().unwrap_or_default(),
                sha256: evaluation.sha256.clone(),
                version: evaluation.version,
                from_request: false,
            });
            message.change_id.clone_from(&change.change_id);
            message.commit_id.clone_from(&change.queued_commit);
            message.tree_id = change.queued_tree.clone().unwrap_or_default();
            message
        });
    v1::LandedChange {
        change_id: change.change_id.clone(),
        queued_commit_id: change.queued_commit.clone(),
        queued_tree_id: change.queued_tree.clone().unwrap_or_default(),
        landed_commit_id: change.landed_commit.clone().unwrap_or_default(),
        landed_tree_id: change.landed_tree.clone().unwrap_or_default(),
        rebased: change.rebased,
        overlap: overlap.into_iter().take(100).collect(),
        overlap_total,
        conflicts,
        conflicts_total,
        evaluation,
        pinned_evidence_ids: landing
            .pinned
            .iter()
            .filter(|(pinned, _)| *pinned == change.change_id)
            .map(|(_, id)| id.clone())
            .collect(),
    }
}

/// A landing as the v1 message; `whole` keeps every evaluation's rules.
pub fn landing_message(landing: &StoredLanding, whole: bool) -> v1::Landing {
    let row = &landing.row;
    v1::Landing {
        id: row.id.clone(),
        repo_id: row.repo_id.as_hex(),
        destination: row.destination.clone(),
        destination_before: row.destination_before.clone().unwrap_or_default(),
        destination_after: row.destination_after.clone().unwrap_or_default(),
        outcome: outcome_message(&row.outcome).into(),
        reason: reason_message(row.reason.as_deref()).into(),
        message: row.message.clone().unwrap_or_default(),
        changes: landing
            .changes
            .iter()
            .map(|change| change_message(landing, change, whole))
            .collect(),
        attempts: row.attempts,
        enqueue_operation_id: row.enqueue_operation_id.clone().unwrap_or_default(),
        operation_id: row.operation_id.clone().unwrap_or_default(),
        operation_collected: row.operation_collected,
        agent: row.agent.clone().unwrap_or_default(),
        actor: crate::lifecycle::messages::actor_message(row.actor.as_deref().unwrap_or_default())
            .into(),
        identity: row.identity_name.clone().unwrap_or_default(),
        remote: row.remote_token.as_ref().map(|token| v1::RemoteCaller {
            token_id: token.clone(),
            token_name: row.remote_token_name.clone().unwrap_or_default(),
        }),
        created_at: Some(timestamp(row.created_at)),
        finished_at: row.finished_at.map(timestamp),
    }
}
