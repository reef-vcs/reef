//! Change lifecycle states (R3.09, ADR 0063; `docs/proposals.md`).
//!
//! A change is a draft until it is proposed. Each transition after that is
//! recorded twice, in a fixed order: a `pending` row in
//! `lifecycle_transitions`, then a jj operation whose `reef.lifecycle`
//! attribute carries it, then the row committed with the operation's id in
//! the same SQLite transaction that updates the change's `lifecycle` row
//! (`record`). A crash between the two leaves a pending row, which
//! `resolve_pending` commits or drops at the next start by looking for its
//! operation. The operation is the log's record and what subscribers see;
//! the rows keep the state when GC drops old operations.
//!
//! Every transition of a repository runs under its lifecycle lock
//! ([`Lifecycle::lock`]), taken after the repository's create lock shared,
//! so a state check and its record cannot interleave with another
//! transition; R3.13's landing will hold it too.
//!
//! Clients propose, withdraw, decline and supersede ([`transition`]); the
//! daemon records a recorded change as landed when it reaches `trunk()`
//! ([`observe`]); undo reverts a client transition ([`undo`]). Queueing and
//! landing are R3.13's.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

use std::time::Duration;
use tracing::Instrument as _;

use futures::FutureExt as _;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use reef_lib::jj::lifecycle::{
    self as jj_lifecycle, LifecycleState, TransitionCause, TransitionRecord,
};
use reef_lib::jj::log::{ChangeInfo, Describer};
use reef_lib::jj::oplog::{self, OpMeta, RemoteToken};
use reef_lib::jj::revset::{self, RevisionError, RevsetScope};
use reef_lib::jj::rewrite::{self, RewriteError};
use reef_lib::jj::undo::UndoError;
use reef_lib::jj::{ChangeIdHex, OpKind, OperationIdHex};

use crate::daemon::AppState;
use crate::db::{
    DbError, LifecycleFilter, LifecycleRow, LifecycleUpdate, TransitionFilter, TransitionRow,
};
use crate::error::{Advice, Advise};
use crate::model::{RepoId, Timestamp, WorkspaceRecord};
use crate::repo::RepoThreadError;
use crate::workspace::ManagerError;

pub mod messages;
pub mod observe;

/// A transition's reason, at most this many bytes.
pub const MAX_REASON_BYTES: usize = 4096;

/// How long before a pending row's creation its operation may have ended
/// and still be found at start.
const PENDING_SLACK: Duration = Duration::from_mins(5);

/// The per-repository lifecycle locks; one in [`AppState`].
#[derive(Debug, Default)]
pub struct Lifecycle {
    locks: Mutex<HashMap<RepoId, Arc<tokio::sync::Mutex<()>>>>,
}

impl Lifecycle {
    /// `repo`'s lifecycle lock: held from a transition's state check to its
    /// commit, by every transition and by R3.13's landing.
    pub fn lock(&self, repo: RepoId) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(locks.entry(repo).or_default())
    }
}

/// Who performed a transition, as recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actor {
    /// A person, as declared on the socket.
    Human,
    /// An agent.
    Agent,
    /// The daemon itself.
    Daemon,
}

impl Actor {
    /// The column's text.
    pub fn as_str(self) -> &'static str {
        match self {
            Actor::Human => "human",
            Actor::Agent => "agent",
            Actor::Daemon => "daemon",
        }
    }

    /// The actor a column's text names; agent for anything else.
    pub fn parse(text: &str) -> Actor {
        match text {
            "human" => Actor::Human,
            "daemon" => Actor::Daemon,
            _ => Actor::Agent,
        }
    }
}

/// Why a transition or a lifecycle read was refused or failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LifecycleError {
    /// No target state, or one this daemon does not know.
    #[error("name the state to move the change to")]
    TargetRequired,
    /// A target state only the daemon records.
    #[error("{to} is recorded by the daemon, not requested")]
    Reserved {
        /// The target.
        to: String,
    },
    /// The table does not allow the transition.
    #[error("change {change} is {state}; it cannot become {to}")]
    NotAllowed {
        /// The change.
        change: String,
        /// Its state.
        state: LifecycleState,
        /// The target.
        to: LifecycleState,
    },
    /// `expected_state` is not the change's.
    #[error("the change is {state}, not the expected {expected}")]
    ExpectedState {
        /// What the caller expected.
        expected: LifecycleState,
        /// What it is.
        state: LifecycleState,
    },
    /// A reason over the limit or with a control character.
    #[error(
        "the reason is over {MAX_REASON_BYTES} bytes or has a control character other than newline and tab"
    )]
    InvalidReason,
    /// A destination that is not a local bookmark, or one given for
    /// another state.
    #[error("{destination:?} is not a local bookmark a change can be proposed for")]
    InvalidDestination {
        /// The destination.
        destination: String,
    },
    /// No `trunk()` and no destination.
    #[error("the repository has no trunk() bookmark; name a destination")]
    NoDestination,
    /// SUPERSEDED without `superseded_by`, or one for another state.
    #[error("superseded_by names the superseding change for SUPERSEDED, and only for it")]
    SupersededByRequired,
    /// `superseded_by` names the change itself, or no visible commit.
    #[error("{superseded_by:?} does not name another change's visible commit")]
    InvalidSupersededBy {
        /// What it named.
        superseded_by: String,
    },
    /// A proposal of an empty change.
    #[error("change {change} changes nothing")]
    Empty {
        /// The change.
        change: String,
    },
    /// A proposal of a conflicted change.
    #[error("change {change} has conflicts")]
    Conflicted {
        /// The change.
        change: String,
    },
    /// A proposal of one commit of a divergent change.
    #[error("change {change} is divergent")]
    Divergent {
        /// The change.
        change: String,
    },
    /// An abandon of a change in review.
    #[error("change {change} is {state}; withdraw it before abandoning it")]
    InReview {
        /// The change.
        change: String,
        /// Its state.
        state: LifecycleState,
    },
    /// Through the remote listener, another token's proposal or transition.
    #[error("change {change}'s proposal was not made through this remote token")]
    RemoteNotOwned {
        /// The change.
        change: String,
    },
    /// A decline through the remote listener.
    #[error("a decline needs a human, which the remote listener cannot verify yet")]
    RemoteHuman,
    /// The revision could not be resolved.
    #[error("resolve the revision")]
    Revision(#[source] RevisionError),
    /// The commit cannot be moved through its lifecycle.
    #[error("check the commit")]
    Rewrite(#[source] RewriteError),
    /// An undo was refused.
    #[error("undo the operation")]
    Undo(#[source] UndoError),
    /// The repository could not be reached or refreshed.
    #[error("reach the repository")]
    Manager(#[source] ManagerError),
    /// The repository thread failed.
    #[error("run on the repository thread")]
    Thread(#[source] RepoThreadError),
    /// The operation could not be written.
    #[error("record the transition")]
    Record(#[source] jj_lifecycle::LifecycleError),
    /// The database failed.
    #[error("read or write lifecycle rows")]
    Db(#[source] DbError),
}

impl From<DbError> for LifecycleError {
    fn from(error: DbError) -> Self {
        LifecycleError::Db(error)
    }
}

impl From<RepoThreadError> for LifecycleError {
    fn from(error: RepoThreadError) -> Self {
        LifecycleError::Thread(error)
    }
}

impl From<ManagerError> for LifecycleError {
    fn from(error: ManagerError) -> Self {
        LifecycleError::Manager(error)
    }
}

impl From<RevisionError> for LifecycleError {
    fn from(error: RevisionError) -> Self {
        LifecycleError::Revision(error)
    }
}

impl From<RewriteError> for LifecycleError {
    fn from(error: RewriteError) -> Self {
        LifecycleError::Rewrite(error)
    }
}

impl From<jj_lifecycle::LifecycleError> for LifecycleError {
    fn from(error: jj_lifecycle::LifecycleError) -> Self {
        LifecycleError::Record(error)
    }
}

impl From<reef_lib::jj::OpenError> for LifecycleError {
    fn from(error: reef_lib::jj::OpenError) -> Self {
        LifecycleError::Manager(ManagerError::from(error))
    }
}

impl Classify for LifecycleError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Reserved { .. }
            | Self::TargetRequired
            | Self::InvalidReason
            | Self::InvalidDestination { .. }
            | Self::SupersededByRequired => ErrorKind::InvalidArgument,
            Self::NotAllowed { .. }
            | Self::ExpectedState { .. }
            | Self::NoDestination
            | Self::InvalidSupersededBy { .. }
            | Self::Empty { .. }
            | Self::Conflicted { .. }
            | Self::Divergent { .. }
            | Self::InReview { .. } => ErrorKind::Conflict,
            Self::RemoteNotOwned { .. } | Self::RemoteHuman => ErrorKind::PermissionDenied,
            Self::Revision(error) => error.kind(),
            Self::Rewrite(error) => error.kind(),
            Self::Undo(error) => error.kind(),
            Self::Manager(error) => error.kind(),
            Self::Thread(error) => error.kind(),
            Self::Record(error) => error.kind(),
            Self::Db(error) => error.kind(),
        }
    }
}

impl Advise for LifecycleError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Reserved { .. } => Some(Advice::hint(
                "Propose, withdraw, decline or supersede; the landing service queues and lands proposals, and the daemon records a landing it sees on trunk().",
            )),
            Self::NotAllowed { change, state, .. } if state.is_final() => Some(Advice::hint(
                format!("Change {change} is {state}, which is final; make a new change instead."),
            )),
            Self::NotAllowed { change, .. } => Some(Advice::run(
                "Read its lifecycle and transitions first.",
                ["reef", "proposal", "show", change.as_str()],
            )),
            Self::ExpectedState { .. } => Some(Advice::hint(
                "The change moved since it was read; read it again and retry with its state.",
            )),
            Self::NoDestination => Some(Advice::hint(
                "Create a `main` bookmark, or name the bookmark it is for with `--to`.",
            )),
            Self::Empty { .. } => Some(Advice::hint(
                "A workspace's `@` is often a fresh empty change: did you mean `@-`?",
            )),
            Self::Conflicted { .. } => Some(Advice::hint(
                "Resolve the conflicts first; landing refuses a conflicted change.",
            )),
            Self::Divergent { .. } => Some(Advice::hint(
                "Resolve the divergence first: abandon the commit you do not want with `reef abandon <commit>`.",
            )),
            Self::InReview { change, .. } => Some(Advice::run(
                "Withdraw the proposal first, then abandon it.",
                ["reef", "proposal", "withdraw", change.as_str()],
            )),
            Self::RemoteNotOwned { .. } => Some(Advice::hint(
                "Through the remote listener, only the token that proposed a change proposes it again, withdraws, supersedes or undoes it.",
            )),
            Self::RemoteHuman => Some(Advice::hint(
                "Decline on the daemon's machine; a remote human can decline once R3.43 verifies them.",
            )),
            _ => None,
        }
    }
}

impl LifecycleError {
    /// Its reason and metadata, as `crate::error` reports them.
    pub(crate) fn reason(&self) -> Option<(&'static str, Vec<(&'static str, String)>)> {
        use reef_api::error as reason;
        Some(match self {
            Self::Reserved { to } => (reason::TRANSITION_RESERVED, vec![("to", to.clone())]),
            Self::TargetRequired => (reason::TRANSITION_TARGET_REQUIRED, Vec::new()),
            Self::NotAllowed { change, state, to } => (
                reason::TRANSITION_NOT_ALLOWED,
                vec![
                    ("change_id", change.clone()),
                    ("state", state.as_str().to_owned()),
                    ("to", to.as_str().to_owned()),
                ],
            ),
            Self::ExpectedState { expected, state } => (
                reason::EXPECTED_STATE_MISMATCH,
                vec![
                    ("expected_state", expected.as_str().to_owned()),
                    ("state", state.as_str().to_owned()),
                ],
            ),
            Self::InvalidReason => (reason::INVALID_REASON, Vec::new()),
            Self::InvalidDestination { destination } => (
                reason::INVALID_DESTINATION,
                vec![("destination", destination.clone())],
            ),
            Self::NoDestination => (reason::NO_DESTINATION, Vec::new()),
            Self::SupersededByRequired => (reason::SUPERSEDED_BY_REQUIRED, Vec::new()),
            Self::InvalidSupersededBy { superseded_by } => (
                reason::INVALID_SUPERSEDED_BY,
                vec![("superseded_by", superseded_by.clone())],
            ),
            Self::Empty { change } => (reason::CHANGE_EMPTY, vec![("change_id", change.clone())]),
            Self::Conflicted { change } => (
                reason::CHANGE_CONFLICTED,
                vec![("change_id", change.clone())],
            ),
            Self::Divergent { change } => (
                reason::CHANGE_DIVERGENT,
                vec![("change_id", change.clone())],
            ),
            Self::InReview { change, state } => (
                reason::CHANGE_IN_REVIEW,
                vec![
                    ("change_id", change.clone()),
                    ("state", state.as_str().to_owned()),
                ],
            ),
            Self::RemoteNotOwned { change } => (
                reason::REMOTE_PROPOSAL_NOT_OWNED,
                vec![("change_id", change.clone())],
            ),
            Self::RemoteHuman => (reason::REMOTE_HUMAN_REQUIRED, Vec::new()),
            _ => return None,
        })
    }
}

/// The transition table for clients: whether `to` may follow `from`.
/// `to == from` is decided by the caller (a no-op, or a proposal's
/// update).
pub fn allowed(from: LifecycleState, to: LifecycleState) -> bool {
    use LifecycleState as S;
    match to {
        S::Proposed => matches!(from, S::Draft | S::Withdrawn | S::Declined | S::Proposed),
        S::Withdrawn | S::Declined | S::Superseded => matches!(from, S::Proposed | S::Queued),
        S::Draft | S::Queued | S::Landed => false,
    }
}

/// Whether only the daemon records `to`.
pub fn reserved(to: LifecycleState) -> bool {
    matches!(
        to,
        LifecycleState::Draft | LifecycleState::Queued | LifecycleState::Landed
    )
}

/// The verb a transition's description names.
pub fn verb(from: LifecycleState, to: LifecycleState) -> &'static str {
    use LifecycleState as S;
    match (from, to) {
        (_, S::Proposed) if from == S::Queued => "dequeue",
        (_, S::Proposed) => "propose",
        (_, S::Withdrawn) => "withdraw",
        (_, S::Declined) => "decline",
        (_, S::Superseded) => "supersede",
        (_, S::Queued) => "queue",
        (_, S::Landed) => "land",
        (_, S::Draft) => "unpropose",
    }
}

/// Whether a reason may be recorded.
pub fn valid_reason(reason: &str) -> bool {
    reason.len() <= MAX_REASON_BYTES
        && !reason
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
}

/// A change's lifecycle state from a row, `Draft` for none.
fn state_of_row(row: Option<&LifecycleRow>) -> LifecycleState {
    row.and_then(|row| row.state.parse().ok())
        .unwrap_or(LifecycleState::Draft)
}

/// A transition about to be recorded: its row (pending) and the attribute's
/// record of it.
struct Planned {
    row: TransitionRow,
    /// Who the change's proposal belongs to afterwards, when this
    /// transition restores it (an undo); `None` to follow [`update_for`]'s
    /// rule.
    owner: Option<Owner>,
}

/// The agent and the remote token a proposal belongs to.
type Owner = (Option<String>, Option<String>);

impl Planned {
    fn record(&self) -> TransitionRecord {
        let row = &self.row;
        TransitionRecord {
            id: row.id.clone(),
            change: row.change_id.clone(),
            commit: row.commit_id.clone(),
            from: row.from_state.parse().unwrap_or(LifecycleState::Draft),
            to: row.to_state.parse().unwrap_or(LifecycleState::Draft),
            cause: row.cause.parse().unwrap_or(TransitionCause::Request),
            round: row.round,
            reason: row.reason.clone().unwrap_or_default(),
            superseded_by: row.superseded_by.clone().unwrap_or_default(),
            destination: row.destination.clone().unwrap_or_default(),
            undoes: row.undoes_operation_id.clone().unwrap_or_default(),
        }
    }
}

/// What committing `row` does to its change's `lifecycle` row, given the
/// row before (`existing`), at `recorded_at`. A proposal belongs to whoever
/// opened its round (a request from another state to `proposed`); an
/// update of the commit keeps the owner, and an undo restores the one
/// `owner` names.
pub(crate) fn update_for(
    row: &TransitionRow,
    existing: Option<&LifecycleRow>,
    recorded_at: Timestamp,
    owner: Option<&Owner>,
) -> LifecycleUpdate {
    let to: LifecycleState = row.to_state.parse().unwrap_or(LifecycleState::Draft);
    if to == LifecycleState::Draft {
        return LifecycleUpdate::Delete;
    }
    let opened = to == LifecycleState::Proposed
        && row.from_state != LifecycleState::Proposed.as_str()
        && row.cause == TransitionCause::Request.as_str();
    let (agent, token) = if opened {
        (row.agent.clone(), row.remote_token.clone())
    } else if let Some((agent, token)) = owner {
        (agent.clone(), token.clone())
    } else {
        (
            existing.and_then(|existing| existing.proposed_by_agent.clone()),
            existing.and_then(|existing| existing.proposed_by_token.clone()),
        )
    };
    LifecycleUpdate::Set(Box::new(LifecycleRow {
        repo_id: row.repo_id,
        change_id: row.change_id.clone(),
        state: row.to_state.clone(),
        round: row.round,
        commit_id: row.commit_id.clone(),
        destination: row
            .destination
            .clone()
            .or_else(|| existing.and_then(|existing| existing.destination.clone())),
        superseded_by: (to == LifecycleState::Superseded)
            .then(|| row.superseded_by.clone())
            .flatten(),
        transition_id: row.id.clone(),
        proposed_by_agent: agent,
        proposed_by_token: token,
        updated_at: recorded_at,
    }))
}

/// What `record` wrote.
#[derive(Debug)]
pub struct Recorded {
    /// The operation.
    pub op_id: OperationIdHex,
    /// The committed rows, in order.
    pub rows: Vec<TransitionRow>,
}

/// Records `planned` in one operation described by `meta`: pending rows,
/// the operation, then the rows committed with the change's `lifecycle`
/// rows. The caller holds the repository's create lock shared and its
/// lifecycle lock.
async fn record(
    state: &AppState,
    repo: RepoId,
    planned: Vec<Planned>,
    mut meta: OpMeta,
) -> Result<Recorded, LifecycleError> {
    for plan in &planned {
        state.db.insert_pending_transition(&plan.row)?;
    }
    let drop_pending = |state: &AppState| {
        for plan in &planned {
            if let Err(error) = state.db.delete_pending_transition(repo, &plan.row.id) {
                tracing::warn!(error = %ErrorChain(&error), "cannot drop a pending transition; the next start does");
            }
        }
    };
    meta.transitions = planned.iter().map(Planned::record).collect();
    let handle = match state.manager.handle(repo) {
        Ok(handle) => handle,
        Err(error) => {
            drop_pending(state);
            return Err(error.into());
        }
    };
    let written = handle
        .call_async(move |head| {
            async move {
                head.refresh().await?;
                let op_id = head.repo.record_transitions(&meta).await?;
                let end = oplog::entry(head.repo.readonly().operation()).end;
                Ok::<_, LifecycleError>((op_id, end))
            }
            .boxed_local()
        })
        .await;
    let (op_id, end) = match written {
        Ok(Ok(written)) => written,
        Ok(Err(error)) => {
            drop_pending(state);
            return Err(error);
        }
        Err(error) => {
            // The job may have died after the operation was written: the
            // next start finds it, or drops the rows.
            tracing::warn!(error = %ErrorChain(&error), "the repository thread failed during a transition; the next start resolves its pending rows");
            return Err(error.into());
        }
    };
    let recorded_at = Timestamp::from(end);
    let mut rows = Vec::with_capacity(planned.len());
    for plan in &planned {
        let existing = state.db.lifecycle_row(repo, &plan.row.change_id)?;
        let update = update_for(
            &plan.row,
            existing.as_ref(),
            recorded_at,
            plan.owner.as_ref(),
        );
        if let Some(row) =
            state
                .db
                .commit_transition(repo, &plan.row.id, op_id.as_str(), recorded_at, &update)?
        {
            rows.push(row);
        }
    }
    Ok(Recorded { op_id, rows })
}

/// A client's transition request, resolved by the service.
#[derive(Clone, Debug)]
pub struct TransitionRequest {
    /// The repository.
    pub repo: RepoId,
    /// The workspace the request names.
    pub workspace: Option<WorkspaceRecord>,
    /// The user email revsets' `mine()` reads.
    pub user_email: String,
    /// The repository's root.
    pub root: std::path::PathBuf,
    /// The revision naming the change.
    pub revision: String,
    /// The target.
    pub to: LifecycleState,
    /// The state the caller expects.
    pub expected_state: Option<LifecycleState>,
    /// The commit the caller expects; empty for no check.
    pub expected_commit: String,
    /// The reason.
    pub reason: String,
    /// For SUPERSEDED, the superseding revision.
    pub superseded_by: String,
    /// For PROPOSED, the destination bookmark; empty for `trunk()`'s.
    pub destination: String,
    /// The declared actor, if any.
    pub actor: Option<Actor>,
    /// The call's agent, marked with its remote token
    /// ([`RemoteToken::mark`]).
    pub agent: Option<String>,
    /// The remote token, `(id, name)`.
    pub remote: Option<(String, String)>,
}

/// What [`transition`] did.
#[derive(Debug)]
pub struct TransitionOutcome {
    /// The change's lifecycle row afterwards.
    pub lifecycle: Option<LifecycleRow>,
    /// The transition recorded; `None` for a no-op.
    pub transition: Option<TransitionRow>,
    /// The operation; `None` for a no-op.
    pub op_id: Option<OperationIdHex>,
    /// The change, at the head the checks read; `None` for a recorded
    /// change with no visible commit, named by its id.
    pub change: Option<ChangeInfo>,
}

/// What a transition's checks found on the repository thread.
struct Facts {
    /// The change; `None` for one with no visible commit, named by its id.
    info: Option<ChangeInfo>,
    change: String,
    commit: String,
    superseded_by: Option<ChangeIdHex>,
    destination: Option<String>,
}

/// Checks what needs no repository.
fn check_request(request: &TransitionRequest) -> Result<(), LifecycleError> {
    if reserved(request.to) {
        return Err(LifecycleError::Reserved {
            to: request.to.as_str().to_owned(),
        });
    }
    if !valid_reason(&request.reason) {
        return Err(LifecycleError::InvalidReason);
    }
    let supersede = request.to == LifecycleState::Superseded;
    if supersede == request.superseded_by.trim().is_empty() {
        return Err(LifecycleError::SupersededByRequired);
    }
    if request.to != LifecycleState::Proposed && !request.destination.trim().is_empty() {
        return Err(LifecycleError::InvalidDestination {
            destination: request.destination.clone(),
        });
    }
    if request.revision.trim().is_empty() {
        return Err(RevisionError::Required.into());
    }
    if request.remote.is_some() && request.to == LifecycleState::Declined {
        return Err(LifecycleError::RemoteHuman);
    }
    Ok(())
}

/// The identity a transition records: the remote token's linked one, else
/// the workspace's provenance session's, else the default person identity;
/// with the session.
fn identity_of(
    state: &AppState,
    request: &TransitionRequest,
) -> (Option<String>, Option<String>, Option<String>) {
    let session = request.workspace.as_ref().and_then(|workspace| {
        state
            .db
            .live_provenance_session(&workspace.id.as_hex())
            .ok()
            .flatten()
    });
    let id = match &request.remote {
        Some((token, _)) => state
            .db
            .remote_tokens()
            .ok()
            .and_then(|rows| rows.into_iter().find(|row| &row.id == token))
            .and_then(|row| row.identity_id),
        None => session
            .as_ref()
            .and_then(|session| session.identity_id.clone())
            .or_else(|| state.identity.default_id().map(str::to_owned)),
    };
    let name = id
        .as_deref()
        .and_then(|id| state.db.identity(id).ok().flatten())
        .map(|row| row.name);
    (id, name, session.map(|session| session.session_id))
}

/// Moves a change through its lifecycle for a client; see the module docs.
///
/// Runs as its own task, as a workspace move does, so a caller that stops
/// waiting cannot cut it between its operation and its row.
pub async fn transition(
    state: &Arc<AppState>,
    request: TransitionRequest,
) -> Result<TransitionOutcome, LifecycleError> {
    let state = Arc::clone(state);
    let repo = request.repo;
    let span = tracing::Span::current();
    tokio::spawn(async move { transition_inner(&state, request).await }.instrument(span))
        .await
        .map_err(|_| LifecycleError::Thread(RepoThreadError::Panicked { id: repo }))?
}

#[expect(
    clippy::too_many_lines,
    reason = "the checks in their order, then the row it records"
)]
#[tracing::instrument(skip_all, fields(repo = %request.repo, to = request.to.as_str()))]
async fn transition_inner(
    state: &AppState,
    request: TransitionRequest,
) -> Result<TransitionOutcome, LifecycleError> {
    check_request(&request)?;
    let repo = request.repo;
    let handle = state.manager.handle(repo)?;
    let create_lock = state.manager.repo_lock(repo);
    let _shared = create_lock.read().await;
    let lifecycle_lock = state.lifecycle.lock(repo);
    let _held = lifecycle_lock.lock().await;

    let resolved = {
        let request = request.clone();
        handle
            .call_async(move |head| async move { resolve(head, &request).await }.boxed_local())
            .await?
    };
    let facts = match resolved {
        Ok(facts) => facts,
        // A recorded change with no visible commit (abandoned outside Reef,
        // or by undoing what made it) can still leave review, by its id.
        Err(error @ LifecycleError::Revision(RevisionError::NotFound { .. }))
            if request.to != LifecycleState::Proposed =>
        {
            hidden_change(state, repo, &request.revision)?.ok_or(error)?
        }
        Err(error) => return Err(error),
    };
    let change = facts.change.clone();
    let commit = facts.commit.clone();
    let current = state.db.lifecycle_row(repo, &change)?;
    let from = state_of_row(current.as_ref());
    if let Some(expected) = request.expected_state
        && expected != from
    {
        return Err(LifecycleError::ExpectedState {
            expected,
            state: from,
        });
    }
    // A proposal keeps its destination unless the request names another,
    // while that bookmark exists.
    let kept = current
        .as_ref()
        .and_then(|row| row.destination.clone())
        .filter(|_| {
            request.to == LifecycleState::Proposed && request.destination.trim().is_empty()
        });
    let destination = match kept {
        Some(kept) => {
            let name = kept.clone();
            let exists = handle
                .call(move |head| jj_lifecycle::local_bookmark_exists(&head.readonly(), &name))
                .await?;
            if exists {
                Some(kept)
            } else {
                facts.destination.clone()
            }
        }
        None => facts.destination.clone(),
    };
    let noop = request.to == from
        && !(request.to == LifecycleState::Proposed
            && current.as_ref().is_some_and(|row| {
                row.commit_id != commit || row.destination.as_deref() != destination.as_deref()
            }));
    if request.to != from && !allowed(from, request.to) {
        return Err(LifecycleError::NotAllowed {
            change,
            state: from,
            to: request.to,
        });
    }
    if let Some((token, _)) = &request.remote {
        let first_proposal =
            request.to == LifecycleState::Proposed && from == LifecycleState::Draft;
        let owner = current
            .as_ref()
            .and_then(|row| row.proposed_by_token.as_deref());
        if !first_proposal && owner != Some(token.as_str()) {
            return Err(LifecycleError::RemoteNotOwned { change });
        }
    }
    if noop {
        return Ok(TransitionOutcome {
            lifecycle: current,
            transition: None,
            op_id: None,
            change: facts.info,
        });
    }
    if request.to == LifecycleState::Proposed
        && let Some(info) = &facts.info
    {
        if info.empty {
            return Err(LifecycleError::Empty { change });
        }
        if info.conflict {
            return Err(LifecycleError::Conflicted { change });
        }
        if info.divergent {
            return Err(LifecycleError::Divergent { change });
        }
    }
    // A new round is one more than any before, so rounds never repeat, even
    // after an undo back to draft.
    let round = if request.to == LifecycleState::Proposed && from != LifecycleState::Proposed {
        state.db.max_round(repo, &change)? + 1
    } else {
        current.as_ref().map_or(0, |row| row.round)
    };
    let (agent, remote_name) = match request.agent.as_deref() {
        Some(marked) => {
            let (agent, token) = RemoteToken::unmark(marked);
            (Some(agent.to_owned()), token.map(|token| token.name))
        }
        None => (None, None),
    };
    let actor = if request.remote.is_some() {
        Actor::Agent
    } else {
        request.actor.unwrap_or(if agent.is_some() {
            Actor::Agent
        } else {
            Actor::Human
        })
    };
    let (identity_id, identity_name, session_id) = identity_of(state, &request);
    let row = TransitionRow {
        repo_id: repo,
        id: crate::identity::new_id("trn_"),
        seq: None,
        committed: false,
        change_id: change.clone(),
        commit_id: commit,
        from_state: from.as_str().to_owned(),
        to_state: request.to.as_str().to_owned(),
        cause: TransitionCause::Request.as_str().to_owned(),
        round,
        reason: Some(request.reason.clone()).filter(|reason| !reason.is_empty()),
        superseded_by: facts.superseded_by.as_ref().map(ToString::to_string),
        destination: destination
            .clone()
            .or_else(|| current.as_ref().and_then(|row| row.destination.clone())),
        undoes_operation_id: None,
        undoes_transition_id: None,
        operation_id: None,
        operation_collected: false,
        agent,
        actor: actor.as_str().to_owned(),
        identity_id,
        identity_name,
        session_id,
        remote_token: request.remote.as_ref().map(|(id, _)| id.clone()),
        remote_token_name: request
            .remote
            .as_ref()
            .map(|(_, name)| name.clone())
            .or(remote_name),
        created_at: Timestamp::now(),
        recorded_at: None,
    };
    let mut meta = OpMeta::new(
        OpKind::Transition,
        oplog::transition_description(verb(from, request.to), &change),
    );
    meta.workspace = request.workspace.as_ref().map(|record| record.id.as_hex());
    meta.agent = request.agent.clone().or_else(|| {
        request
            .workspace
            .as_ref()
            .map(|record| record.owner.clone())
    });
    let recorded = record(state, repo, vec![Planned { row, owner: None }], meta).await?;
    let lifecycle = state.db.lifecycle_row(repo, &change)?;
    tracing::info!(
        change_id = %change,
        from = from.as_str(),
        op_id = %recorded.op_id,
        "lifecycle transition recorded"
    );
    Ok(TransitionOutcome {
        lifecycle,
        transition: recorded.rows.into_iter().next(),
        op_id: Some(recorded.op_id),
        change: facts.info,
    })
}

/// A transition's checks on the repository thread.
async fn resolve(
    head: &mut crate::repo::RepoHead,
    request: &TransitionRequest,
) -> Result<Facts, LifecycleError> {
    let expected = rewrite::parse_expected(&head.repo.readonly(), &request.expected_commit)?;
    let workspace = request.workspace.as_ref().map(|record| record.id);
    let agent = request.agent.clone().or_else(|| {
        request
            .workspace
            .as_ref()
            .map(|record| record.owner.clone())
    });
    crate::workspace::refresh_for_resolve(
        head,
        &crate::workspace::import_meta(workspace, agent.as_deref()),
    )
    .await?;
    let repo = head.repo.readonly();
    let jj_name = workspace.map(|id| id.jj_workspace_name());
    let scope = RevsetScope {
        workspace: jj_name.as_deref(),
        user_email: &request.user_email,
        root: &request.root,
    };
    let resolved = revset::resolve_revision(&repo, &request.revision, &scope).await?;
    if let Some(expected) = expected
        && expected != resolved.commit_id
    {
        return Err(RewriteError::Mismatch {
            expected,
            actual: resolved.commit_id,
        }
        .into());
    }
    let describer = Describer::new(&repo, &scope)?;
    let info = describer.describe(&resolved.commit_id).await?;
    // Only a proposal needs a visible commit; a change can leave review by
    // a hidden one.
    if info.hidden && request.to == LifecycleState::Proposed {
        return Err(RewriteError::Hidden(info.commit_id).into());
    }
    if info.immutable {
        return Err(RewriteError::Immutable(info.commit_id).into());
    }
    let superseded_by = if request.to == LifecycleState::Superseded {
        let invalid = || LifecycleError::InvalidSupersededBy {
            superseded_by: request.superseded_by.clone(),
        };
        let other = revset::resolve_revision(&repo, &request.superseded_by, &scope)
            .await
            .map_err(|_| invalid())?;
        let other_info = describer
            .describe(&other.commit_id)
            .await
            .map_err(|_| invalid())?;
        if other_info.hidden || other_info.change_id == info.change_id {
            return Err(invalid());
        }
        Some(other_info.change_id)
    } else {
        None
    };
    let destination = if request.to == LifecycleState::Proposed {
        let named = request.destination.trim();
        if named.is_empty() {
            Some(
                revset::trunk(&repo)
                    .ok_or(LifecycleError::NoDestination)?
                    .bookmark,
            )
        } else if jj_lifecycle::local_bookmark_exists(&repo, named) {
            Some(named.to_owned())
        } else {
            return Err(LifecycleError::InvalidDestination {
                destination: named.to_owned(),
            });
        }
    } else {
        None
    };
    Ok(Facts {
        change: info.change_id.to_string(),
        commit: info.commit_id.to_string(),
        info: Some(info),
        superseded_by,
        destination,
    })
}

/// The facts of a recorded change that `revision` names by a change id
/// prefix (hex or reverse hex) when it has no visible commit: its recorded
/// commit. `None` when the prefix names no recorded change, or several.
fn hidden_change(
    state: &AppState,
    repo: RepoId,
    revision: &str,
) -> Result<Option<Facts>, LifecycleError> {
    let Some(prefix) = crate::service::operations::change_prefix(revision) else {
        return Ok(None);
    };
    let rows = state.db.lifecycle_page(
        repo,
        &LifecycleFilter {
            changes: vec![prefix],
            ..LifecycleFilter::default()
        },
        None,
        2,
    )?;
    let [row] = rows.as_slice() else {
        return Ok(None);
    };
    Ok(Some(Facts {
        info: None,
        change: row.change_id.clone(),
        commit: row.commit_id.clone(),
        superseded_by: None,
        destination: None,
    }))
}

/// What [`undo`] did.
#[derive(Debug)]
pub struct UndoOutcome {
    /// The operation recording the inverse transitions.
    pub op_id: OperationIdHex,
}

/// Reverts the transitions the operation `op_id` recorded, each still its
/// change's latest, with inverse transitions in one operation of kind
/// `undo`; see `UndoOperation`. `remote` is the caller's token id through
/// the remote listener.
pub async fn undo(
    state: &Arc<AppState>,
    repo: RepoId,
    workspace: Option<WorkspaceRecord>,
    agent: Option<String>,
    remote: Option<(String, String)>,
    op_id: OperationIdHex,
) -> Result<UndoOutcome, LifecycleError> {
    let state = Arc::clone(state);
    let span = tracing::Span::current();
    tokio::spawn(
        async move { undo_inner(&state, repo, workspace.as_ref(), agent, remote, &op_id).await }
            .instrument(span),
    )
    .await
    .map_err(|_| LifecycleError::Thread(RepoThreadError::Panicked { id: repo }))?
}

#[expect(
    clippy::too_many_lines,
    reason = "the checks per transition, then the inverse rows"
)]
#[tracing::instrument(skip_all, fields(repo = %repo, op_id = %op_id))]
async fn undo_inner(
    state: &AppState,
    repo: RepoId,
    workspace: Option<&WorkspaceRecord>,
    agent: Option<String>,
    remote: Option<(String, String)>,
    op_id: &OperationIdHex,
) -> Result<UndoOutcome, LifecycleError> {
    let create_lock = state.manager.repo_lock(repo);
    let _shared = create_lock.read().await;
    let lifecycle_lock = state.lifecycle.lock(repo);
    let _held = lifecycle_lock.lock().await;
    let refuse = |why| {
        LifecycleError::Undo(UndoError::NotUndoable {
            op: op_id.clone(),
            why,
        })
    };
    let rows = state.db.transitions_of_operation(repo, op_id.as_str())?;
    if rows.is_empty() {
        return Err(refuse(
            "its lifecycle transitions are not in this daemon's records",
        ));
    }
    let (unmarked, remote_name) = match agent.as_deref() {
        Some(marked) => {
            let (agent, token) = RemoteToken::unmark(marked);
            (Some(agent.to_owned()), token.map(|token| token.name))
        }
        None => (None, None),
    };
    let mut planned = Vec::with_capacity(rows.len());
    for row in &rows {
        if row.cause == TransitionCause::Observed.as_str() {
            return Err(refuse(
                "it records a landing seen on trunk(); move the bookmark back instead",
            ));
        }
        if let Some((token, _)) = &remote
            && row.remote_token.as_deref() != Some(token.as_str())
        {
            return Err(LifecycleError::RemoteNotOwned {
                change: row.change_id.clone(),
            });
        }
        let history = state.db.transition_page(
            repo,
            &TransitionFilter {
                changes: vec![row.change_id.clone()],
                ..TransitionFilter::default()
            },
            None,
            usize::MAX,
        )?;
        let history: Vec<&TransitionRow> = history
            .iter()
            .filter(|other| other.change_id == row.change_id)
            .collect();
        if history.first().map(|latest| latest.id.as_str()) != Some(row.id.as_str()) {
            return Err(LifecycleError::Undo(UndoError::Superseded {
                op: op_id.clone(),
                what: format!("change {}'s lifecycle", row.change_id),
            }));
        }
        let previous = history.get(1).copied();
        let to = previous.map_or("draft".to_owned(), |previous| previous.to_state.clone());
        if to == LifecycleState::Queued.as_str() {
            return Err(refuse(
                "it would put the change back in the landing queue, which only the landing service does",
            ));
        }
        // The proposal's owner then: whoever opened the round the restored
        // state belongs to.
        let owner = history[1..]
            .iter()
            .find(|earlier| {
                earlier.to_state == LifecycleState::Proposed.as_str()
                    && earlier.from_state != LifecycleState::Proposed.as_str()
                    && earlier.cause == TransitionCause::Request.as_str()
            })
            .map(|opened| (opened.agent.clone(), opened.remote_token.clone()));
        planned.push(Planned {
            owner: Some(owner.unwrap_or((None, None))),
            row: TransitionRow {
                repo_id: repo,
                id: crate::identity::new_id("trn_"),
                seq: None,
                committed: false,
                change_id: row.change_id.clone(),
                commit_id: previous.map_or_else(|| row.commit_id.clone(), |p| p.commit_id.clone()),
                from_state: row.to_state.clone(),
                to_state: to,
                cause: TransitionCause::Undo.as_str().to_owned(),
                round: previous.map_or(0, |previous| previous.round),
                reason: None,
                superseded_by: previous.and_then(|previous| previous.superseded_by.clone()),
                destination: previous.and_then(|previous| previous.destination.clone()),
                undoes_operation_id: Some(op_id.to_string()),
                undoes_transition_id: Some(row.id.clone()),
                operation_id: None,
                operation_collected: false,
                agent: unmarked.clone(),
                actor: if remote.is_some() || unmarked.is_some() {
                    Actor::Agent
                } else {
                    Actor::Human
                }
                .as_str()
                .to_owned(),
                identity_id: None,
                identity_name: None,
                session_id: None,
                remote_token: remote.as_ref().map(|(id, _)| id.clone()),
                remote_token_name: remote
                    .as_ref()
                    .map(|(_, name)| name.clone())
                    .or_else(|| remote_name.clone()),
                created_at: Timestamp::now(),
                recorded_at: None,
            },
        });
    }
    let mut meta = OpMeta::new(OpKind::Undo, format!("{}{op_id}", oplog::REVERT_PREFIX));
    meta.workspace = workspace.map(|record| record.id.as_hex());
    meta.agent = agent.or_else(|| workspace.map(|record| record.owner.clone()));
    let recorded = record(state, repo, planned, meta).await?;
    Ok(UndoOutcome {
        op_id: recorded.op_id,
    })
}

/// `AbandonChange`'s check (R3.09): refuses a change in review.
pub fn check_abandon(
    state: &AppState,
    repo: RepoId,
    change: &str,
    divergent: bool,
) -> Result<(), LifecycleError> {
    if divergent {
        return Ok(());
    }
    let row = state.db.lifecycle_row(repo, change)?;
    let current = state_of_row(row.as_ref());
    if matches!(current, LifecycleState::Proposed | LifecycleState::Queued) {
        return Err(LifecycleError::InReview {
            change: change.to_owned(),
            state: current,
        });
    }
    Ok(())
}

/// Records `change` as landed on `trunk()` at `commit` (cause `observed`,
/// actor daemon), unless it is not recorded or already landed.
pub(crate) async fn record_observed(
    state: &AppState,
    repo: RepoId,
    change: &str,
    commit: &str,
    trunk_bookmark: &str,
) -> Result<Option<Recorded>, LifecycleError> {
    let create_lock = state.manager.repo_lock(repo);
    let _shared = create_lock.read().await;
    let lifecycle_lock = state.lifecycle.lock(repo);
    let _held = lifecycle_lock.lock().await;
    let Some(current) = state.db.lifecycle_row(repo, change)? else {
        return Ok(None);
    };
    let from = state_of_row(Some(&current));
    if from == LifecycleState::Landed {
        return Ok(None);
    }
    let row = TransitionRow {
        repo_id: repo,
        id: crate::identity::new_id("trn_"),
        seq: None,
        committed: false,
        change_id: change.to_owned(),
        commit_id: commit.to_owned(),
        from_state: from.as_str().to_owned(),
        to_state: LifecycleState::Landed.as_str().to_owned(),
        cause: TransitionCause::Observed.as_str().to_owned(),
        round: current.round,
        reason: None,
        superseded_by: None,
        destination: Some(trunk_bookmark.to_owned()),
        undoes_operation_id: None,
        undoes_transition_id: None,
        operation_id: None,
        operation_collected: false,
        agent: None,
        actor: Actor::Daemon.as_str().to_owned(),
        identity_id: None,
        identity_name: None,
        session_id: None,
        remote_token: None,
        remote_token_name: None,
        created_at: Timestamp::now(),
        recorded_at: None,
    };
    let meta = OpMeta::new(
        OpKind::Transition,
        oplog::transition_description("land", change),
    );
    let recorded = record(state, repo, vec![Planned { row, owner: None }], meta).await?;
    tracing::info!(change_id = change, op_id = %recorded.op_id, "recorded a landing seen on trunk()");
    Ok(Some(recorded))
}

/// At start, before the socket serves: commits every pending transition
/// whose operation exists, and drops the others. A repository that is not
/// open keeps its pending rows for the next start.
pub(crate) async fn resolve_pending(state: &AppState) {
    let pending = match state.db.pending_transitions() {
        Ok(pending) => pending,
        Err(error) => {
            tracing::error!(error = %ErrorChain(&error), "cannot read pending lifecycle transitions");
            return;
        }
    };
    let mut by_repo: HashMap<RepoId, Vec<TransitionRow>> = HashMap::new();
    for row in pending {
        by_repo.entry(row.repo_id).or_default().push(row);
    }
    for (repo, rows) in by_repo {
        let Some(handle) = state.repos.get(repo) else {
            continue;
        };
        let ids: BTreeSet<String> = rows.iter().map(|row| row.id.clone()).collect();
        let oldest = rows
            .iter()
            .map(|row| row.created_at)
            .min()
            .unwrap_or(Timestamp(0));
        let not_before = (std::time::UNIX_EPOCH
            + Duration::from_millis(u64::try_from(oldest.0).unwrap_or(0)))
        .checked_sub(PENDING_SLACK)
        .unwrap_or(std::time::UNIX_EPOCH);
        let found = handle
            .call_async(move |head| {
                async move {
                    let repo = head.refresh().await?;
                    Ok::<_, LifecycleError>(
                        jj_lifecycle::find_transitions(&repo, &ids, not_before).await?,
                    )
                }
                .boxed_local()
            })
            .await;
        let found = match found {
            Ok(Ok(found)) => found,
            Ok(Err(error)) => {
                tracing::warn!(%repo, error = %ErrorChain(&error), "cannot look for pending transitions' operations");
                continue;
            }
            Err(error) => {
                tracing::warn!(%repo, error = %ErrorChain(&error), "cannot look for pending transitions' operations");
                continue;
            }
        };
        let (mut committed, mut dropped) = (0, 0);
        for row in rows {
            let outcome = match found.get(&row.id) {
                Some(op) => {
                    let recorded_at = Timestamp::from(op.end);
                    state
                        .db
                        .lifecycle_row(repo, &row.change_id)
                        .and_then(|existing| {
                            // Committed late: its state change applies only
                            // if nothing moved the change since; the row is
                            // kept for the record either way.
                            let update = if row.from_state == state_of_row(existing.as_ref()).as_str() {
                                update_for(&row, existing.as_ref(), recorded_at, None)
                            } else {
                                tracing::warn!(%repo, transition = %row.id, "a transition committed late no longer applies; recorded, state unchanged");
                                LifecycleUpdate::Keep
                            };
                            state.db.commit_transition(
                                repo,
                                &row.id,
                                op.op_id.as_str(),
                                recorded_at,
                                &update,
                            )
                        })
                        .map(|_| committed += 1)
                }
                None => state
                    .db
                    .delete_pending_transition(repo, &row.id)
                    .map(|()| dropped += 1),
            };
            if let Err(error) = outcome {
                tracing::warn!(%repo, transition = %row.id, error = %ErrorChain(&error), "cannot resolve a pending transition");
            }
        }
        tracing::info!(%repo, committed, dropped, "resolved pending lifecycle transitions");
    }
}

/// GC's step after its jj step: maps the operation ids of `repo`'s
/// transitions through the rewrite (old to new), marking those it dropped.
pub(crate) fn map_operations(
    state: &AppState,
    repo: RepoId,
    rewritten: &[(OperationIdHex, OperationIdHex)],
) {
    let map: HashMap<String, String> = rewritten
        .iter()
        .map(|(old, new)| (old.to_string(), new.to_string()))
        .collect();
    match state.db.map_transition_operations(repo, &map) {
        Ok((mapped, collected)) if mapped + collected > 0 => {
            tracing::info!(%repo, mapped, collected, "mapped lifecycle transitions' operation ids");
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%repo, error = %ErrorChain(&error), "cannot map lifecycle transitions' operation ids");
        }
    }
}

/// A `ListProposals` page: rows matching `filter` after `after`.
pub fn proposals(
    state: &AppState,
    repo: RepoId,
    filter: &LifecycleFilter,
    after: Option<(Timestamp, String)>,
    limit: usize,
) -> Result<Vec<LifecycleRow>, LifecycleError> {
    Ok(state.db.lifecycle_page(repo, filter, after, limit)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    // R3.09 test 2: the transition table.
    #[test]
    fn the_table() {
        use LifecycleState as S;
        let allowed_pairs = [
            (S::Draft, S::Proposed),
            (S::Withdrawn, S::Proposed),
            (S::Declined, S::Proposed),
            (S::Proposed, S::Proposed),
            (S::Proposed, S::Withdrawn),
            (S::Queued, S::Withdrawn),
            (S::Proposed, S::Declined),
            (S::Queued, S::Declined),
            (S::Proposed, S::Superseded),
            (S::Queued, S::Superseded),
        ];
        for from in LifecycleState::ALL {
            for to in LifecycleState::ALL {
                assert_eq!(
                    allowed(from, to),
                    allowed_pairs.contains(&(from, to)),
                    "{from} -> {to}"
                );
            }
        }
        for state in [S::Landed, S::Superseded] {
            assert!(LifecycleState::ALL.iter().all(|to| !allowed(state, *to)));
        }
        assert!(reserved(S::Queued) && reserved(S::Landed) && reserved(S::Draft));
        assert!(!reserved(S::Proposed));
        assert_eq!(verb(S::Draft, S::Proposed), "propose");
        assert_eq!(verb(S::Queued, S::Proposed), "dequeue");
        assert_eq!(verb(S::Queued, S::Landed), "land");
    }

    #[test]
    fn reasons() {
        assert!(valid_reason("line one\n\tline two"));
        assert!(valid_reason(&"x".repeat(MAX_REASON_BYTES)));
        assert!(!valid_reason(&"x".repeat(MAX_REASON_BYTES + 1)));
        assert!(!valid_reason("bell \u{7}"));
        assert!(!valid_reason("csi \u{9b}"));
    }

    #[test]
    fn every_refusal_has_a_reason() {
        let errors = [
            LifecycleError::TargetRequired,
            LifecycleError::Reserved {
                to: "queued".into(),
            },
            LifecycleError::NotAllowed {
                change: "ab".into(),
                state: LifecycleState::Landed,
                to: LifecycleState::Proposed,
            },
            LifecycleError::ExpectedState {
                expected: LifecycleState::Draft,
                state: LifecycleState::Proposed,
            },
            LifecycleError::InvalidReason,
            LifecycleError::InvalidDestination {
                destination: "x".into(),
            },
            LifecycleError::NoDestination,
            LifecycleError::SupersededByRequired,
            LifecycleError::InvalidSupersededBy {
                superseded_by: "x".into(),
            },
            LifecycleError::Empty {
                change: "ab".into(),
            },
            LifecycleError::Conflicted {
                change: "ab".into(),
            },
            LifecycleError::Divergent {
                change: "ab".into(),
            },
            LifecycleError::InReview {
                change: "ab".into(),
                state: LifecycleState::Proposed,
            },
            LifecycleError::RemoteNotOwned {
                change: "ab".into(),
            },
            LifecycleError::RemoteHuman,
        ];
        for error in errors {
            let status = crate::error::status(error);
            let detail = reef_api::error::detail(&status).unwrap();
            assert!(!detail.reason.is_empty(), "{status:?}");
        }
    }
}
