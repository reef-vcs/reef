//! The daemon's sentence for each item: the same words in the web UI,
//! `reef inbox`, and R3.39's and R3.40's notifications. Never agent text,
//! which stays in the record (S-262), except names: agents and attestor
//! sets.

use reef_api::v1;

use super::{Reason, Timestamp, UNCLAIMED_AFTER_MS, time};

fn agent_or(agent: &str, fallback: &'static str) -> String {
    if agent.is_empty() {
        fallback.to_owned()
    } else {
        agent.to_owned()
    }
}

fn list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

pub(super) fn proposal(reason: Reason, entry: &v1::ProposalEntry) -> String {
    let status = entry.automatic.as_ref();
    match reason {
        Reason::Signoff => {
            let mut sets: Vec<String> = status
                .map(|status| {
                    status
                        .missing
                        .iter()
                        .flat_map(|need| need.attestors.iter().cloned())
                        .collect()
                })
                .unwrap_or_default();
            sets.sort();
            sets.dedup();
            if sets.is_empty() {
                "Needs a sign-off".to_owned()
            } else {
                format!("Needs a sign-off from {}", list(&sets))
            }
        }
        Reason::Returned => {
            let why = status
                .filter(|status| status.state() == v1::AutomaticState::Returned)
                .map(|status| status.reason.clone())
                .filter(|reason| !reason.is_empty())
                .or_else(|| {
                    entry
                        .lifecycle
                        .as_ref()
                        .and_then(|lifecycle| lifecycle.last.as_ref())
                        .map(|last| super::rules::landing_reason(&last.reason).to_owned())
                        .filter(|reason| !reason.is_empty())
                });
            match why.as_deref().and_then(crate::landing::Reason::parse) {
                Some(reason) => format!("Came back from landing: {}", landing_words(reason)),
                None => "Came back from landing".to_owned(),
            }
        }
        Reason::NoExecutor => "Waiting for an executor: no executor claims its checks".to_owned(),
        _ => in_review(entry),
    }
}

/// Why a landing came back, as a person reads it; every reason has words.
pub(super) fn landing_words(reason: crate::landing::Reason) -> &'static str {
    use crate::landing::Reason as R;
    match reason {
        R::Policy => "the policy blocks it",
        R::Conflicted => "it has conflicts, or conflicts with the destination",
        R::UnlandedParents => "a change below it did not land",
        R::Amended => "it changed after it was proposed",
        R::Destination => "its destination is gone, conflicted, or has it already",
        R::Busy => "the destination kept moving; land it again",
        R::Failed => "the landing failed",
        R::Checks => "a check failed",
        R::Unavailable => "no executor gave a verdict",
        R::Empty => "its edits are already there",
        R::Alone => "it changes a policy file and lands on its own",
        R::Dependency => "a change below it left review",
    }
}

/// A proposal in watching: where the automatic service has it.
fn in_review(entry: &v1::ProposalEntry) -> String {
    let queued = entry
        .lifecycle
        .as_ref()
        .is_some_and(|lifecycle| lifecycle.state() == v1::LifecycleState::Queued);
    let Some(status) = entry.automatic.as_ref() else {
        return if queued {
            "Queued for landing"
        } else {
            "In review"
        }
        .to_owned();
    };
    match status.state() {
        v1::AutomaticState::Off | v1::AutomaticState::Unspecified => {
            if queued {
                "Queued for landing".to_owned()
            } else {
                "In review; no policy judges it here".to_owned()
            }
        }
        v1::AutomaticState::Pending => "Not judged yet".to_owned(),
        v1::AutomaticState::Waiting => "Waiting for its checks".to_owned(),
        v1::AutomaticState::Ready => "Ready to land".to_owned(),
        v1::AutomaticState::Queued => "Queued for landing".to_owned(),
        v1::AutomaticState::Needs => {
            if status.message.is_empty() {
                "Blocked: its agent has something to fix".to_owned()
            } else {
                format!("Blocked: {}", status.message)
            }
        }
        v1::AutomaticState::NoExecutor => "Waiting for an executor".to_owned(),
        v1::AutomaticState::Returned => "Came back from landing".to_owned(),
    }
}

pub(super) fn comment(reason: Reason, comment: &v1::Comment, now: Timestamp) -> String {
    let agent = comment
        .author
        .as_ref()
        .map(|author| author.agent.as_str())
        .unwrap_or_default();
    match reason {
        Reason::Help => {
            let why = comment
                .help
                .as_ref()
                .map_or(v1::HelpReason::Other, v1::HelpRequest::reason);
            let about = match why {
                v1::HelpReason::Decision => "a decision",
                v1::HelpReason::Conflict => "a conflict",
                v1::HelpReason::Blocked => "being blocked",
                v1::HelpReason::Unclear => "an unclear task",
                v1::HelpReason::Other | v1::HelpReason::Unspecified => "",
            };
            let who = agent_or(agent, "An agent");
            if about.is_empty() {
                format!("{who} asks for help")
            } else {
                format!("{who} asks for help with {about}")
            }
        }
        Reason::Confirm => "Your instruction waits for its Confirm".to_owned(),
        _ => {
            let Some(instruction) = comment.instruction.as_ref() else {
                return "Instruction".to_owned();
            };
            let target = instruction.target.as_ref();
            let to = target
                .map(|target| target.agent.as_str())
                .filter(|agent| !agent.is_empty())
                .map_or_else(|| "its agent".to_owned(), str::to_owned);
            let stale = target
                .and_then(|target| target.last_seen_at.as_ref())
                .is_some_and(|seen| now.0 - time(Some(seen)).0 > UNCLAIMED_AFTER_MS);
            match instruction.state() {
                v1::InstructionState::Pending if stale => {
                    format!("Instruction for {to}: nobody has picked it up")
                }
                v1::InstructionState::Pending => format!("Instruction waiting for {to}"),
                v1::InstructionState::Delivered => format!("Instruction delivered to {to}"),
                v1::InstructionState::Running => format!(
                    "{} is working on an instruction",
                    agent_or(&instruction.claimed_agent, "An agent")
                ),
                _ => "Instruction".to_owned(),
            }
        }
    }
}

pub(super) fn conflict(reason: Reason, assignment: &v1::ConflictAssignment) -> String {
    if reason == Reason::ConflictUnowned {
        return "A conflict nobody owns".to_owned();
    }
    let ask = assignment
        .tier
        .as_ref()
        .is_some_and(|tier| tier.tier() == v1::ConflictTier::Ask);
    let spent = assignment.attempt_limit > 0 && assignment.attempts >= assignment.attempt_limit;
    if spent {
        "A conflict whose resolution attempts are spent".to_owned()
    } else if ask {
        "A conflict waiting for your go-ahead".to_owned()
    } else {
        "A conflict waiting for a person".to_owned()
    }
}

/// R2.20's attention meanings, as sentences.
pub(super) fn workspace(workspace: &v1::Workspace) -> String {
    let what = match workspace.attention() {
        v1::Attention::Divergent => "its change has more than one visible commit",
        v1::Attention::StaleRecovered => "its working copy was stale and was recovered",
        v1::Attention::FinalSnapshotFailed => "its final snapshot failed; it was kept",
        v1::Attention::StoreHashMismatch => "its lockfile no longer matches a mounted store",
        v1::Attention::StoreMissing => "a store it declares had no build",
        v1::Attention::WatchFailed => "its file watch could not be attached",
        v1::Attention::UpperMissing => "its overlay's upper directory is gone",
        v1::Attention::MountFailed => "its overlay could not be mounted",
        v1::Attention::Unspecified => "it needs attention",
    };
    format!(
        "Workspace {}: {what}",
        agent_or(&workspace.owner, "with no owner")
    )
}
