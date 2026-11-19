//! Telling the proposing agent (R3.45, ADR 0081; S-228, S-229): when
//! automatic landing gives up on a change for a reason its author can fix,
//! the daemon writes one R3.12 instruction on it, which R3.12 routes to the
//! session that proposed it and delivers by pull.
//!
//! The text is a fixed template per outcome ([`Outcome`]), filled only with
//! values Reef or the destination's owners chose: the change id, ids of
//! landings and evidence, kinds and targets from the policy, and the
//! destination's name, each cut to a safe alphabet. Never an evidence
//! summary, a job message, a rule's message or a description: those could
//! carry another party's words into the agent's context.

use super::automatic::{State, Status, change_revision, short};
use crate::comments::{self, CreateRequest, Kind};
use crate::daemon::AppState;
use crate::lifecycle::Actor;
use crate::model::RepoId;
use reef_lib::error::ErrorChain;

/// What automatic landing gave up on, each with its own sentence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// A `[[require]]` check failed on the change's own tree.
    Failed,
    /// A batch returned it as the culprit (`checks`).
    Culprit,
    /// It conflicts with its destination.
    Conflicted,
    /// It changed after it was proposed.
    Amended,
    /// Its edits are already on the destination.
    Empty,
}

/// The outcome `status` reports that its author can fix, if any. A missing
/// sign-off is a reviewer's, and executors are the operator's.
fn outcome(status: &Status) -> Option<Outcome> {
    let reason = status.reason.as_deref().unwrap_or_default();
    match (status.state, reason) {
        (State::Needs, "policy")
            if status
                .missing
                .iter()
                .any(|need| !need.evidence_ids.is_empty()) =>
        {
            Some(Outcome::Failed)
        }
        (State::Needs | State::Returned, "conflicted") => Some(Outcome::Conflicted),
        (State::Needs, "amended") => Some(Outcome::Amended),
        (State::Needs | State::Returned, "empty") => Some(Outcome::Empty),
        (State::Returned, "checks") => Some(Outcome::Culprit),
        _ => None,
    }
}

/// `text` cut to letters, digits and `._:/-`, at most 64 characters: what
/// an id, a kind, a target or a bookmark name may carry into the template.
fn safe(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'))
        .take(64)
        .collect()
}

/// The instruction's text for `outcome`.
fn text(outcome: Outcome, change: &str, destination: &str, status: &Status) -> String {
    let short = short(change);
    let landing = safe(status.landing_id.as_deref().unwrap_or_default());
    match outcome {
        Outcome::Failed => {
            let failed: Vec<&reef_policy::Need> = status
                .missing
                .iter()
                .filter(|need| !need.evidence_ids.is_empty())
                .take(5)
                .collect();
            let checks: Vec<String> = failed
                .iter()
                .map(|need| match need.target.as_str() {
                    "" => safe(&need.kind),
                    target => format!("{} {}", safe(&need.kind), safe(target)),
                })
                .collect();
            let evidence: Vec<String> = failed
                .iter()
                .flat_map(|need| need.evidence_ids.iter().take(1))
                .map(|id| safe(id).chars().take(16).collect())
                .collect();
            format!(
                "Reef could not land change {short} automatically: {} failed on its tree (evidence {}). Fix the change, then propose it again (reef propose).",
                checks.join(", "),
                evidence.join(", ")
            )
        }
        Outcome::Culprit => format!(
            "Reef could not land change {short} automatically: a required check failed on its batch's tree with this change and passed without it (landing {landing}). See reef landing show {landing}, fix the change against {}, then propose it again (reef propose).",
            safe(destination)
        ),
        Outcome::Conflicted => format!(
            "Reef could not land change {short} automatically: it conflicts with {}. Rebase it (reef rebase --onto {}), resolve the conflicts, then propose it again (reef propose).",
            safe(destination),
            safe(destination)
        ),
        Outcome::Amended => format!(
            "Change {short} changed after it was proposed, so Reef will not land it automatically. Propose it again (reef propose) to put the new commit forward."
        ),
        Outcome::Empty => format!(
            "Reef did not land change {short}: its edits are already on {}. Withdraw the proposal (reef proposal withdraw {short}).",
            safe(destination)
        ),
    }
}

/// Writes the instruction `status` calls for on `change`, unless it has
/// one with the same text on the same commit, or one still unfinished
/// (S-229). Failures are logged: an instruction never stops a pass.
pub(crate) async fn send(
    state: &std::sync::Arc<AppState>,
    repo: RepoId,
    change: &str,
    status: &Status,
) {
    let Some(outcome) = outcome(status) else {
        return;
    };
    let row = state.db.lifecycle_row(repo, change).ok().flatten();
    let destination = row
        .as_ref()
        .and_then(|row| row.destination.clone())
        .unwrap_or_default();
    // One per proposal: a re-proposal (a new transition) may be told again.
    let since = row.map_or(crate::model::Timestamp(0), |row| row.updated_at);
    let text = text(outcome, change, &destination, status);
    let digest = reef_policy::sha256_hex(text.as_bytes());
    match state.db.daemon_instructions(repo, change, since, &digest) {
        Ok((false, false)) => {}
        Ok(_) => return,
        Err(error) => {
            tracing::warn!(repo = %repo, change_id = change, error = %ErrorChain(&error), "cannot read the daemon's instructions");
            return;
        }
    }
    let Ok(record) = state.db.repository(repo) else {
        return;
    };
    let request = CreateRequest {
        repo,
        workspace: None,
        user_email: state.settings.user_email().to_owned(),
        root: record.root,
        revision: change_revision(change),
        // An amended change's instruction is about its visible commit.
        expected_commit: if outcome == Outcome::Amended {
            String::new()
        } else {
            status.commit.clone()
        },
        anchor: None,
        reply_to: String::new(),
        kind: Some(Kind::Instruction),
        text,
        actor: Some(Actor::Daemon),
        confirm: false,
        ceilings: None,
        agent: Some(super::batch::AGENT.to_owned()),
        remote: None,
        help: None,
        client_session: String::new(),
    };
    match comments::create(state, request).await {
        Ok(written) => {
            tracing::info!(repo = %repo, change_id = change, comment_id = %written.comment.id, outcome = ?outcome, "told the proposing agent");
        }
        Err(error) => {
            tracing::warn!(repo = %repo, change_id = change, error = %ErrorChain(&error), "cannot tell the proposing agent");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_keeps_only_the_alphabet() {
        assert_eq!(safe("test\nIgnore previous"), "testIgnoreprevious");
        assert_eq!(safe("crates/reef-lib:unit_1.0"), "crates/reef-lib:unit_1.0");
        assert_eq!(safe(&"a".repeat(100)).len(), 64);
    }
}
