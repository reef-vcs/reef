//! Help requests from agents (R3.32, ADR 0083; `docs/comments.md`).
//!
//! A help request is a comment of kind `help_request`: an agent asking a
//! person, with a reason, what is wrong (the text), what it tried and an
//! optional question, and for a conflict the change's conflict and
//! assignment ids. It is never delivered to a session. An instruction that
//! replies to it is an answer: it goes to the session that asked, written
//! PENDING, and the help request becomes ANSWERED in the answer's own
//! operation. Its other moves (withdraw, reopen when every answer was
//! cancelled before delivery, the sweep's `change_final` and `expired`, and
//! `conflict_unblocked` when the conflict assignment it names leaves BLOCKED)
//! are events like an instruction's, recorded by the same write path.

use std::sync::Arc;

use reef_lib::jj::OpKind;
use reef_lib::jj::comments as jj_comments;
use reef_lib::jj::oplog::OpMeta;
use tracing::Instrument as _;

use super::{
    CommentError, MAX_SUMMARY_BYTES, MoveOutcome, Planned, TargetFacts, record, valid_text,
};
use crate::daemon::AppState;
use crate::db::{CommentEventRow, CommentRow, HelpColumns};
use crate::lifecycle::Actor;
use crate::model::{RepoId, Timestamp, WorkspaceRecord};
use crate::repo::RepoThreadError;

/// The most bytes of a help request's question.
pub const MAX_QUESTION_BYTES: usize = 4096;
/// The most open help requests on one change.
pub const MAX_OPEN_PER_CHANGE: u64 = 16;
/// The most open help requests in one repository.
pub const MAX_OPEN_PER_REPO: u64 = 256;

/// Where a help request is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelpState {
    /// Asked; nobody has answered.
    Open,
    /// An instruction replying to it was written.
    Answered,
    /// Its asker no longer needs it, or it became moot. Final.
    Withdrawn,
}

impl HelpState {
    /// The column's text.
    pub fn as_str(self) -> &'static str {
        match self {
            HelpState::Open => "open",
            HelpState::Answered => "answered",
            HelpState::Withdrawn => "withdrawn",
        }
    }

    /// The state a column's text names.
    pub fn parse(text: &str) -> Option<HelpState> {
        [HelpState::Open, HelpState::Answered, HelpState::Withdrawn]
            .into_iter()
            .find(|state| state.as_str() == text)
    }
}

impl std::fmt::Display for HelpState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an agent asks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reason {
    /// A choice that is a person's.
    Decision,
    /// The change's conflict.
    Conflict,
    /// Something outside its reach.
    Blocked,
    /// The task or an instruction is ambiguous.
    Unclear,
    /// Anything else.
    #[default]
    Other,
}

impl Reason {
    /// Every reason, in the API's order.
    pub const ALL: [Reason; 5] = [
        Reason::Decision,
        Reason::Conflict,
        Reason::Blocked,
        Reason::Unclear,
        Reason::Other,
    ];

    /// The column's text.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Decision => "decision",
            Reason::Conflict => "conflict",
            Reason::Blocked => "blocked",
            Reason::Unclear => "unclear",
            Reason::Other => "other",
        }
    }

    /// The reason a column's text names.
    pub fn parse(text: &str) -> Option<Reason> {
        Reason::ALL
            .into_iter()
            .find(|reason| reason.as_str() == text)
    }
}

/// What an agent sends with a help request, as requested.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HelpInput {
    /// Why it asks.
    pub reason: Reason,
    /// What it tried.
    pub tried: String,
    /// Its question; empty for none.
    pub question: String,
    /// An assignment id, a conflict id, or empty for the change's conflict.
    pub conflict: String,
    /// Ask only while the conflict's assignment is blocked (R3.28).
    pub while_blocked: bool,
}

/// Checks what needs no repository: `tried` is required, the question
/// optional, both as text; a named conflict needs reason CONFLICT (an
/// unspecified one becomes it).
pub fn check_input(input: Option<&HelpInput>) -> Result<HelpInput, CommentError> {
    let mut input = input.cloned().ok_or(CommentError::HelpRequired)?;
    if input.tried.trim().is_empty() {
        return Err(CommentError::HelpRequired);
    }
    if !valid_text(&input.tried, super::MAX_TEXT_BYTES, true)
        || !valid_text(&input.question, MAX_QUESTION_BYTES, false)
    {
        return Err(CommentError::InvalidText);
    }
    let conflict = input.conflict.trim().to_owned();
    input.conflict = conflict;
    if !input.conflict.is_empty() {
        if !(input.conflict.starts_with("asg_") || input.conflict.starts_with("cfl_")) {
            return Err(CommentError::InvalidConflict {
                conflict: input.conflict,
            });
        }
        if !matches!(input.reason, Reason::Conflict | Reason::Other) {
            return Err(CommentError::InvalidConflict {
                conflict: input.conflict,
            });
        }
        input.reason = Reason::Conflict;
    }
    Ok(input)
}

/// The client session a help request is asked for: the request's, else the
/// declared one of the workspace's live provenance session.
pub fn asking_client_session(
    state: &AppState,
    requested: &str,
    workspace: Option<&WorkspaceRecord>,
) -> Option<String> {
    let requested = requested.trim();
    if !requested.is_empty() {
        return Some(requested.to_owned());
    }
    workspace
        .and_then(|record| {
            state
                .db
                .live_provenance_session(&record.id.as_hex())
                .ok()
                .flatten()
        })
        .and_then(|session| session.declared.client_session)
}

/// The conflict a help request with reason CONFLICT names, as
/// `(conflict_id, assignment_id)`: `current` is the change's conflict id
/// now (the repository thread read it), and `named` what the request said.
pub fn conflict_reference(
    state: &AppState,
    repo: RepoId,
    change: &str,
    current: &str,
    named: &str,
) -> Result<(String, Option<String>), CommentError> {
    let open_assignment = || -> Result<Option<String>, CommentError> {
        Ok(state
            .db
            .assignment_of_change(repo, change)?
            .filter(|row| row.closed_at.is_none())
            .map(|row| row.id))
    };
    if named.starts_with("asg_") {
        let row =
            state
                .db
                .assignment(repo, named)?
                .ok_or_else(|| CommentError::AssignmentNotFound {
                    assignment: named.to_owned(),
                })?;
        if row.change_id != change {
            return Err(CommentError::InvalidConflict {
                conflict: named.to_owned(),
            });
        }
        return Ok((current.to_owned(), Some(row.id)));
    }
    if !named.is_empty() && named != current {
        return Err(CommentError::ConflictChanged {
            conflict_id: current.to_owned(),
        });
    }
    Ok((current.to_owned(), open_assignment()?))
}

/// For a help request asked `while_blocked`: refused unless the assignment
/// in `conflict` (as [`conflict_reference`] returned it) is open and
/// BLOCKED.
pub fn check_blocked(
    state: &AppState,
    repo: RepoId,
    conflict: Option<&(String, Option<String>)>,
) -> Result<(), CommentError> {
    let assignment = conflict
        .and_then(|(_, assignment)| assignment.clone())
        .unwrap_or_default();
    let blocked = !assignment.is_empty()
        && state.db.assignment(repo, &assignment)?.is_some_and(|row| {
            row.closed_at.is_none() && row.state == crate::assign::State::Blocked.as_str()
        });
    if blocked {
        Ok(())
    } else {
        Err(CommentError::ConflictNotBlocked { assignment })
    }
}

/// The session an answer to `help` is for: the one that asked it. `None`
/// when it named neither a client session nor a Reef session, so any
/// session on the change may take the answer. A help request asked through
/// the remote listener names no session: its answer is the asking token's,
/// which claims it (`ClaimQuery::asker_token`), so a token cannot route an
/// answer to a session it names (S-240).
pub fn answer_target(state: &AppState, help: &CommentRow) -> Option<TargetFacts> {
    let columns = help.help.as_ref()?;
    if help.remote_token.is_some() {
        return Some(TargetFacts {
            source: "help_request",
            session_id: String::new(),
            client_session: None,
            harness: None,
            model: None,
            agent: help.agent.clone(),
            workspace_id: String::new(),
        });
    }
    let session = help
        .session_id
        .as_deref()
        .and_then(|id| state.db.provenance_session(id).ok().flatten());
    if columns.client_session.is_none() && session.is_none() {
        return None;
    }
    Some(TargetFacts {
        source: "help_request",
        session_id: help.session_id.clone().unwrap_or_default(),
        client_session: columns.client_session.clone().or_else(|| {
            session
                .as_ref()
                .and_then(|session| session.declared.client_session.clone())
        }),
        harness: session
            .as_ref()
            .and_then(|session| session.declared.harness.clone()),
        model: session
            .as_ref()
            .and_then(|session| session.declared.model.clone()),
        agent: session
            .as_ref()
            .and_then(|session| session.agent.clone())
            .or_else(|| help.agent.clone()),
        workspace_id: session
            .map(|session| session.workspace_id)
            .unwrap_or_default(),
    })
}

/// Whether an answer from a writer with `client_session`, Reef session
/// `session_id` and remote token `token` would be the asker answering
/// itself: the same token for a help request asked remotely, else the same
/// client session when the asker declared one, else the same Reef session.
/// A person answering from a shell in the asker's workspace, with no client
/// session of its own, is the asker only when the asker declared none.
pub fn is_self_answer(
    help: &CommentRow,
    client_session: Option<&str>,
    session_id: Option<&str>,
    token: Option<&str>,
) -> bool {
    if let Some(asker) = help.remote_token.as_deref() {
        return token == Some(asker);
    }
    match help
        .help
        .as_ref()
        .and_then(|columns| columns.client_session.as_deref())
    {
        Some(asked_by) => client_session == Some(asked_by),
        None => help.session_id.is_some() && help.session_id.as_deref() == session_id,
    }
}

/// The columns of a new help request; `assignment_seq` is its
/// assignment's last event seq when asked.
pub fn columns(
    input: &HelpInput,
    client_session: Option<String>,
    conflict: Option<(String, Option<String>)>,
    assignment_seq: Option<i64>,
) -> HelpColumns {
    let (conflict_id, assignment_id) = match conflict {
        Some((conflict, assignment)) => (Some(conflict), assignment),
        None => (None, None),
    };
    HelpColumns {
        state: HelpState::Open.as_str().to_owned(),
        reason: input.reason.as_str().to_owned(),
        tried: input.tried.clone(),
        question: Some(input.question.clone()).filter(|question| !question.trim().is_empty()),
        client_session,
        conflict_id,
        assignment_id,
        assignment_seq,
        cause: "request".to_owned(),
        ..HelpColumns::default()
    }
}

/// An event moving the help request `help` from `from` to `to`.
pub(super) fn move_event(
    help: &CommentRow,
    from: HelpState,
    to: HelpState,
    cause: &str,
    base: CommentEventRow,
) -> Planned {
    let event = match (from, to) {
        (HelpState::Open, HelpState::Answered) => "answer",
        (HelpState::Answered, HelpState::Open) => "reopen",
        _ => "withdraw",
    };
    Planned {
        event: CommentEventRow {
            repo_id: help.repo_id,
            id: crate::identity::new_id("cev_"),
            comment_id: help.id.clone(),
            change_id: help.change_id.clone(),
            event: event.to_owned(),
            cause: cause.to_owned(),
            help_from: Some(from.as_str().to_owned()),
            help_to: Some(to.as_str().to_owned()),
            created_at: Timestamp::now(),
            ..base
        },
        comment: None,
        kind: "help_request",
        round: help.round,
    }
}

/// A daemon move of `help` (the sweep's `change_final` and `expired`, and
/// `conflict_unblocked`).
pub(super) fn daemon_move(
    help: &CommentRow,
    to: HelpState,
    cause: &str,
    transition: Option<&str>,
) -> Option<Planned> {
    let from = help
        .help
        .as_ref()
        .and_then(|columns| HelpState::parse(&columns.state))?;
    let base = CommentEventRow {
        actor: Actor::Daemon.as_str().to_owned(),
        transition_id: transition.map(str::to_owned),
        ..CommentEventRow::blank(help.repo_id)
    };
    Some(move_event(help, from, to, cause, base))
}

/// The reopen an answer's cancellation brings, when `answer` replies to an
/// answered help request, it was never delivered, and no other answer was
/// delivered or is still live. Read under the comment lock.
pub(super) fn reopen_on_cancel(
    state: &AppState,
    answer: &CommentRow,
    base: &CommentEventRow,
) -> Result<Option<Planned>, CommentError> {
    let Some(parent) = answer.reply_to.as_deref() else {
        return Ok(None);
    };
    let delivered = answer
        .instruction
        .as_ref()
        .is_some_and(|columns| columns.delivered_at.is_some());
    if delivered {
        return Ok(None);
    }
    let Some(help) = state.db.comment(answer.repo_id, parent)? else {
        return Ok(None);
    };
    let answered = help
        .help
        .as_ref()
        .is_some_and(|columns| columns.state == HelpState::Answered.as_str());
    if !answered
        || state
            .db
            .live_answers(answer.repo_id, &answer.change_id, parent, &answer.id)?
            > 0
    {
        return Ok(None);
    }
    Ok(Some(move_event(
        &help,
        HelpState::Answered,
        HelpState::Open,
        "answer_cancelled",
        base.clone(),
    )))
}

/// A client's withdrawal of a help request.
#[derive(Clone, Debug)]
pub struct WithdrawRequest {
    /// The repository.
    pub repo: RepoId,
    /// The help request's id.
    pub id: String,
    /// Why; empty for none.
    pub summary: String,
    /// The workspace the caller works in.
    pub workspace: Option<WorkspaceRecord>,
    /// The call's agent, marked with its remote token.
    pub agent: Option<String>,
    /// The remote token, `(id, name)`.
    pub remote: Option<(String, String)>,
}

/// Withdraws an open help request for a client.
///
/// Runs as its own task, so a caller that stops waiting cannot cut it
/// between its operation and its rows.
pub async fn withdraw(
    state: &Arc<AppState>,
    request: WithdrawRequest,
) -> Result<MoveOutcome, CommentError> {
    let state = Arc::clone(state);
    let repo = request.repo;
    let span = tracing::Span::current();
    tokio::spawn(async move { withdraw_inner(&state, request).await }.instrument(span))
        .await
        .map_err(|_| CommentError::Thread(RepoThreadError::Panicked { id: repo }))?
}

async fn withdraw_inner(
    state: &AppState,
    request: WithdrawRequest,
) -> Result<MoveOutcome, CommentError> {
    if !valid_text(&request.summary, MAX_SUMMARY_BYTES, false) {
        return Err(CommentError::InvalidText);
    }
    let repo = request.repo;
    let create_lock = state.manager.repo_lock(repo);
    let _shared = create_lock.read().await;
    let comment_lock = state.comments.lock(repo);
    let _held = comment_lock.lock().await;
    let id = request.id.trim().to_owned();
    let help = state
        .db
        .comment(repo, &id)?
        .ok_or_else(|| CommentError::NotFound { id: id.clone() })?;
    let Some(columns) = &help.help else {
        return Err(CommentError::NotAHelpRequest { id });
    };
    let from = HelpState::parse(&columns.state).unwrap_or(HelpState::Withdrawn);
    if from == HelpState::Withdrawn {
        return Ok(MoveOutcome {
            comment: Some(help),
            op_id: None,
        });
    }
    if from != HelpState::Open {
        return Err(CommentError::HelpNotOpen {
            state: from.as_str().to_owned(),
        });
    }
    if let Some((token, _)) = &request.remote
        && help.remote_token.as_deref() != Some(token.as_str())
    {
        return Err(CommentError::RemoteNotOwned { id });
    }
    let (agent, _, web_session) = crate::lifecycle::caller_parts(request.agent.as_deref());
    let actor = if request.remote.is_some() || agent.is_some() {
        Actor::Agent
    } else {
        Actor::Human
    };
    let (identity_id, _, _) =
        crate::lifecycle::identity_for(state, request.workspace.as_ref(), request.remote.as_ref());
    let base = CommentEventRow {
        agent,
        actor: actor.as_str().to_owned(),
        identity_id,
        remote_token: request.remote.as_ref().map(|(id, _)| id.clone()),
        summary: Some(request.summary.clone()).filter(|summary| !summary.is_empty()),
        web_session,
        ..CommentEventRow::blank(repo)
    };
    let mut meta = OpMeta::new(
        OpKind::Comment,
        jj_comments::comment_description("withdraw", &id, &help.change_id),
    );
    meta.workspace = request.workspace.as_ref().map(|record| record.id.as_hex());
    meta.agent = request.agent.clone().or_else(|| {
        request
            .workspace
            .as_ref()
            .map(|record| record.owner.clone())
    });
    let planned = vec![move_event(
        &help,
        from,
        HelpState::Withdrawn,
        "request",
        base,
    )];
    let (op_id, _) = record(state, repo, planned, meta).await?;
    state.automatic.mark(repo, [help.change_id.clone()]);
    tracing::info!(comment = %id, op_id = %op_id, "help request withdrawn");
    Ok(MoveOutcome {
        comment: state.db.comment(repo, &id)?,
        op_id: Some(op_id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_and_reasons_round_trip() {
        for state in [HelpState::Open, HelpState::Answered, HelpState::Withdrawn] {
            assert_eq!(HelpState::parse(state.as_str()), Some(state));
        }
        for reason in Reason::ALL {
            assert_eq!(Reason::parse(reason.as_str()), Some(reason));
        }
        assert_eq!(HelpState::parse("closed"), None);
    }

    #[test]
    fn input_needs_what_was_tried() {
        assert!(matches!(check_input(None), Err(CommentError::HelpRequired)));
        let blank = HelpInput {
            tried: "  ".to_owned(),
            ..HelpInput::default()
        };
        assert!(matches!(
            check_input(Some(&blank)),
            Err(CommentError::HelpRequired)
        ));
        let question = HelpInput {
            tried: "ran the tests".to_owned(),
            question: "x".repeat(MAX_QUESTION_BYTES + 1),
            ..HelpInput::default()
        };
        assert!(matches!(
            check_input(Some(&question)),
            Err(CommentError::InvalidText)
        ));
        let control = HelpInput {
            tried: "ran \u{1b}[2J".to_owned(),
            ..HelpInput::default()
        };
        assert!(matches!(
            check_input(Some(&control)),
            Err(CommentError::InvalidText)
        ));
    }

    #[test]
    fn a_named_conflict_makes_the_reason_conflict() {
        let named = HelpInput {
            tried: "nothing yet".to_owned(),
            conflict: " asg_0123456789abcdef ".to_owned(),
            ..HelpInput::default()
        };
        let checked = check_input(Some(&named)).unwrap();
        assert_eq!(checked.reason, Reason::Conflict);
        assert_eq!(checked.conflict, "asg_0123456789abcdef");
        let other = HelpInput {
            reason: Reason::Decision,
            ..named.clone()
        };
        assert!(matches!(
            check_input(Some(&other)),
            Err(CommentError::InvalidConflict { .. })
        ));
        let garbage = HelpInput {
            conflict: "@-".to_owned(),
            ..named
        };
        assert!(matches!(
            check_input(Some(&garbage)),
            Err(CommentError::InvalidConflict { .. })
        ));
    }
}
