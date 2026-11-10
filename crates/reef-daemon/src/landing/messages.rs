//! Landings as `reef.api.v1` messages (R3.13).

use reef_api::v1;

use super::{Outcome, Reason};
use crate::db::{BatchTrialRow, LandingChangeRow, StoredBatch, StoredLanding};
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
        Some(Reason::Checks) => v1::LandingReason::Checks,
        Some(Reason::Unavailable) => v1::LandingReason::Unavailable,
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
    let evaluation_of = |json: Option<&str>, tree: Option<&String>| {
        json.and_then(|json| serde_json::from_str::<reef_policy::Evaluation>(json).ok())
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
                message.tree_id = tree.cloned().unwrap_or_default();
                message
            })
    };
    let evaluation = evaluation_of(change.evaluation.as_deref(), change.queued_tree.as_ref());
    let tested_evaluation = evaluation_of(
        change.tested_evaluation.as_deref(),
        change.tested_tree.as_ref(),
    );
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
        tested_tree_id: change.tested_tree.clone().unwrap_or_default(),
        tested_commit_id: change.tested_commit.clone().unwrap_or_default(),
        tested_evaluation,
        batch_position: change.batch_position.unwrap_or_default(),
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
        batch_id: row.batch_id.clone().unwrap_or_default(),
    }
}

/// A batch state as the v1 enum.
fn batch_state_message(text: &str) -> v1::BatchState {
    match text {
        "testing" => v1::BatchState::Testing,
        "bisecting" => v1::BatchState::Bisecting,
        "finished" => v1::BatchState::Finished,
        _ => v1::BatchState::Unspecified,
    }
}

/// A batch outcome as the v1 enum.
fn batch_outcome_message(text: Option<&str>) -> v1::BatchOutcome {
    match text {
        Some("landed") => v1::BatchOutcome::Landed,
        Some("split") => v1::BatchOutcome::Split,
        Some("unavailable") => v1::BatchOutcome::Unavailable,
        Some("reformed") => v1::BatchOutcome::Reformed,
        Some("interrupted") => v1::BatchOutcome::Interrupted,
        Some("busy") => v1::BatchOutcome::Busy,
        _ => v1::BatchOutcome::Unspecified,
    }
}

/// A member result as the v1 enum.
fn member_result_message(text: Option<&str>) -> v1::BatchMemberResult {
    match text {
        Some("landed") => v1::BatchMemberResult::Landed,
        Some("culprit") => v1::BatchMemberResult::Culprit,
        Some("requeued") => v1::BatchMemberResult::Requeued,
        Some("dequeued") => v1::BatchMemberResult::Dequeued,
        Some("left") => v1::BatchMemberResult::Left,
        Some("unavailable") => v1::BatchMemberResult::Unavailable,
        _ => v1::BatchMemberResult::Unspecified,
    }
}

/// A trial result as the v1 enum.
fn trial_result_message(text: Option<&str>) -> v1::TrialResult {
    match text {
        Some("passed") => v1::TrialResult::Passed,
        Some("failed") => v1::TrialResult::Failed,
        Some("unavailable") => v1::TrialResult::Unavailable,
        Some("cancelled") => v1::TrialResult::Cancelled,
        _ => v1::TrialResult::Unspecified,
    }
}

/// A trial's needs read back: the list and the total.
fn trial_needs(trial: &BatchTrialRow) -> (Vec<v1::PolicyEvidenceNeed>, u32) {
    let Some(value) = trial
        .needs
        .as_deref()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
    else {
        return (Vec::new(), 0);
    };
    let needs: Vec<reef_policy::Need> =
        serde_json::from_value(value["needs"].clone()).unwrap_or_default();
    let total = value["total"]
        .as_u64()
        .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX));
    (
        needs
            .iter()
            .map(crate::service::policy::need_message)
            .collect(),
        total,
    )
}

/// A batch as the v1 message (R3.14).
pub fn batch_message(batch: &StoredBatch) -> v1::LandingBatch {
    let row = &batch.row;
    v1::LandingBatch {
        id: row.id.clone(),
        repo_id: row.repo_id.as_hex(),
        destination: row.destination.clone(),
        base_commit_id: row.base_commit.clone(),
        state: batch_state_message(&row.state).into(),
        outcome: batch_outcome_message(row.outcome.as_deref()).into(),
        members: batch
            .members
            .iter()
            .map(|member| v1::BatchMember {
                position: member.position,
                landing_id: member.landing_id.clone(),
                change_id: member.change_id.clone(),
                queued_commit_id: member.queued_commit.clone(),
                planned_tree_id: member.planned_tree.clone(),
                result: member_result_message(member.result.as_deref()).into(),
            })
            .collect(),
        trials: batch
            .trials
            .iter()
            .map(|trial| {
                let (needs, needs_total) = trial_needs(trial);
                v1::BatchTrial {
                    prefix: trial.prefix,
                    tree_id: trial.tree.clone(),
                    commit_id: trial.commit_id.clone().unwrap_or_default(),
                    operation_id: trial.operation_id.clone().unwrap_or_default(),
                    result: trial_result_message(trial.result.as_deref()).into(),
                    job_ids: trial.job_ids.clone(),
                    needs,
                    needs_total,
                    attempts: trial.attempts,
                    created_at: Some(timestamp(trial.created_at)),
                    finished_at: trial.finished_at.map(timestamp),
                }
            })
            .collect(),
        culprit_position: row.culprit_position.unwrap_or_default(),
        landed_count: row.landed_count,
        checks: row.checks.clone(),
        policy_commit_id: row.policy_commit.clone().unwrap_or_default(),
        operation_id: row.operation_id.clone().unwrap_or_default(),
        operation_collected: row.operation_collected,
        message: row.message.clone().unwrap_or_default(),
        created_at: Some(timestamp(row.created_at)),
        finished_at: row.finished_at.map(timestamp),
    }
}
