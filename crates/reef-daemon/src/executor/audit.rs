//! The executor queue's audit log (R3.05): every submission and every
//! transition, under the target `reef_daemon::executor::audit`. A claim
//! token is never logged.

use crate::db::{JobRow, JobState};

/// A job was queued.
pub(super) fn submitted(job: &JobRow) {
    tracing::info!(
        target: "reef_daemon::executor::audit",
        job_id = %job.id,
        repo = %job.repo_id,
        tree_id = %job.tree_id,
        kinds = %job.kinds.join(","),
        target_count = job.targets.len(),
        requested_by_agent = job.requested_by_agent.as_deref().unwrap_or(""),
        remote_token_id = job.requested_by_token.as_deref().unwrap_or(""),
        "job submitted"
    );
}

/// `job` moved from `before` to its state.
pub(super) fn transition(before: JobState, job: &JobRow) {
    if before == job.state && job.state == JobState::Running {
        // A heartbeat or a progress report: not an audit event.
        return;
    }
    let executor = job.executor.as_ref();
    tracing::info!(
        target: "reef_daemon::executor::audit",
        job_id = %job.id,
        repo = %job.repo_id,
        from = before.as_str(),
        to = job.state.as_str(),
        attempt = job.attempt,
        outcome = job.outcome.as_deref().unwrap_or(""),
        end_reason = job.end_reason.as_deref().unwrap_or(""),
        requested_by_agent = job.requested_by_agent.as_deref().unwrap_or(""),
        remote_token_id = job.requested_by_token.as_deref().unwrap_or(""),
        cancelled_by_agent = job.cancelled_by_agent.as_deref().unwrap_or(""),
        executor = executor.map_or("", |e| e.name.as_str()),
        executor_token = executor.and_then(|e| e.token.as_deref()).unwrap_or(""),
        executor_identity_id = executor.and_then(|e| e.identity_id.as_deref()).unwrap_or(""),
        "job transition"
    );
}
