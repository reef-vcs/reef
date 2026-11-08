//! A job's presence entry (R3.05): kind `JOB`, reported by the daemon only,
//! one per open job, keyed [`DaemonKey::Job`]. It appears at submission,
//! changes at each claim, progress report and requeue, and at a terminal
//! transition carries the final state and then ends (`CLEARED`), so the
//! ended event says how the job ended. Its fixed fields are the
//! requester's agent, the job's repository, no workspace, and the job's
//! change. It lapses 30 seconds after the job's own next due time, which
//! the queue's transitions always reach first.

use std::time::Duration;

use reef_api::v1::{self, PresenceEndReason, PresenceKind};

use super::due_at;
use crate::db::{JobRow, JobState};
use crate::model::Timestamp;
use crate::presence::{DaemonKey, PresenceHub, Report};

/// How long after the job's next due time its entry lapses.
const TTL_SLACK: Duration = Duration::from_secs(30);

/// The agent a job's entry names when its requester sent none.
const NO_AGENT: &str = "reef";

/// `JobState` in the API.
pub(crate) fn state_message(state: JobState) -> v1::JobState {
    match state {
        JobState::Queued => v1::JobState::Queued,
        JobState::Running => v1::JobState::Running,
        JobState::Completed => v1::JobState::Completed,
        JobState::Cancelled => v1::JobState::Cancelled,
        JobState::Expired => v1::JobState::Expired,
    }
}

/// A job's outcome column in the API.
pub(crate) fn outcome_message(outcome: Option<&str>) -> v1::JobOutcome {
    match outcome {
        Some("done") => v1::JobOutcome::Done,
        Some("budget_exhausted") => v1::JobOutcome::BudgetExhausted,
        Some("failed") => v1::JobOutcome::Failed,
        _ => v1::JobOutcome::Unspecified,
    }
}

/// A job's end reason column in the API.
pub(crate) fn end_reason_message(reason: Option<&str>) -> v1::JobEndReason {
    match reason {
        Some("queue_timeout") => v1::JobEndReason::QueueTimeout,
        Some("attempts") => v1::JobEndReason::Attempts,
        Some("deadline") => v1::JobEndReason::Deadline,
        _ => v1::JobEndReason::Unspecified,
    }
}

/// A job's progress column in the API; `None` when unset or unreadable.
pub(crate) fn progress_message(progress: Option<&str>) -> Option<v1::JobProgress> {
    let value: serde_json::Value = serde_json::from_str(progress?).ok()?;
    Some(v1::JobProgress {
        phase: value["phase"].as_str().unwrap_or_default().to_owned(),
        done: value["done"].as_u64().unwrap_or_default(),
        total: value["total"].as_u64().unwrap_or_default(),
        unit: value["unit"].as_str().unwrap_or_default().to_owned(),
    })
}

/// The entry's job part.
fn job_presence(job: &JobRow) -> v1::JobPresence {
    v1::JobPresence {
        job_id: job.id.clone(),
        state: state_message(job.state).into(),
        outcome: outcome_message(job.outcome.as_deref()).into(),
        end_reason: end_reason_message(job.end_reason.as_deref()).into(),
        attempt: job.attempt,
        executor: job
            .executor
            .as_ref()
            .map(|executor| executor.name.clone())
            .unwrap_or_default(),
        kinds: job.kinds.clone(),
        tree_id: job.tree_id.clone(),
        progress: progress_message(job.progress.as_deref()),
    }
}

/// Publishes `job`'s entry as of `now`, and ends it when the job finished.
pub(super) fn publish(hub: &PresenceHub, job: &JobRow, now: Timestamp) {
    let key = DaemonKey::Job(super::presence_key(&job.id));
    let until_due = due_at(job).map_or(Duration::ZERO, |at| {
        Duration::from_millis(u64::try_from(at.0 - now.0).unwrap_or(0))
    });
    let report = Report {
        agent: job
            .requested_by_agent
            .clone()
            .unwrap_or_else(|| NO_AGENT.to_owned()),
        repo: Some(job.repo_id),
        workspace: None,
        change_id: job.change_id.clone().unwrap_or_default(),
        kind: PresenceKind::Job,
        job: Some(job_presence(job)),
        ttl: until_due + TTL_SLACK,
        ..Report::default()
    };
    hub.publish_daemon(key, |_| report);
    if !job.state.is_open() {
        hub.end_daemon(key, PresenceEndReason::Cleared);
    }
}
