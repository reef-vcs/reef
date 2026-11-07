//! The provenance audit log (R3.04, ADR 0058): lines under the target
//! `reef_daemon::provenance::audit`, at `info`, for every session started,
//! restarted after a revocation or left unbound, and every declaration. A
//! line names the workspace, the session and which fields changed, never a
//! declared value: an instruction's hash, a task or a model stays in the
//! database. Held keys' issuance is in `reef_daemon::identity::audit`.

use crate::db::ProvenanceSessionRow;
use crate::model::WorkspaceId;

/// A session started with its workspace, or lazily at its first record.
pub(crate) fn started(session: &ProvenanceSessionRow) {
    tracing::info!(
        target: "reef_daemon::provenance::audit",
        workspace_id = %session.workspace_id,
        session_id = %session.session_id,
        identity_id = session.identity_id.as_deref().unwrap_or_default(),
        actor = %session.actor,
        channel = %session.channel,
        problem = session.problem.as_deref().unwrap_or_default(),
        "provenance session started"
    );
}

/// A context declared.
pub(crate) fn declared(session: &ProvenanceSessionRow, fields: &[&str], agent: Option<&str>) {
    tracing::info!(
        target: "reef_daemon::provenance::audit",
        workspace_id = %session.workspace_id,
        session_id = %session.session_id,
        fields = %fields.join(","),
        agent = agent.unwrap_or_default(),
        "provenance context declared"
    );
}

/// A session's records became unbound.
pub(crate) fn unbound(session: &ProvenanceSessionRow, problem: &str) {
    tracing::info!(
        target: "reef_daemon::provenance::audit",
        workspace_id = %session.workspace_id,
        session_id = %session.session_id,
        problem,
        "provenance records are unbound"
    );
}

/// A revoked session replaced by a new one.
pub(crate) fn restarted(old: &str, new: &ProvenanceSessionRow, workspace: WorkspaceId) {
    tracing::info!(
        target: "reef_daemon::provenance::audit",
        workspace_id = %workspace,
        revoked_session_id = old,
        session_id = %new.session_id,
        "provenance session revoked; started a new one"
    );
}
