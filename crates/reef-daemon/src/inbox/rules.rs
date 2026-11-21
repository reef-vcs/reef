//! The inbox's rules (R3.33, decision 4): one table per object, its rows in
//! precedence order. A record takes the first row that admits it, every
//! needs-you-now row of an object comes before its watching row, and so an
//! item is never in both buckets (a test checks the order).

use reef_api::v1;

#[cfg(test)]
use super::Bucket;
use super::Reason;

/// One object's rows: a reason and whether it admits the record.
pub(crate) type Rules<R> = &'static [(Reason, fn(&R) -> bool)];

/// The first reason of `rules` that admits `record`.
pub(crate) fn place<R>(rules: Rules<R>, record: &R) -> Option<Reason> {
    rules
        .iter()
        .find(|(_, admits)| admits(record))
        .map(|(reason, _)| *reason)
}

pub(crate) const PROPOSAL: Rules<v1::ProposalEntry> = &[
    (Reason::Signoff, only_signoffs_missing),
    (Reason::Returned, returned_unchanged),
    (Reason::NoExecutor, no_executor),
    (Reason::InReview, |_| true),
];

pub(crate) const HELP: Rules<v1::Comment> = &[(Reason::Help, open_help)];

pub(crate) const INSTRUCTION: Rules<v1::Comment> = &[
    (Reason::Confirm, awaits_persons_confirm),
    (Reason::InFlight, in_flight),
];

pub(crate) const CONFLICT: Rules<v1::ConflictAssignment> = &[
    (Reason::ConflictUnowned, unowned),
    (Reason::ConflictBlocked, blocked),
];

pub(crate) const WORKSPACE: Rules<v1::Workspace> = &[(Reason::Attention, needs_attention)];

fn automatic_state(entry: &v1::ProposalEntry) -> Option<v1::AutomaticState> {
    entry.automatic.as_ref().map(v1::AutomaticStatus::state)
}

/// The status is `NEEDS` and every missing need is a sign-off: what is left
/// is a person's. A failed check, a rule or a conflict beside it is the
/// agent's to fix first, and a sign-off on a tree that will change is
/// wasted. A cut list (`missing_total` over `missing`) is not admitted.
fn only_signoffs_missing(entry: &v1::ProposalEntry) -> bool {
    let Some(status) = entry.automatic.as_ref() else {
        return false;
    };
    status.state() == v1::AutomaticState::Needs
        && !status.missing.is_empty()
        && usize::try_from(status.missing_total).is_ok_and(|total| total == status.missing.len())
        && status
            .missing
            .iter()
            .all(|need| need.kind == crate::landing::automatic::SIGNOFF_NEED)
}

/// The reason of a landing the destination's moving head turned back.
const BUSY: &str = "busy";

/// The landing's reason in a returning transition's text, which reads
/// `<reason>: <message>` (`landing::worker`).
pub(super) fn landing_reason(text: &str) -> &str {
    text.split_once(':')
        .map_or(text, |(reason, _)| reason)
        .trim()
}

/// A landing returned it and it has not changed since: its last transition
/// is cause `LANDING` back to `PROPOSED` and it was not amended after, or
/// the automatic service holds it as `RETURNED` and will not queue it again
/// by itself (`retrying`, a busy destination it tries three times).
fn returned_unchanged(entry: &v1::ProposalEntry) -> bool {
    if let Some(status) = entry
        .automatic
        .as_ref()
        .filter(|status| status.state() == v1::AutomaticState::Returned)
    {
        return !status.retrying;
    }
    let Some(lifecycle) = entry.lifecycle.as_ref() else {
        return false;
    };
    let returned = lifecycle.last.as_ref().is_some_and(|last| {
        last.cause() == v1::TransitionCause::Landing && last.to() == v1::LifecycleState::Proposed
    });
    // A busy return the service queued, before its next pass sees it: the
    // status still says QUEUED, and the service will queue it again.
    let queued = automatic_state(entry) == Some(v1::AutomaticState::Queued);
    let busy = lifecycle
        .last
        .as_ref()
        .is_some_and(|last| landing_reason(&last.reason) == BUSY);
    returned
        && !(queued && busy)
        && !lifecycle.amended
        && lifecycle.state() == v1::LifecycleState::Proposed
}

fn no_executor(entry: &v1::ProposalEntry) -> bool {
    automatic_state(entry) == Some(v1::AutomaticState::NoExecutor)
}

fn open_help(comment: &v1::Comment) -> bool {
    comment
        .help
        .as_ref()
        .is_some_and(|help| help.state() == v1::HelpState::Open)
}

/// An instruction a person wrote that waits for its Confirm. An agent's
/// instruction waiting for its own confirm is the agent's.
fn awaits_persons_confirm(comment: &v1::Comment) -> bool {
    let persons = comment
        .author
        .as_ref()
        .is_some_and(|author| author.actor() == v1::TransitionActor::Human || author.web.is_some());
    persons
        && comment.instruction.as_ref().is_some_and(|instruction| {
            instruction.state() == v1::InstructionState::AwaitingConfirmation
        })
}

fn in_flight(comment: &v1::Comment) -> bool {
    comment.instruction.as_ref().is_some_and(|instruction| {
        matches!(
            instruction.state(),
            v1::InstructionState::Pending
                | v1::InstructionState::Delivered
                | v1::InstructionState::Running
        )
    })
}

fn unowned(assignment: &v1::ConflictAssignment) -> bool {
    assignment.state() == v1::ConflictAssignmentState::Unowned
}

fn blocked(assignment: &v1::ConflictAssignment) -> bool {
    assignment.state() == v1::ConflictAssignmentState::Blocked
}

fn needs_attention(workspace: &v1::Workspace) -> bool {
    workspace.attention() != v1::Attention::Unspecified
        && workspace.state() == v1::WorkspaceState::Active
}

#[cfg(test)]
/// The reasons a table names, in order.
fn reasons<R>(rules: Rules<R>) -> Vec<Reason> {
    rules.iter().map(|(reason, _)| *reason).collect()
}

#[cfg(test)]
/// Every table's reasons, for the tests.
pub(crate) fn tables() -> [Vec<Reason>; 5] {
    [
        reasons(PROPOSAL),
        reasons(HELP),
        reasons(INSTRUCTION),
        reasons(CONFLICT),
        reasons(WORKSPACE),
    ]
}

#[cfg(test)]
/// Whether every needs-you-now row of `reasons` comes before its watching
/// rows.
pub(crate) fn needs_first(reasons: &[Reason]) -> bool {
    reasons.windows(2).all(|pair| {
        !(pair[0].bucket() == Bucket::Watching && pair[1].bucket() == Bucket::NeedsYouNow)
    })
}
